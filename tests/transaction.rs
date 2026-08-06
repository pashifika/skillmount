//! Crash recovery and concurrency, proved with real processes.
//!
//! Every test here kills or stalls a real `asm` process at a real boundary and then runs a real
//! second invocation against whatever the first one left on disk. That is the only evidence that
//! matters for this behaviour: a unit test that hand-writes a `staged` journal proves recovery can
//! read that journal, not that the apply sequence ever produces it, and not that the filesystem
//! matches what the journal claims.
//!
//! Failure injection is compiled only into a debug build, which is what `cargo test` produces, so
//! these tests exercise the same code path a shipped binary runs minus the checkpoint itself.

use std::fs;
use std::io::{BufRead, BufReader, Read};
use std::path::{Path, PathBuf};
use std::process::{Child, ChildStderr, Command, Output, Stdio};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use skillmount::domain::LinkMode;
use skillmount::link::{LinkRequest, PlacementOutcome, platform_backend};

const ASM: &str = env!("CARGO_BIN_EXE_asm");
/// The reused `asm` child rejects Codex-native injected arguments with this usage status.
const FIXTURE_CHILD_STATUS: i32 = 64;
/// Real-process startup includes durable filesystem I/O on the selected native volume.
const HOLD_START_TIMEOUT: Duration = Duration::from_secs(10);

/// Every boundary from preliminary discovery through automatically recoverable transaction state.
///
/// Kept as literals rather than imported from the crate on purpose: the names are a contract
/// between the library and this suite, and a rename that silently updated both sides would turn a
/// crash test into a no-crash test without anyone noticing.
const BOUNDARIES: [&str; 13] = [
    "discovery-inspected",
    "journal-scan-complete",
    "journal-planned",
    "journal-applying",
    "action-intent",
    "temporary-created",
    "action-staged",
    "final-placed",
    "action-applied",
    "journal-active",
    "journal-cleaning",
    "entry-removed",
    "directory-removed",
];

/// Durable checkpoints reachable when the current Codex layout needs only one Skill link.
const CURRENT_LAYOUT_BOUNDARIES: [&str; 10] = [
    "journal-planned",
    "journal-applying",
    "action-intent",
    "temporary-created",
    "action-staged",
    "final-placed",
    "action-applied",
    "journal-active",
    "journal-cleaning",
    "entry-removed",
];

/// A project, a Skill source, and a private state root.
struct Fixture {
    root: PathBuf,
    project: PathBuf,
    sources: PathBuf,
    state: PathBuf,
}

impl Fixture {
    fn new(label: &str) -> Self {
        let nonce = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .expect("clock after epoch")
            .as_nanos();
        let root = std::env::temp_dir().join(format!(
            "skillmount-txn-{label}-{}-{nonce}",
            std::process::id()
        ));
        let fixture = Self {
            project: root.join("project"),
            sources: root.join("sources"),
            state: root.join("state"),
            root,
        };
        for path in [&fixture.project, &fixture.sources] {
            fs::create_dir_all(path).expect("fixture directory");
        }
        fixture
    }

    fn skill(&self, name: &str) -> &Self {
        let path = self.sources.join(name);
        fs::create_dir_all(&path).expect("skill directory");
        fs::write(
            path.join("SKILL.md"),
            format!("---\nname: {name}\ndescription: {name} description\n---\n"),
        )
        .expect("SKILL.md");
        self
    }

    fn install_current_codex_layout(&self) {
        let agents = self.project.join(".agents");
        let store = self.project.join(".codex/skills");
        fs::create_dir_all(&agents).expect("current .agents helper");
        fs::create_dir_all(store.join("rasen")).expect("project-owned rasen Skill");
        fs::write(
            store.join("rasen/SKILL.md"),
            "---\nname: rasen\ndescription: project fixture\n---\n",
        )
        .expect("project-owned Skill metadata");

        let backend = platform_backend();
        let staged = backend
            .create_directory_link(&LinkRequest {
                source: backend
                    .canonical_directory(&store)
                    .expect("canonical current store"),
                staged_path: agents.join(".skills.skillmount-fixture"),
                mode: LinkMode::Auto,
            })
            .expect("current discovery-link fixture");
        let outcome = backend
            .place_no_replace(&staged, &agents.join("skills"))
            .expect("place current discovery-link fixture");
        assert!(matches!(outcome, PlacementOutcome::Placed(_)));
    }

    /// Builds a session command with every redirection this suite depends on.
    ///
    /// The project root and the state root are both redirected. Without the first, a session would
    /// resolve the project from the harness's working directory and mount into this repository;
    /// without the second, it would write journals and locks into the developer's real
    /// application-support directory and contend with concurrent test runs.
    fn command(&self, agent: &str, extra: &[&str]) -> Command {
        self.command_for(agent, extra, &self.project, &self.root.join("home"))
    }

    fn command_for(&self, agent: &str, extra: &[&str], project: &Path, home: &Path) -> Command {
        fs::create_dir_all(home.join("codex-home")).expect("Codex home fixture");
        let mut command = Command::new(ASM);
        command
            .arg(agent)
            .arg("--skills-dir")
            .arg(&self.sources)
            .arg("--project-root")
            .arg(project)
            .arg("--cwd")
            .arg(project)
            .arg("--agent-bin")
            .arg(ASM)
            .args(extra)
            // Reuse the already-built cross-platform `asm` executable as a harmless agent child.
            // Each adapter validates the operator's own passthrough before anything is mounted, so
            // the shape has to be one that Agent accepts; the child then rejects whatever it
            // receives with usage 64.
            .arg("--")
            .args(fixture_child_args(agent))
            .env("HOME", home)
            .env("USERPROFILE", home)
            .env("SKILLMOUNT_TEST_CODEX_USER_HOME", home)
            .env("SKILLMOUNT_TEST_CODEX_MANAGED_CONFIG", "absent")
            .env("LOCALAPPDATA", home.join("AppData/Local"))
            .env("CODEX_HOME", home.join("codex-home"))
            .env("SKILLMOUNT_TEST_CODEX_VERSION", "codex-cli 0.146.0")
            .env("SKILLMOUNT_TEST_CLAUDE_VERSION", "2.1.220 (Claude Code)")
            .env("CLAUDE_CONFIG_DIR", home.join(".claude"))
            .env(
                "SKILLMOUNT_CLAUDE_MANAGED_SKILLS_DIR",
                home.join("claude-managed/skills"),
            )
            .env_remove("CLAUDE_CODE_SAFE_MODE")
            .env_remove("CLAUDE_CODE_SIMPLE")
            .env(
                "SKILLMOUNT_CODEX_ADMIN_SKILLS_DIR",
                home.join("admin-skills"),
            )
            .env("SKILLMOUNT_TEST_OMP_VERSION", "omp/17.2.9")
            // OMP resolves its roots from the environment, so the developer's real profile,
            // configuration overlay, and XDG bases must never reach a fixture.
            .env_remove("OMP_PROFILE")
            .env_remove("PI_PROFILE")
            .env_remove("PI_CONFIG_FILES")
            .env_remove("PI_CONFIG_DIR")
            .env_remove("PI_CODING_AGENT_DIR")
            .env_remove("XDG_DATA_HOME")
            .env("SKILLMOUNT_STATE_DIR", &self.state)
            // Contention must be reported rather than waited out, so a serialization test finishes
            // in milliseconds instead of the production timeout.
            .env("SKILLMOUNT_LOCK_WAIT_MS", "200")
            .current_dir(project);
        command
    }

    fn run(&self, agent: &str, extra: &[&str]) -> Output {
        self.command(agent, extra).output().expect("asm should run")
    }

    /// Runs a session that aborts at `boundary`.
    fn run_stopping_at(&self, agent: &str, boundary: &str, extra: &[&str]) -> Output {
        self.command(agent, extra)
            .env("SKILLMOUNT_STOP_AT", boundary)
            .output()
            .expect("asm should run")
    }

    fn cleanup(&self, all: bool) -> Output {
        self.cleanup_for(&self.project, all)
    }

    fn cleanup_for(&self, project: &Path, all: bool) -> Output {
        self.cleanup_command_for(project, all)
            .output()
            .expect("asm cleanup should run")
    }

    fn cleanup_command_for(&self, project: &Path, all: bool) -> Command {
        let mut command = Command::new(ASM);
        command
            .arg("cleanup")
            .env("SKILLMOUNT_STATE_DIR", &self.state)
            .current_dir(project);
        if all {
            command.arg("--all");
        } else {
            command.arg("--project-root").arg(project);
        }
        command
    }

    fn transactions(&self) -> PathBuf {
        self.state.join("transactions")
    }

    fn lock_files(&self) -> Vec<PathBuf> {
        let mut found = fs::read_dir(self.state.join("locks")).map_or_else(
            |_| Vec::new(),
            |entries| {
                entries
                    .filter_map(Result::ok)
                    .map(|entry| entry.path())
                    .filter(|path| {
                        path.extension()
                            .is_some_and(|extension| extension == "lock")
                    })
                    .collect()
            },
        );
        found.sort();
        found
    }

    /// Returns every journal file currently present.
    fn journals(&self) -> Vec<PathBuf> {
        let mut found = fs::read_dir(self.transactions()).map_or_else(
            |_| Vec::new(),
            |entries| {
                entries
                    .filter_map(Result::ok)
                    .map(|entry| entry.path())
                    .filter(|path| {
                        path.extension()
                            .is_some_and(|extension| extension == "journal")
                    })
                    .collect()
            },
        );
        found.sort();
        found
    }

    #[cfg(windows)]
    fn retired_journals(&self) -> Vec<PathBuf> {
        let mut found = fs::read_dir(self.transactions()).map_or_else(
            |_| Vec::new(),
            |entries| {
                entries
                    .filter_map(Result::ok)
                    .map(|entry| entry.path())
                    .filter(|path| {
                        path.file_name().is_some_and(|name| {
                            name.to_string_lossy().contains(".journal.removed-")
                        })
                    })
                    .collect()
            },
        );
        found.sort();
        found
    }

    /// Returns every entry beneath the project, so residue is visible in an assertion message.
    fn project_tree(&self) -> Vec<String> {
        let mut entries = Vec::new();
        collect(&self.project, &self.project, &mut entries);
        entries.sort();
        entries
    }

    fn session_tree(&self) -> Vec<String> {
        let root = self.state.join("sessions");
        let mut entries = Vec::new();
        collect(&root, &root, &mut entries);
        entries.sort();
        entries
    }
}

impl Drop for Fixture {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.root);
    }
}

fn collect(root: &Path, current: &Path, entries: &mut Vec<String>) {
    let Ok(metadata) = fs::symlink_metadata(current) else {
        return;
    };
    if let Ok(relative) = current.strip_prefix(root) {
        if !relative.as_os_str().is_empty() {
            entries.push(relative.display().to_string());
        }
    }
    if metadata.is_dir() && !metadata.file_type().is_symlink() {
        let Ok(children) = fs::read_dir(current) else {
            return;
        };
        for child in children.flatten() {
            collect(root, &child.path(), entries);
        }
    }
}

/// Returns whether a path exists without following it.
fn exists(path: &Path) -> bool {
    fs::symlink_metadata(path).is_ok()
}

/// Returns the Agent-native passthrough that reaches the reused `asm` child.
///
/// Codex and Claude both accept the static `exec` subcommand shape. OMP does not: it refuses any
/// recognized-or-not subcommand in the first non-flag position as something that does not start a
/// supervised foreground session, so it receives print mode plus a prompt instead. Both shapes end
/// at the same harmless child.
fn fixture_child_args(agent: &str) -> [&'static str; 2] {
    if agent == "omp" {
        ["--print", "fixture"]
    } else {
        ["exec", "fixture"]
    }
}

#[test]
fn scoped_cleanup_reconciles_one_stale_transaction_through_the_shared_path() {
    let fixture = Fixture::new("explicit-cleanup-scoped");
    fixture.skill("alpha");
    let stopped = fixture.run_stopping_at("codex", "journal-active", &[]);
    assert!(!stopped.status.success());
    assert_eq!(fixture.journals().len(), 1);
    assert!(
        fixture
            .project_tree()
            .iter()
            .any(|entry| entry.ends_with("alpha")),
        "the stopped transaction must leave its applied mount"
    );

    let cleaned = fixture.cleanup(false);

    assert!(
        cleaned.status.success(),
        "explicit cleanup should succeed: {}",
        String::from_utf8_lossy(&cleaned.stderr)
    );
    let rendered = String::from_utf8_lossy(&cleaned.stdout);
    assert!(rendered.contains("SkillMount cleanup"));
    assert!(rendered.contains("[RECOVERED]"));
    assert!(
        rendered.contains("entry removed") || rendered.contains("entries removed"),
        "cleanup reports its removal count: {rendered}"
    );
    assert!(
        fixture.project_tree().is_empty(),
        "all owned helpers are removed"
    );
    assert!(fixture.journals().is_empty());
    assert!(fixture.sources.join("alpha/SKILL.md").is_file());
}

#[test]
fn cleanup_reloads_a_journal_after_waiting_for_its_locks() {
    let fixture = Fixture::new("explicit-cleanup-refresh-after-lock");
    fixture.skill("alpha");
    let session_release = fixture.root.join("release-session");
    let cleanup_release = fixture.root.join("release-cleanup");

    let mut session = fixture
        .command("codex", &[])
        .env("SKILLMOUNT_HOLD_AT", "journal-planned")
        .env("SKILLMOUNT_HOLD_MS", "10000")
        .env("SKILLMOUNT_HOLD_UNTIL", &session_release)
        .env("SKILLMOUNT_STOP_AT", "final-placed@3")
        .stderr(Stdio::piped())
        .spawn()
        .expect("session that advances after cleanup scans");
    let mut session_stderr = wait_for_hold(&mut session, "journal-planned");

    let mut cleanup = fixture.cleanup_command_for(&fixture.project, false);
    cleanup
        .env("SKILLMOUNT_HOLD_AT", "journal-scan-complete")
        .env("SKILLMOUNT_HOLD_MS", "10000")
        .env("SKILLMOUNT_HOLD_UNTIL", &cleanup_release)
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    let mut cleanup = cleanup.spawn().expect("overlapping explicit cleanup");
    let mut cleanup_stderr = wait_for_hold(&mut cleanup, "journal-scan-complete");

    fs::write(&session_release, "continue").expect("release the applying session");
    let session_status = session.wait().expect("applying session stops");
    let mut session_diagnostics = String::new();
    session_stderr
        .read_to_string(&mut session_diagnostics)
        .expect("session diagnostics");
    assert!(!session_status.success(), "{session_diagnostics}");
    assert!(
        session_diagnostics.contains("stopping at final-placed occurrence 3"),
        "{session_diagnostics}"
    );
    let mounted = fixture.project.join(".agents/skills/alpha");
    assert!(exists(&mounted), "the advancing session placed its mount");

    fs::write(&cleanup_release, "continue").expect("release explicit cleanup");
    let cleanup_status = cleanup.wait().expect("cleanup finishes");
    let mut cleanup_output = String::new();
    cleanup
        .stdout
        .take()
        .expect("cleanup stdout")
        .read_to_string(&mut cleanup_output)
        .expect("cleanup report");
    let mut cleanup_diagnostics = String::new();
    cleanup_stderr
        .read_to_string(&mut cleanup_diagnostics)
        .expect("cleanup diagnostics");

    assert!(
        cleanup_status.success(),
        "cleanup must use the fresh staged journal: {cleanup_output}\n{cleanup_diagnostics}"
    );
    assert!(cleanup_output.contains("[RECOVERED]"), "{cleanup_output}");
    assert!(!exists(&mounted), "the freshly recorded mount is removed");
    assert!(
        fixture.journals().is_empty(),
        "no ownership journal is discarded while its mount remains"
    );
    assert!(fixture.sources.join("alpha/SKILL.md").is_file());
}

#[test]
fn cleanup_all_reports_an_active_transaction_without_touching_it() {
    let fixture = Fixture::new("explicit-cleanup-active");
    fixture.skill("alpha");
    let mut holder = fixture
        .command("codex", &[])
        .env("SKILLMOUNT_HOLD_AT", "journal-active")
        .env("SKILLMOUNT_HOLD_MS", "4000")
        .stderr(Stdio::piped())
        .spawn()
        .expect("active fixture session");
    let mut holder_stderr = wait_for_hold(&mut holder, "journal-active");
    let mounted = fixture.project.join(".agents/skills/alpha");
    assert!(exists(&mounted));

    let cleaned = fixture.cleanup(true);

    assert_eq!(cleaned.status.code(), Some(75));
    let rendered = String::from_utf8_lossy(&cleaned.stdout);
    assert!(rendered.contains("[ACTIVE]"), "{rendered}");
    assert!(
        rendered.contains("another SkillMount session holds"),
        "{rendered}"
    );
    assert!(exists(&mounted), "active mounts are left untouched");
    assert_eq!(fixture.journals().len(), 1);

    let status = holder.wait().expect("active fixture finishes");
    let mut diagnostics = String::new();
    holder_stderr
        .read_to_string(&mut diagnostics)
        .expect("holder diagnostics");
    assert_eq!(status.code(), Some(FIXTURE_CHILD_STATUS), "{diagnostics}");
}

#[test]
fn explicit_cleanup_releases_a_supervising_journal_after_operator_assertion() {
    let fixture = Fixture::new("explicit-cleanup-supervising");
    fixture.skill("alpha");
    let stopped = fixture.run_stopping_at("codex", "journal-supervising", &[]);
    assert!(!stopped.status.success());
    assert!(exists(&fixture.project.join(".agents/skills/alpha")));

    let cleaned = fixture.cleanup(false);

    assert!(
        cleaned.status.success(),
        "{}",
        String::from_utf8_lossy(&cleaned.stdout)
    );
    assert!(String::from_utf8_lossy(&cleaned.stdout).contains("[RECOVERED]"));
    assert!(fixture.project_tree().is_empty());
    assert!(fixture.journals().is_empty());
    assert!(fixture.sources.join("alpha/SKILL.md").is_file());
}

#[test]
fn scoped_cleanup_releases_kept_mounts_and_leaves_other_projects_out_of_scope() {
    let fixture = Fixture::new("explicit-cleanup-project-scope");
    fixture.skill("alpha");
    let second_project = fixture.root.join("second-project");
    let second_home = fixture.root.join("second-home");
    fs::create_dir(&second_project).expect("second project");

    let first = fixture.run("codex", &["--keep-mounts"]);
    assert_eq!(first.status.code(), Some(FIXTURE_CHILD_STATUS));
    let second = fixture
        .command_for("codex", &["--keep-mounts"], &second_project, &second_home)
        .output()
        .expect("second kept session");
    assert_eq!(second.status.code(), Some(FIXTURE_CHILD_STATUS));
    assert_eq!(fixture.journals().len(), 2);
    let second_mount = second_project.join(".agents/skills/alpha");
    assert!(exists(&second_mount));

    let scoped = fixture.cleanup(false);

    assert!(
        scoped.status.success(),
        "{}",
        String::from_utf8_lossy(&scoped.stdout)
    );
    let scoped_output = String::from_utf8_lossy(&scoped.stdout);
    assert!(scoped_output.contains("1 recovered"), "{scoped_output}");
    assert!(scoped_output.contains("1 out of scope"), "{scoped_output}");
    assert!(fixture.project_tree().is_empty());
    assert!(
        exists(&second_mount),
        "the other project is outside scoped cleanup"
    );
    assert_eq!(fixture.journals().len(), 1);

    let all = fixture.cleanup(true);
    assert!(
        all.status.success(),
        "{}",
        String::from_utf8_lossy(&all.stdout)
    );
    assert!(!exists(&second_mount));
    assert!(fixture.journals().is_empty());
    assert!(fixture.sources.join("alpha/SKILL.md").is_file());
}

#[test]
fn cleanup_reconciles_overlapping_kept_journals_and_their_shared_helpers_in_one_pass() {
    let fixture = Fixture::new("explicit-cleanup-overlapping-kept");
    fixture.skill("alpha");

    let first = fixture.run("codex", &["--keep-mounts"]);
    assert_eq!(first.status.code(), Some(FIXTURE_CHILD_STATUS));
    fixture.skill("beta");
    let second = fixture.run("codex", &["--keep-mounts"]);
    assert_eq!(second.status.code(), Some(FIXTURE_CHILD_STATUS));
    assert_eq!(fixture.journals().len(), 2);
    assert!(exists(&fixture.project.join(".agents/skills/alpha")));
    assert!(exists(&fixture.project.join(".agents/skills/beta")));

    let cleaned = fixture.cleanup(false);
    let output = String::from_utf8_lossy(&cleaned.stdout);

    assert!(cleaned.status.success(), "{output}");
    assert!(output.contains("2 recovered"), "{output}");
    assert!(!output.contains("[ACTIVE]"), "{output}");
    assert!(fixture.project_tree().is_empty());
    assert!(fixture.journals().is_empty());
    assert!(fixture.sources.join("alpha/SKILL.md").is_file());
    assert!(fixture.sources.join("beta/SKILL.md").is_file());
}

#[test]
fn cleanup_fails_closed_when_a_journal_disappears_after_its_candidate_scan() {
    let fixture = Fixture::new("explicit-cleanup-missing-journal");
    fixture.skill("alpha");
    let kept = fixture.run("codex", &["--keep-mounts"]);
    assert_eq!(kept.status.code(), Some(FIXTURE_CHILD_STATUS));
    let mount = fixture.project.join(".agents/skills/alpha");
    assert!(exists(&mount));
    let journal = fixture.journals().into_iter().next().expect("kept journal");
    let release = fixture.root.join("release-stale-cleanup-scan");

    let mut stale = fixture.cleanup_command_for(&fixture.project, false);
    stale
        .env("SKILLMOUNT_HOLD_AT", "journal-scan-complete")
        .env("SKILLMOUNT_HOLD_MS", "10000")
        .env("SKILLMOUNT_HOLD_UNTIL", &release)
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    let mut stale = stale.spawn().expect("cleanup with a stale candidate scan");
    let mut stale_stderr = wait_for_hold(&mut stale, "journal-scan-complete");

    fs::remove_file(&journal).expect("simulate journal-only external removal");
    fs::write(&release, "continue").expect("release stale cleanup");

    let stale_status = stale.wait().expect("stale cleanup finishes");
    let mut stale_output = String::new();
    stale
        .stdout
        .take()
        .expect("stale cleanup stdout")
        .read_to_string(&mut stale_output)
        .expect("stale cleanup report");
    let mut stale_diagnostics = String::new();
    stale_stderr
        .read_to_string(&mut stale_diagnostics)
        .expect("stale cleanup diagnostics");

    assert_eq!(
        stale_status.code(),
        Some(75),
        "{stale_output}\n{stale_diagnostics}"
    );
    assert!(stale_output.contains("[CORRUPT]"), "{stale_output}");
    assert!(stale_output.contains("disappeared"), "{stale_output}");
    assert!(
        exists(&mount),
        "an unjournalled residual mount is never removed"
    );
    assert!(fixture.journals().is_empty());
}

#[test]
fn cleanup_reports_corrupt_state_and_a_neighboring_lock_failure_together() {
    let fixture = Fixture::new("explicit-cleanup-corrupt-and-lock-failure");
    fixture.skill("alpha");

    let first = fixture.run("codex", &["--keep-mounts"]);
    assert_eq!(first.status.code(), Some(FIXTURE_CHILD_STATUS));
    let first_journal = fixture
        .journals()
        .into_iter()
        .next()
        .expect("first kept journal");
    let first_locks = fixture.lock_files();

    let second_project = fixture.root.join("second-project");
    let second_home = fixture.root.join("second-home");
    fs::create_dir(&second_project).expect("second project");
    let second = fixture
        .command_for("codex", &["--keep-mounts"], &second_project, &second_home)
        .output()
        .expect("second kept session");
    assert_eq!(second.status.code(), Some(FIXTURE_CHILD_STATUS));
    let second_journal = fixture
        .journals()
        .into_iter()
        .find(|journal| journal != &first_journal)
        .expect("second kept journal");
    let broken_lock = fixture
        .lock_files()
        .into_iter()
        .find(|path| !first_locks.contains(path))
        .expect("second project has a distinct lock file");
    let first_mount = fixture.project.join(".agents/skills/alpha");
    let second_mount = second_project.join(".agents/skills/alpha");
    assert!(exists(&first_mount));
    assert!(exists(&second_mount));

    let release = fixture.root.join("release-mixed-cleanup-failures");
    let mut cleanup = fixture.cleanup_command_for(&fixture.project, true);
    cleanup
        .env("SKILLMOUNT_HOLD_AT", "journal-scan-complete")
        .env("SKILLMOUNT_HOLD_MS", "10000")
        .env("SKILLMOUNT_HOLD_UNTIL", &release)
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    let mut cleanup = cleanup.spawn().expect("cleanup with two late failures");
    let mut cleanup_stderr = wait_for_hold(&mut cleanup, "journal-scan-complete");

    fs::remove_file(&first_journal).expect("simulate journal-only external removal");
    fs::remove_file(&broken_lock).expect("remove the neighboring lock file");
    fs::create_dir(&broken_lock).expect("replace the neighboring lock with a directory");
    fs::write(&release, "continue").expect("release mixed-failure cleanup");

    let cleanup_status = cleanup.wait().expect("mixed-failure cleanup finishes");
    let mut cleanup_output = String::new();
    cleanup
        .stdout
        .take()
        .expect("cleanup stdout")
        .read_to_string(&mut cleanup_output)
        .expect("cleanup report");
    let mut cleanup_diagnostics = String::new();
    cleanup_stderr
        .read_to_string(&mut cleanup_diagnostics)
        .expect("cleanup diagnostics");

    assert_eq!(
        cleanup_status.code(),
        Some(73),
        "{cleanup_output}\n{cleanup_diagnostics}"
    );
    assert!(cleanup_output.contains("[CORRUPT]"), "{cleanup_output}");
    assert!(cleanup_output.contains("[FAILED]"), "{cleanup_output}");
    assert!(cleanup_output.contains("disappeared"), "{cleanup_output}");
    assert!(exists(&first_mount), "the unjournalled mount is retained");
    assert!(exists(&second_mount), "the lock-failed mount is retained");
    assert_eq!(fixture.journals(), vec![second_journal]);
}

#[test]
fn cleanup_reports_completed_mutations_before_a_later_lock_io_failure() {
    let fixture = Fixture::new("explicit-cleanup-partial-report");
    fixture.skill("alpha");
    let first_release = fixture.root.join("release-first-stale-session");
    let mut first = fixture
        .command("codex", &[])
        .env("SKILLMOUNT_HOLD_AT", "journal-active")
        .env("SKILLMOUNT_HOLD_MS", "10000")
        .env("SKILLMOUNT_HOLD_UNTIL", &first_release)
        .env("SKILLMOUNT_STOP_AT", "journal-active")
        .stderr(Stdio::piped())
        .spawn()
        .expect("first stale session");
    let mut first_stderr = wait_for_hold(&mut first, "journal-active");
    let first_journal = fixture
        .journals()
        .into_iter()
        .next()
        .expect("first stale journal");
    let first_locks = fixture.lock_files();

    let second_project = fixture.root.join("second-project");
    let second_home = fixture.root.join("second-home");
    fs::create_dir(&second_project).expect("second project");
    let second = fixture
        .command_for("codex", &[], &second_project, &second_home)
        .env("SKILLMOUNT_STOP_AT", "journal-active")
        .output()
        .expect("second stale session");
    assert!(!second.status.success());
    fs::write(&first_release, "continue").expect("release first stale session");
    let first_status = first.wait().expect("first stale session stops");
    let mut first_diagnostics = String::new();
    first_stderr
        .read_to_string(&mut first_diagnostics)
        .expect("first stale-session diagnostics");
    assert!(!first_status.success(), "{first_diagnostics}");
    let journals = fixture.journals();
    assert_eq!(journals.len(), 2);
    assert_eq!(
        journals[0], first_journal,
        "the first transaction must be reconciled before the injected later failure"
    );

    let broken_lock = fixture
        .lock_files()
        .into_iter()
        .find(|path| !first_locks.contains(path))
        .expect("the second project has a distinct lock file");
    fs::remove_file(&broken_lock).expect("remove the second transaction lock file");
    fs::create_dir(&broken_lock).expect("replace the lock file with a directory");

    let cleaned = fixture.cleanup(true);
    let rendered = String::from_utf8_lossy(&cleaned.stdout);

    assert_eq!(cleaned.status.code(), Some(73), "{rendered}");
    assert!(rendered.contains("[RECOVERED]"), "{rendered}");
    assert!(rendered.contains("[FAILED]"), "{rendered}");
    assert!(
        fixture.project_tree().is_empty(),
        "the first cleanup mutation is both completed and reported"
    );
    assert!(
        exists(&second_project.join(".agents/skills/alpha")),
        "the journal whose lock could not be opened remains untouched"
    );
    assert_eq!(fixture.journals().len(), 1);
    assert!(fixture.sources.join("alpha/SKILL.md").is_file());
}

#[test]
fn cleanup_retains_a_replaced_mount_and_its_journal() {
    let fixture = Fixture::new("explicit-cleanup-replaced");
    fixture.skill("alpha");
    fixture.run_stopping_at("codex", "journal-active", &[]);
    let mounted = fixture.project.join(".agents/skills/alpha");
    remove_directory_link(&mounted);
    fs::create_dir(&mounted).expect("replacement directory");
    fs::write(mounted.join("operator-owned"), "mine").expect("replacement content");

    let cleaned = fixture.cleanup(false);

    assert_eq!(cleaned.status.code(), Some(73));
    let rendered = String::from_utf8_lossy(&cleaned.stdout);
    assert!(rendered.contains("retained"), "{rendered}");
    assert!(
        rendered.contains("regular directory replaced"),
        "{rendered}"
    );
    assert!(mounted.join("operator-owned").is_file());
    assert_eq!(fixture.journals().len(), 1);
    assert!(fixture.sources.join("alpha/SKILL.md").is_file());
}

#[test]
fn cleanup_accepts_an_already_missing_mount_but_never_touches_the_source() {
    let fixture = Fixture::new("explicit-cleanup-missing");
    fixture.skill("alpha");
    fixture.run_stopping_at("codex", "journal-active", &[]);
    let mounted = fixture.project.join(".agents/skills/alpha");
    remove_directory_link(&mounted);

    let cleaned = fixture.cleanup(false);

    assert!(
        cleaned.status.success(),
        "{}",
        String::from_utf8_lossy(&cleaned.stdout)
    );
    assert!(fixture.project_tree().is_empty());
    assert!(fixture.journals().is_empty());
    assert!(fixture.sources.join("alpha/SKILL.md").is_file());
}

#[test]
fn cleanup_uses_lock_state_not_a_reused_pid_in_holder_text() {
    let fixture = Fixture::new("explicit-cleanup-pid-reuse");
    fixture.skill("alpha");
    fixture.run_stopping_at("codex", "journal-active", &[]);
    let locks = fixture.state.join("locks");
    for entry in fs::read_dir(&locks).expect("stopped transaction lock files") {
        let path = entry.expect("lock entry").path();
        if path
            .extension()
            .is_some_and(|extension| extension == "lock")
        {
            let digest = path.file_stem().expect("lock digest").to_string_lossy();
            fs::write(
                locks.join(format!("{digest}.owner")),
                format!("transaction=reused pid={}\n", std::process::id()),
            )
            .expect("stale holder text");
        }
    }

    let cleaned = fixture.cleanup(false);

    assert!(
        cleaned.status.success(),
        "PID-looking text is not liveness evidence: {}",
        String::from_utf8_lossy(&cleaned.stdout)
    );
    assert!(fixture.project_tree().is_empty());
    assert!(fixture.journals().is_empty());
}

#[test]
fn cleanup_retains_a_non_empty_owned_helper_after_removing_its_mount() {
    let fixture = Fixture::new("explicit-cleanup-non-empty-helper");
    fixture.skill("alpha");
    fixture.run_stopping_at("codex", "journal-active", &[]);
    let operator_file = fixture.project.join(".agents/skills/operator-notes.txt");
    fs::write(&operator_file, "mine").expect("operator helper content");

    let cleaned = fixture.cleanup(false);

    assert_eq!(cleaned.status.code(), Some(73));
    let rendered = String::from_utf8_lossy(&cleaned.stdout);
    assert!(rendered.contains("holds entries"), "{rendered}");
    assert!(operator_file.is_file());
    assert!(!exists(&fixture.project.join(".agents/skills/alpha")));
    assert_eq!(fixture.journals().len(), 1);
    assert!(fixture.sources.join("alpha/SKILL.md").is_file());
}

#[test]
fn corrupt_journal_blocks_explicit_cleanup_of_a_healthy_neighbor() {
    let fixture = Fixture::new("explicit-cleanup-corrupt");
    fixture.skill("alpha");
    fixture.run_stopping_at("codex", "journal-active", &[]);
    let before = fixture.project_tree();
    let corrupt = fixture.transactions().join("ffff-future.journal");
    fs::write(&corrupt, "skillmount-journal 99 unix deadbeef\n").expect("future journal");

    let cleaned = fixture.cleanup(true);

    assert_eq!(cleaned.status.code(), Some(75));
    let rendered = String::from_utf8_lossy(&cleaned.stdout);
    assert!(rendered.contains("[CORRUPT]"), "{rendered}");
    assert!(
        rendered.contains("no valid neighbor was cleaned"),
        "{rendered}"
    );
    assert_eq!(fixture.project_tree(), before);
    assert_eq!(fixture.journals().len(), 2);
    assert!(corrupt.is_file());
}

#[test]
fn cleanup_all_is_bounded_to_journals_and_ignores_similarly_named_entries() {
    let fixture = Fixture::new("explicit-cleanup-boundary");
    fixture.skill("alpha");
    fixture.run_stopping_at("codex", "journal-active", &[]);
    let unrelated = fixture.state.join("transactions/looks-like-skillmount.txt");
    fs::write(&unrelated, "operator data").expect("unrelated state file");
    let arbitrary = fixture.root.join(".skillmount-not-owned");
    fs::create_dir(&arbitrary).expect("similarly named arbitrary directory");
    fs::write(arbitrary.join("sentinel"), "mine").expect("arbitrary sentinel");

    let cleaned = fixture.cleanup(true);

    assert!(
        cleaned.status.success(),
        "{}",
        String::from_utf8_lossy(&cleaned.stdout)
    );
    assert_eq!(fs::read_to_string(&unrelated).unwrap(), "operator data");
    assert_eq!(
        fs::read_to_string(arbitrary.join("sentinel")).unwrap(),
        "mine"
    );
    assert!(fixture.sources.join("alpha/SKILL.md").is_file());
}

fn remove_directory_link(path: &Path) {
    let result = if cfg!(windows) {
        fs::remove_dir(path)
    } else {
        fs::remove_file(path)
    };
    result.unwrap_or_else(|error| panic!("removing link {} failed: {error}", path.display()));
}

#[test]
fn every_named_boundary_actually_stops_a_session() {
    for boundary in BOUNDARIES {
        let fixture = Fixture::new(&format!("boundary-{boundary}"));
        fixture.skill("alpha");

        let output = fixture.run_stopping_at("codex", boundary, &[]);

        assert!(
            !output.status.success(),
            "{boundary} did not stop the session"
        );
        assert!(
            String::from_utf8_lossy(&output.stderr)
                .contains(&format!("stopping at {boundary} occurrence")),
            "{boundary} was never reached, so nothing about it is under test: {}",
            String::from_utf8_lossy(&output.stderr)
        );
    }
}

#[cfg(windows)]
#[test]
fn terminal_journal_retirement_can_stop_after_the_write_through_rename() {
    let fixture = Fixture::new("journal-retired");
    fixture.skill("alpha");

    let stopped = fixture.run_stopping_at("codex", "journal-retired", &[]);

    assert!(!stopped.status.success());
    let stderr = String::from_utf8_lossy(&stopped.stderr);
    assert!(
        stderr.contains("stopping at journal-retired occurrence 1"),
        "the session did not reach terminal write-through retirement: {stderr}"
    );
    assert!(
        fixture.journals().is_empty(),
        "the terminal journal must already be outside the scanner namespace"
    );
    let retired = fixture.retired_journals();
    assert_eq!(
        retired.len(),
        1,
        "forced termination must preserve the retired tombstone: {retired:?}"
    );
    assert!(fixture.sources.join("alpha/SKILL.md").is_file());
    assert!(
        !exists(&fixture.project.join(".agents/skills/alpha")),
        "terminal retirement may occur only after the owned mount is gone"
    );

    let later = fixture.run("codex", &[]);

    assert_eq!(
        later.status.code(),
        Some(FIXTURE_CHILD_STATUS),
        "a retired tombstone must not re-enter journal recovery: {}",
        String::from_utf8_lossy(&later.stderr)
    );
    assert!(fixture.journals().is_empty());
    assert!(
        retired[0].is_file(),
        "the inert evidence remains inspectable"
    );
    assert!(fixture.sources.join("alpha/SKILL.md").is_file());
}

#[test]
fn a_second_invocation_recovers_every_boundary_and_leaves_the_project_clean() {
    for boundary in BOUNDARIES {
        let fixture = Fixture::new(&format!("recover-{boundary}"));
        fixture.skill("alpha");

        let killed = fixture.run_stopping_at("codex", boundary, &[]);
        assert!(!killed.status.success(), "{boundary} must stop the session");

        // A real second invocation, against whatever the first one left behind.
        let recovered = fixture.run("codex", &[]);

        assert_eq!(
            recovered.status.code(),
            Some(FIXTURE_CHILD_STATUS),
            "the recovering session must launch and clean up after its fixture child at {boundary}: {}",
            String::from_utf8_lossy(&recovered.stderr)
        );
        assert!(
            exists(&fixture.sources.join("alpha/SKILL.md")),
            "no boundary may ever cost a Skill source"
        );

        // One boundary genuinely cannot be reconciled automatically. `temporary-created` stops
        // between creating the temporary entry and recording its identity, so the entry exists and
        // nothing proves which transaction made it. Retaining it is the specified outcome — residue
        // over deleting an unowned entry — and both the entry and the journal describing it stay,
        // reported, so an operator can finish the job.
        let residue = fixture.project_tree();
        if boundary == "temporary-created" {
            assert!(
                residue.iter().any(|entry| entry.contains(".skillmount-")),
                "the unprovable staged entry must still be present at {boundary}: {residue:?}"
            );
            assert!(
                String::from_utf8_lossy(&recovered.stderr).contains("retained"),
                "retained residue must be reported: {}",
                String::from_utf8_lossy(&recovered.stderr)
            );
            assert_eq!(
                fixture.journals().len(),
                1,
                "the journal describing unreconciled residue is kept: {:?}",
                fixture.journals()
            );
        } else {
            assert!(
                residue.is_empty(),
                "a session stopped at {boundary} must leave the project clean once recovered: \
                 {residue:?}"
            );
            assert!(
                fixture.journals().is_empty(),
                "recovery plus a completed session must leave no journal at {boundary}: {:?}",
                fixture.journals()
            );
        }
    }
}

#[test]
fn a_second_claude_invocation_recovers_every_staging_boundary() {
    for boundary in BOUNDARIES {
        let fixture = Fixture::new(&format!("recover-claude-{boundary}"));
        fixture.skill("alpha");

        let stopped = fixture.run_stopping_at("claude", boundary, &[]);
        assert!(
            !stopped.status.success(),
            "{boundary} must stop the session"
        );
        let recovered = fixture.run("claude", &[]);
        let stderr = String::from_utf8_lossy(&recovered.stderr);

        assert_eq!(
            recovered.status.code(),
            Some(FIXTURE_CHILD_STATUS),
            "Claude recovery must reach its fixture child at {boundary}: {stderr}"
        );
        assert!(
            fixture.project_tree().is_empty(),
            "Claude recovery must not touch the project at {boundary}: {:?}",
            fixture.project_tree()
        );
        assert!(fixture.sources.join("alpha/SKILL.md").is_file());

        if boundary == "temporary-created" {
            assert!(
                fixture
                    .session_tree()
                    .iter()
                    .any(|entry| entry.contains(".skillmount-")),
                "unrecorded staging identity is retained at {boundary}: {:?}",
                fixture.session_tree()
            );
            assert!(stderr.contains("retained"), "{stderr}");
            assert_eq!(fixture.journals().len(), 1);
        } else {
            assert!(
                fixture.session_tree().is_empty(),
                "owned Claude staging residue remains at {boundary}: {:?}",
                fixture.session_tree()
            );
            assert!(
                fixture.journals().is_empty(),
                "completed Claude recovery retains a journal at {boundary}: {:?}",
                fixture.journals()
            );
        }
    }
}

#[test]
fn current_codex_layout_survives_every_reachable_recovery_boundary() {
    for boundary in CURRENT_LAYOUT_BOUNDARIES {
        let fixture = Fixture::new(&format!("recover-current-{boundary}"));
        fixture.skill("alpha");
        fixture.install_current_codex_layout();

        let discovery = fixture.project.join(".agents/skills");
        let discovery_before = platform_backend()
            .inspect_no_follow(&discovery)
            .expect("inspect current discovery link");
        let baseline = fixture.project_tree();
        let rasen_body = fs::read_to_string(fixture.project.join(".codex/skills/rasen/SKILL.md"))
            .expect("read project-owned Skill");

        let killed = fixture.run_stopping_at("codex", boundary, &[]);
        assert!(!killed.status.success(), "{boundary} must stop the session");

        let recovered = fixture.run("codex", &[]);
        assert_eq!(
            recovered.status.code(),
            Some(FIXTURE_CHILD_STATUS),
            "the current layout must recover and launch at {boundary}: {}",
            String::from_utf8_lossy(&recovered.stderr)
        );
        assert_eq!(
            platform_backend()
                .inspect_no_follow(&discovery)
                .expect("reinspect current discovery link"),
            discovery_before,
            "the pre-existing discovery entry changed at {boundary}"
        );
        assert_eq!(
            fs::read_to_string(fixture.project.join(".codex/skills/rasen/SKILL.md"))
                .expect("project-owned Skill survives"),
            rasen_body
        );

        let recovered_tree = fixture.project_tree();
        if boundary == "temporary-created" {
            assert!(
                baseline.iter().all(|entry| recovered_tree.contains(entry)),
                "the current layout must remain a subset of retained residue at {boundary}: {recovered_tree:?}"
            );
            assert!(
                recovered_tree
                    .iter()
                    .any(|entry| entry.contains(".skillmount-")),
                "the unrecorded staged entry must be retained at {boundary}: {recovered_tree:?}"
            );
            assert_eq!(fixture.journals().len(), 1);
        } else {
            assert_eq!(
                recovered_tree, baseline,
                "only the pre-existing current layout may remain after {boundary}"
            );
            assert!(fixture.journals().is_empty());
        }
    }
}

#[test]
fn recovery_removes_a_staged_entry_that_was_never_placed() {
    let fixture = Fixture::new("staged-only");
    fixture.skill("alpha");

    // The second `action-staged` occurrence is the store directory: action 1 is `.agents`, action
    // 2 is `.agents/skills`. Stopping there leaves a staged sibling whose identity is durable.
    fixture.run_stopping_at("codex", "action-staged@2", &[]);
    let staged_before = fixture
        .project_tree()
        .into_iter()
        .filter(|entry| entry.contains(".skillmount-"))
        .collect::<Vec<_>>();

    let recovered = fixture.run("codex", &[]);

    assert_eq!(
        staged_before.len(),
        1,
        "the fixture must actually leave a staged entry: {staged_before:?}"
    );
    assert_eq!(recovered.status.code(), Some(FIXTURE_CHILD_STATUS));
    assert!(
        !fixture
            .project_tree()
            .iter()
            .any(|entry| entry.contains(".skillmount-")),
        "a staged entry with a durable identity must be removable: {:?}",
        fixture.project_tree()
    );
}

#[test]
fn recovery_removes_an_entry_placed_before_its_applied_record() {
    let fixture = Fixture::new("placed-not-applied");
    fixture.skill("alpha");

    // Stopping after the last placement leaves the journal saying `staged` while the entry already
    // occupies its final path. Recovery has to inspect both paths and remove the matching one.
    fixture.run_stopping_at("codex", "final-placed@3", &[]);
    let mounted = fixture.project.join(".agents/skills/alpha");
    assert!(
        exists(&mounted),
        "the fixture must leave the mount in place: {:?}",
        fixture.project_tree()
    );

    let recovered = fixture.run("codex", &[]);

    assert_eq!(recovered.status.code(), Some(FIXTURE_CHILD_STATUS));
    assert!(
        fixture.project_tree().is_empty(),
        "recovery must reconcile the placed-but-unrecorded entry: {:?}",
        fixture.project_tree()
    );
}

#[test]
fn auto_link_intent_with_an_unrecorded_kind_retains_every_existing_candidate() {
    let fixture = Fixture::new("auto-link-undecided");
    fixture.skill("bootstrap");

    // Establish the normal pre-existing Codex layout without relying on a test-only link helper.
    // The kept bootstrap transaction owns that layout and remains terminal throughout this test.
    let bootstrap = fixture.run("codex", &["--keep-mounts"]);
    assert_eq!(bootstrap.status.code(), Some(FIXTURE_CHILD_STATUS));
    fs::remove_file(fixture.sources.join("bootstrap/SKILL.md"))
        .expect("bootstrap is no longer selected");
    fixture.skill("alpha");

    // With all helper entries already present, alpha is the first and only owned action. `auto`
    // has created its staged link here, but the chosen symlink/junction kind and identity have not
    // reached the journal yet.
    let stopped = fixture.run_stopping_at("codex", "temporary-created@1", &[]);
    assert!(!stopped.status.success());
    let store = fixture.project.join(".agents/skills");
    let staged_before = fs::read_dir(&store)
        .expect("pre-existing store")
        .filter_map(Result::ok)
        .map(|entry| entry.path())
        .filter(|path| {
            path.file_name()
                .and_then(|name| name.to_str())
                .is_some_and(|name| name.starts_with(".skillmount-"))
        })
        .collect::<Vec<_>>();
    assert_eq!(staged_before.len(), 1, "the auto-link window must exist");

    let recovered = fixture.run("codex", &[]);
    let stderr = String::from_utf8_lossy(&recovered.stderr);

    assert_eq!(
        recovered.status.code(),
        Some(FIXTURE_CHILD_STATUS),
        "{stderr}"
    );
    assert!(stderr.contains("concrete kind and identity"), "{stderr}");
    assert!(
        staged_before.iter().all(|path| exists(path)),
        "unproven auto-link candidates must be retained"
    );
    assert_eq!(
        fixture.journals().len(),
        2,
        "the terminal bootstrap journal and the unreconciled auto-link journal remain"
    );
}

#[test]
fn recovery_never_removes_an_entry_a_user_replaced_after_the_crash() {
    let fixture = Fixture::new("replaced-after-crash");
    fixture.skill("alpha");
    fixture.run_stopping_at("codex", "journal-active", &[]);

    // The operator replaces the mount with work of their own before the next session runs.
    let mounted = fixture.project.join(".agents/skills/alpha");
    if cfg!(windows) {
        fs::remove_dir(&mounted)
    } else {
        fs::remove_file(&mounted)
    }
    .expect("the crashed session left a link here");
    fs::create_dir_all(mounted.join("their-own-work")).expect("replacement");

    let recovered = fixture.run("codex", &[]);

    assert!(
        exists(&mounted.join("their-own-work")),
        "recovery must never delete something it cannot prove it created"
    );
    assert!(
        String::from_utf8_lossy(&recovered.stderr).contains("retained"),
        "the mismatch must be reported: {}",
        String::from_utf8_lossy(&recovered.stderr)
    );
}

#[test]
fn claude_recovery_never_removes_a_replaced_staging_entry() {
    let fixture = Fixture::new("claude-replaced-after-crash");
    fixture.skill("alpha");
    fixture.run_stopping_at("claude", "journal-active", &[]);

    let sessions = fs::read_dir(fixture.state.join("sessions"))
        .expect("crashed Claude session root")
        .filter_map(Result::ok)
        .map(|entry| entry.path())
        .collect::<Vec<_>>();
    assert_eq!(sessions.len(), 1, "the stopped run owns one staging root");
    let mounted = sessions[0].join("root/.claude/skills/alpha");
    if cfg!(windows) {
        fs::remove_dir(&mounted)
    } else {
        fs::remove_file(&mounted)
    }
    .expect("replace the crashed session's mount");
    fs::create_dir(&mounted).expect("replacement Skill directory");
    fs::write(mounted.join("operator-owned.txt"), "mine\n").expect("replacement content");

    let recovered = fixture.run("claude", &[]);
    let stderr = String::from_utf8_lossy(&recovered.stderr);

    assert!(mounted.join("operator-owned.txt").is_file());
    assert!(stderr.contains("retained"), "{stderr}");
    assert!(
        !fixture.journals().is_empty(),
        "mismatched ownership evidence must remain durable"
    );
    assert!(fixture.project_tree().is_empty());
}

#[test]
fn claude_recovery_retains_a_replaced_session_root() {
    let fixture = Fixture::new("claude-replaced-session-root");
    fixture.skill("alpha");
    fixture.run_stopping_at("claude", "journal-active", &[]);

    let sessions = fs::read_dir(fixture.state.join("sessions"))
        .expect("crashed Claude session root")
        .filter_map(Result::ok)
        .map(|entry| entry.path())
        .collect::<Vec<_>>();
    assert_eq!(sessions.len(), 1);
    let session = &sessions[0];
    let moved = fixture.root.join("operator-moved-session");
    fs::rename(session, &moved).expect("move the owned root away from its recorded path");
    fs::create_dir(session).expect("create a replacement session root");
    fs::write(session.join("operator-owned.txt"), "mine\n").expect("replacement content");

    let recovered = fixture.run("claude", &[]);
    let stderr = String::from_utf8_lossy(&recovered.stderr);

    assert!(session.join("operator-owned.txt").is_file());
    assert!(moved.join("root/.claude/skills/alpha").is_dir());
    assert!(stderr.contains("retained"), "{stderr}");
    assert!(
        !fixture.journals().is_empty(),
        "root identity mismatch must retain its ownership journal"
    );
    assert!(fixture.project_tree().is_empty());
}

#[test]
fn no_recover_fails_closed_and_changes_nothing() {
    let fixture = Fixture::new("no-recover");
    fixture.skill("alpha");
    fixture.run_stopping_at("codex", "journal-active", &[]);
    let before = fixture.project_tree();
    let journals_before = fixture.journals();

    let refused = fixture.run("codex", &["--no-recover"]);

    assert_eq!(
        refused.status.code(),
        Some(75),
        "unreconciled state under --no-recover is a temporary failure: {}",
        String::from_utf8_lossy(&refused.stderr)
    );
    let stderr = String::from_utf8_lossy(&refused.stderr);
    assert!(stderr.contains("--no-recover"), "{stderr}");
    assert!(stderr.contains("nothing was changed"), "{stderr}");
    assert_eq!(fixture.project_tree(), before);
    assert_eq!(fixture.journals(), journals_before);
}

#[test]
fn a_session_stopped_after_supervision_intent_is_quarantined_not_recovered() {
    let fixture = Fixture::new("supervising-stop");
    fixture.skill("alpha");

    let stopped = fixture.run_stopping_at("codex", "journal-supervising", &[]);
    let mounted = fixture.project.join(".agents/skills/alpha");

    assert!(!stopped.status.success());
    assert!(exists(&mounted), "the stopped session left an active mount");
    assert_eq!(fixture.journals().len(), 1);

    let reported = fixture.run("codex", &["--dry-run"]);
    let report = String::from_utf8_lossy(&reported.stdout);
    assert!(reported.status.success(), "{report}");
    assert!(report.contains("WOULD QUARANTINE"), "{report}");
    assert!(
        exists(&mounted),
        "read-only quarantine reporting must not remove the mount"
    );

    let refused = fixture.run("codex", &[]);
    let stderr = String::from_utf8_lossy(&refused.stderr);

    assert_eq!(refused.status.code(), Some(75), "{stderr}");
    assert!(
        stderr.contains("process-domain death was never proved"),
        "{stderr}"
    );
    assert!(stderr.contains("recovery[0] argv[1] = cleanup"), "{stderr}");
    assert!(
        stderr.contains("the quarantined mounts were not changed"),
        "{stderr}"
    );
    assert!(exists(&mounted), "quarantine must not remove the mount");
    assert_eq!(fixture.journals().len(), 1, "ownership evidence remains");
}

#[test]
fn cross_project_quarantine_names_the_recorded_project_cleanup_argv() {
    let fixture = Fixture::new("cross-project-quarantine-guidance");
    fixture.skill("alpha");
    let stopped = fixture.run_stopping_at("codex", "journal-supervising", &[]);
    assert!(!stopped.status.success());

    let second_project = fixture.root.join("second-project");
    let second_home = fixture.root.join("second-home");
    fs::create_dir(&second_project).expect("second project");
    let refused = fixture
        .command_for("codex", &[], &second_project, &second_home)
        .output()
        .expect("cross-project recovery attempt");
    let stderr = String::from_utf8_lossy(&refused.stderr);
    let recorded_project = fs::canonicalize(&fixture.project).expect("recorded project root");
    let unrelated_project = fs::canonicalize(&second_project).expect("unrelated project root");

    assert_eq!(refused.status.code(), Some(75), "{stderr}");
    let recovery_project = stderr
        .lines()
        .find(|line| line.contains("recovery[0] argv[3] ="))
        .expect("targeted project recovery argv");
    assert!(
        recovery_project.contains(&recorded_project.to_string_lossy().into_owned()),
        "the recovery command must target the quarantined journal's project: {recovery_project}"
    );
    assert!(
        !recovery_project.contains(&unrelated_project.to_string_lossy().into_owned()),
        "the current but unrelated project must not be suggested: {recovery_project}"
    );
    assert!(exists(&fixture.project.join(".agents/skills/alpha")));
    assert_eq!(fixture.journals().len(), 1);
}

#[test]
fn automatic_recovery_reloads_a_journal_that_advanced_to_supervising() {
    let fixture = Fixture::new("automatic-recovery-refresh-after-lock");
    fixture.skill("alpha");
    let session_release = fixture.root.join("release-supervising-session");
    let recovery_release = fixture.root.join("release-recovery");

    let mut session = fixture
        .command("codex", &[])
        .env("SKILLMOUNT_HOLD_AT", "journal-planned")
        .env("SKILLMOUNT_HOLD_MS", "10000")
        .env("SKILLMOUNT_HOLD_UNTIL", &session_release)
        .env("SKILLMOUNT_STOP_AT", "journal-supervising")
        .stderr(Stdio::piped())
        .spawn()
        .expect("session that advances to supervision");
    let mut session_stderr = wait_for_hold(&mut session, "journal-planned");

    let second_project = fixture.root.join("second-project");
    let second_home = fixture.root.join("second-home");
    fs::create_dir(&second_project).expect("second project");
    let mut recovery = fixture.command_for("codex", &[], &second_project, &second_home);
    recovery
        .env("SKILLMOUNT_HOLD_AT", "journal-scan-complete")
        .env("SKILLMOUNT_HOLD_MS", "10000")
        .env("SKILLMOUNT_HOLD_UNTIL", &recovery_release)
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    let mut recovery = recovery.spawn().expect("overlapping recovery session");
    let mut recovery_stderr = wait_for_hold(&mut recovery, "journal-scan-complete");

    fs::write(&session_release, "continue").expect("release supervising session");
    let session_status = session.wait().expect("supervising session stops");
    let mut session_diagnostics = String::new();
    session_stderr
        .read_to_string(&mut session_diagnostics)
        .expect("session diagnostics");
    assert!(!session_status.success(), "{session_diagnostics}");
    assert!(
        session_diagnostics.contains("stopping at journal-supervising"),
        "{session_diagnostics}"
    );

    fs::write(&recovery_release, "continue").expect("release recovery session");
    let recovery_status = recovery.wait().expect("recovery session finishes");
    let mut recovery_diagnostics = String::new();
    recovery_stderr
        .read_to_string(&mut recovery_diagnostics)
        .expect("recovery diagnostics");

    assert_eq!(recovery_status.code(), Some(75), "{recovery_diagnostics}");
    assert!(
        recovery_diagnostics.contains("process-domain death was never proved"),
        "{recovery_diagnostics}"
    );
    assert!(
        exists(&fixture.project.join(".agents/skills/alpha")),
        "fresh supervising state must be quarantined, never automatically cleaned"
    );
    assert_eq!(fixture.journals().len(), 1, "ownership evidence remains");
}

#[test]
fn automatic_recovery_fails_closed_when_a_scanned_journal_disappears() {
    let fixture = Fixture::new("automatic-recovery-missing-journal");
    fixture.skill("alpha");
    let stopped = fixture.run_stopping_at("codex", "journal-active", &[]);
    assert!(!stopped.status.success());
    let mount = fixture.project.join(".agents/skills/alpha");
    assert!(exists(&mount));
    let journal = fixture
        .journals()
        .into_iter()
        .next()
        .expect("incomplete journal");

    let second_project = fixture.root.join("second-project");
    let second_home = fixture.root.join("second-home");
    fs::create_dir(&second_project).expect("second project");
    let release = fixture.root.join("release-missing-journal-recovery");
    let mut recovery = fixture.command_for("codex", &[], &second_project, &second_home);
    recovery
        .env("SKILLMOUNT_HOLD_AT", "journal-scan-complete")
        .env("SKILLMOUNT_HOLD_MS", "10000")
        .env("SKILLMOUNT_HOLD_UNTIL", &release)
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    let mut recovery = recovery
        .spawn()
        .expect("recovery with a stale candidate scan");
    let mut recovery_stderr = wait_for_hold(&mut recovery, "journal-scan-complete");

    fs::remove_file(&journal).expect("simulate journal-only external removal");
    fs::write(&release, "continue").expect("release recovery");
    let recovery_status = recovery.wait().expect("recovery finishes");
    let mut recovery_diagnostics = String::new();
    recovery_stderr
        .read_to_string(&mut recovery_diagnostics)
        .expect("recovery diagnostics");

    assert_eq!(recovery_status.code(), Some(75), "{recovery_diagnostics}");
    assert!(
        recovery_diagnostics.contains("disappeared"),
        "{recovery_diagnostics}"
    );
    assert!(exists(&mount), "the residual mount is retained");
    assert!(
        !exists(&second_project.join(".agents")),
        "the new session must stop before planning or mutation"
    );
    assert!(fixture.journals().is_empty());
}

#[test]
fn no_recover_succeeds_when_there_is_nothing_to_reconcile() {
    let fixture = Fixture::new("no-recover-clean");
    fixture.skill("alpha");

    let output = fixture.run("codex", &["--no-recover"]);

    assert_eq!(
        output.status.code(),
        Some(FIXTURE_CHILD_STATUS),
        "a clean state must not be refused: {}",
        String::from_utf8_lossy(&output.stderr)
    );
}

#[test]
fn a_kept_transaction_survives_every_later_session() {
    let fixture = Fixture::new("kept");
    fixture.skill("alpha");

    let kept = fixture.run("codex", &["--keep-mounts"]);
    assert_eq!(kept.status.code(), Some(FIXTURE_CHILD_STATUS));
    let kept_stderr = String::from_utf8_lossy(&kept.stderr);
    assert!(
        kept_stderr.contains("retained because --keep-mounts was requested"),
        "intentional retention must be diagnosed as requested: {kept_stderr}"
    );
    assert!(
        !kept_stderr.contains("cleanup could not finish"),
        "intentional retention is not a cleanup failure: {kept_stderr}"
    );
    let mounted = fixture.project.join(".agents/skills/alpha");
    assert!(exists(&mounted), "--keep-mounts retains the mounts");
    assert_eq!(fixture.journals().len(), 1, "the kept journal is retained");

    // A later session sees the mount, reuses it, and must not treat the terminal journal as stale.
    let later = fixture.run("codex", &[]);

    assert_eq!(
        later.status.code(),
        Some(FIXTURE_CHILD_STATUS),
        "{}",
        String::from_utf8_lossy(&later.stderr)
    );
    assert!(
        exists(&mounted),
        "a terminal kept transaction is never recovered automatically"
    );
    assert_eq!(
        fixture.journals().len(),
        1,
        "only the kept journal remains: {:?}",
        fixture.journals()
    );
}

#[test]
fn keep_enabled_crashes_before_terminal_keep_are_reconciled() {
    for boundary in [
        "journal-planned",
        "journal-applying",
        "action-staged@3",
        "journal-active",
        "journal-cleaning",
    ] {
        let fixture = Fixture::new(&format!("keep-crash-{boundary}"));
        fixture.skill("alpha");

        let stopped = fixture.run_stopping_at("codex", boundary, &["--keep-mounts"]);
        assert!(
            !stopped.status.success(),
            "the keep-enabled session must stop at {boundary}: {}",
            String::from_utf8_lossy(&stopped.stderr)
        );

        let recovered = fixture.run("codex", &[]);

        assert_eq!(
            recovered.status.code(),
            Some(FIXTURE_CHILD_STATUS),
            "the incomplete keep request must reconcile at {boundary}: {}",
            String::from_utf8_lossy(&recovered.stderr)
        );
        assert!(
            fixture.project_tree().is_empty(),
            "partial keep state must not become permanent at {boundary}: {:?}",
            fixture.project_tree()
        );
        assert!(
            fixture.journals().is_empty(),
            "no partial keep journal may become terminal at {boundary}: {:?}",
            fixture.journals()
        );
    }
}

#[test]
fn a_journal_this_build_cannot_interpret_blocks_every_mutating_run() {
    let fixture = Fixture::new("corrupt-journal");
    fixture.skill("alpha");
    fs::create_dir_all(fixture.transactions()).expect("transaction directory");
    let corrupt = fixture.transactions().join("aaaa-bbbb.journal");
    fs::write(&corrupt, "skillmount-journal 99 unix deadbeef\n").expect("corrupt journal");

    let output = fixture.run("codex", &[]);

    assert_eq!(
        output.status.code(),
        Some(75),
        "unknown recovery state must fail closed before planning or mutation: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(
        String::from_utf8_lossy(&output.stderr).contains("cannot be interpreted"),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert_eq!(
        fs::read_to_string(&corrupt).expect("still readable"),
        "skillmount-journal 99 unix deadbeef\n",
        "a journal that cannot be read is never rewritten or removed"
    );
    assert!(
        fixture.project_tree().is_empty(),
        "the unknown journal must block every project mutation: {:?}",
        fixture.project_tree()
    );
    assert!(
        !fixture.state.join("locks").exists(),
        "the read-only rejection preflight must run before new lock-state mutation"
    );

    let refused = fixture.run("codex", &["--no-recover"]);
    assert_eq!(
        refused.status.code(),
        Some(75),
        "under --no-recover an uninterpretable journal is a hard stop"
    );
}

#[test]
fn a_corrupt_current_schema_journal_also_blocks_an_ordinary_run() {
    let fixture = Fixture::new("corrupt-current-journal");
    fixture.skill("alpha");
    fs::create_dir_all(fixture.transactions()).expect("transaction directory");
    let corrupt = fixture.transactions().join("aaaa-cccc.journal");
    fs::write(&corrupt, "skillmount-journal 1 unix deadbeef\n").expect("corrupt journal");

    let output = fixture.run("codex", &[]);

    assert_eq!(output.status.code(), Some(75));
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(stderr.contains(&*corrupt.to_string_lossy()), "{stderr}");
    assert!(
        stderr.contains("account for every recorded path"),
        "{stderr}"
    );
    assert_eq!(
        fs::read_to_string(&corrupt).expect("retained journal"),
        "skillmount-journal 1 unix deadbeef\n"
    );
    assert!(fixture.project_tree().is_empty());
}

#[test]
fn an_unknown_journal_blocks_recovery_of_healthy_neighbors_before_any_removal() {
    let fixture = Fixture::new("unknown-blocks-healthy-recovery");
    fixture.skill("alpha");
    let stopped = fixture.run_stopping_at("codex", "journal-active", &[]);
    assert!(!stopped.status.success());
    let before = fixture.project_tree();
    assert!(
        exists(&fixture.project.join(".agents/skills/alpha")),
        "the healthy incomplete journal must own a visible mount: {before:?}"
    );
    let unknown = fixture.transactions().join("ffff-future.journal");
    fs::write(&unknown, "skillmount-journal 99 unix deadbeef\n").expect("future journal");

    let output = fixture.run("codex", &[]);

    assert_eq!(output.status.code(), Some(75));
    assert_eq!(
        fixture.project_tree(),
        before,
        "unknown state must stop the whole recovery pass before a healthy neighbor is changed"
    );
    assert_eq!(fixture.journals().len(), 2);
    assert!(unknown.exists());
}

#[test]
fn a_preexisting_destination_conflict_leaves_the_obstruction_untouched() {
    let fixture = Fixture::new("late-conflict");
    fixture.skill("alpha").skill("beta");
    // Occupies the destination of a later Skill link, so earlier actions apply and one fails.
    fs::create_dir_all(fixture.project.join(".agents/skills/beta/mine")).expect("obstruction");

    let output = fixture.run("codex", &[]);

    assert_eq!(
        output.status.code(),
        Some(73),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(
        exists(&fixture.project.join(".agents/skills/beta/mine")),
        "the obstruction is never touched"
    );
    assert!(fixture.project.join(".agents/skills").is_dir());
    assert!(
        !exists(&fixture.project.join(".agents/skills/alpha")),
        "including the mount that had already succeeded"
    );
}

#[test]
fn a_conflict_introduced_after_preliminary_discovery_is_seen_under_lock() {
    let fixture = Fixture::new("conflict-after-discovery");
    fixture.skill("alpha");

    let mut session = fixture
        .command("codex", &[])
        .env("SKILLMOUNT_HOLD_AT", "discovery-inspected")
        .env("SKILLMOUNT_HOLD_MS", "4000")
        .stderr(Stdio::piped())
        .spawn()
        .expect("the session should reach preliminary discovery");
    let mut diagnostics = wait_for_hold(&mut session, "discovery-inspected");

    let obstruction = fixture.project.join(".agents/skills/alpha/operator-owned");
    fs::create_dir_all(&obstruction).expect("late operator-owned Skill");

    let status = session.wait().expect("the held session remains waitable");
    let mut remaining = String::new();
    diagnostics
        .read_to_string(&mut remaining)
        .expect("the held session diagnostics remain readable");

    assert_eq!(status.code(), Some(73), "{remaining}");
    assert!(
        obstruction.is_dir(),
        "the late conflict must never be replaced"
    );
    assert!(
        fixture.journals().is_empty(),
        "the locked rebuild must reject the conflict before opening a transaction"
    );
}

#[test]
fn a_claude_conflict_introduced_after_preliminary_discovery_is_seen_under_lock() {
    let fixture = Fixture::new("claude-conflict-after-discovery");
    fixture.skill("alpha");

    let mut session = fixture
        .command("claude", &[])
        .env("SKILLMOUNT_HOLD_AT", "discovery-inspected")
        .env("SKILLMOUNT_HOLD_MS", "4000")
        .stderr(Stdio::piped())
        .spawn()
        .expect("the Claude session should reach preliminary discovery");
    let mut diagnostics = wait_for_hold(&mut session, "discovery-inspected");

    let obstruction = fixture.project.join(".claude/skills/alpha/operator-owned");
    fs::create_dir_all(&obstruction).expect("late project-owned Claude Skill");

    let status = session.wait().expect("the held session remains waitable");
    let mut remaining = String::new();
    diagnostics
        .read_to_string(&mut remaining)
        .expect("the held session diagnostics remain readable");

    assert_eq!(status.code(), Some(73), "{remaining}");
    assert!(obstruction.is_dir());
    assert!(
        fixture.journals().is_empty(),
        "the locked rebuild rejects the conflict before opening a transaction"
    );
    assert!(fixture.session_tree().is_empty());
}

/// A hard control appearing while the locks are held must abort before any intent is durable.
///
/// The repeated post-lock check exists because acquisition can wait behind a long-running session.
/// Failing there must leave no journal and no destination mutation at all, which is what separates
/// it from the post-apply case below.
#[test]
fn hard_agent_controls_appearing_after_lock_stabilization_prevent_any_intent() {
    for (agent, marker_variable, expected_error) in [
        (
            "codex",
            "SKILLMOUNT_TEST_CODEX_MANAGED_CONFIG_PATH",
            "legacy managed configuration",
        ),
        (
            "claude",
            "SKILLMOUNT_TEST_CLAUDE_ENVIRONMENT_CONTROL_PATH",
            "environment-control marker",
        ),
    ] {
        let fixture = Fixture::new(&format!("{agent}-control-after-lock"));
        fixture.skill("alpha");
        let marker = fixture.root.join("late-agent-control");
        let release = fixture.root.join("release-agent-control");
        let mut session = fixture
            .command(agent, &[])
            .env(marker_variable, &marker)
            .env("SKILLMOUNT_HOLD_AT", "journal-scan-complete")
            .env("SKILLMOUNT_HOLD_MS", "10000")
            .env("SKILLMOUNT_HOLD_UNTIL", &release)
            .stderr(Stdio::piped())
            .spawn()
            .expect("the session should reach its locked recovery scan");
        let mut diagnostics = wait_for_hold(&mut session, "journal-scan-complete");

        assert!(
            fixture.journals().is_empty(),
            "{agent}: nothing may be durable before the repeated hard check"
        );
        fs::write(&marker, b"present\n").expect("introduce the late hard Agent control");
        fs::write(&release, b"continue\n").expect("release the locked replan");

        let status = session.wait().expect("the held session remains waitable");
        let mut remaining = String::new();
        diagnostics
            .read_to_string(&mut remaining)
            .expect("the held session diagnostics remain readable");

        assert_eq!(status.code(), Some(64), "{agent}: {remaining}");
        assert!(remaining.contains(expected_error), "{agent}: {remaining}");
        assert!(
            !remaining.contains("Launching"),
            "{agent}: the Agent child must not start: {remaining}"
        );
        assert!(
            fixture.journals().is_empty(),
            "{agent}: a pre-intent failure writes no journal"
        );
        assert!(
            !exists(&fixture.project.join(".agents")),
            "{agent}: no destination directory may be created"
        );
        assert!(
            !exists(&fixture.project.join(".claude")),
            "{agent}: no destination directory may be created"
        );
        assert!(
            fixture.session_tree().is_empty(),
            "{agent}: no staging entry may be created"
        );
    }
}

#[test]
fn hard_agent_controls_appearing_after_apply_prevent_child_and_force_owned_cleanup() {
    for (agent, marker_variable, expected_error) in [
        (
            "codex",
            "SKILLMOUNT_TEST_CODEX_MANAGED_CONFIG_PATH",
            "legacy managed configuration",
        ),
        (
            "claude",
            "SKILLMOUNT_TEST_CLAUDE_ENVIRONMENT_CONTROL_PATH",
            "environment-control marker",
        ),
    ] {
        let fixture = Fixture::new(&format!("{agent}-control-after-apply"));
        fixture.skill("alpha");
        let marker = fixture.root.join("late-agent-control");
        let release = fixture.root.join("release-agent-control");
        let mut session = fixture
            .command(agent, &["--keep-mounts"])
            .env(marker_variable, &marker)
            .env("SKILLMOUNT_HOLD_AT", "journal-active")
            .env("SKILLMOUNT_HOLD_MS", "10000")
            .env("SKILLMOUNT_HOLD_UNTIL", &release)
            .stderr(Stdio::piped())
            .spawn()
            .expect("the session should reach its active transaction");
        let mut diagnostics = wait_for_hold(&mut session, "journal-active");

        match agent {
            "codex" => assert!(exists(&fixture.project.join(".agents/skills/alpha"))),
            "claude" => assert!(!fixture.session_tree().is_empty()),
            _ => unreachable!("the fixture table names both supported Agents"),
        }
        fs::write(&marker, b"present\n").expect("introduce the late hard Agent control");
        fs::write(&release, b"continue\n").expect("release the spawn-boundary check");

        let status = session.wait().expect("the held session remains waitable");
        let mut remaining = String::new();
        diagnostics
            .read_to_string(&mut remaining)
            .expect("the held session diagnostics remain readable");

        assert_eq!(status.code(), Some(64), "{agent}: {remaining}");
        assert!(remaining.contains(expected_error), "{agent}: {remaining}");
        assert!(
            !remaining.contains("Launching"),
            "the Agent child must not start: {agent}: {remaining}"
        );
        assert!(
            marker.is_file(),
            "the external control is not transaction-owned"
        );
        assert!(
            fixture.journals().is_empty(),
            "matching-evidence cleanup retires the {agent} transaction"
        );
        match agent {
            "codex" => {
                assert!(!exists(&fixture.project.join(".agents")));
                assert!(!exists(&fixture.project.join(".codex")));
            }
            "claude" => assert!(fixture.session_tree().is_empty()),
            _ => unreachable!("the fixture table names both supported Agents"),
        }
    }
}

#[test]
fn a_cleanup_that_cannot_finish_keeps_its_journal_and_its_evidence() {
    let fixture = Fixture::new("cleanup-blocked");
    fixture.skill("alpha");
    fixture.run_stopping_at("codex", "journal-active", &[]);

    // Something is added to the store, so the store directory can no longer be removed while the
    // mount inside it still can.
    fs::write(fixture.project.join(".agents/skills/notes.md"), "mine").expect("user content");
    let recovered = fixture.run("codex", &[]);

    assert!(
        exists(&fixture.project.join(".agents/skills/notes.md")),
        "the operator's file must survive"
    );
    let stderr = String::from_utf8_lossy(&recovered.stderr);
    assert!(stderr.contains("holds entries"), "{stderr}");
    assert!(
        !fixture.journals().is_empty(),
        "a cleanup that retained something keeps the journal describing it"
    );
}

#[test]
fn two_codex_sessions_on_one_store_serialize() {
    for checkpoint in ["journal-active", "journal-cleaning"] {
        let fixture = Fixture::new(&format!("codex-serialized-{checkpoint}"));
        fixture.skill("alpha");

        // The first session pauses while holding its locks, once during apply and once immediately
        // before cleanup/removal. A second session must not enter either mutation interval.
        let mut holder: Child = fixture
            .command("codex", &[])
            .env("SKILLMOUNT_HOLD_AT", checkpoint)
            .env("SKILLMOUNT_HOLD_MS", "4000")
            .stderr(Stdio::piped())
            .spawn()
            .expect("the first session should start");
        let mut holder_stderr = wait_for_hold(&mut holder, checkpoint);

        let contender = fixture.run("codex", &[]);
        let holder_status = holder.wait().expect("the first session remains waitable");
        let mut holder_diagnostics = String::new();
        holder_stderr
            .read_to_string(&mut holder_diagnostics)
            .expect("the first session stderr remains readable");

        assert_eq!(
            holder_status.code(),
            Some(FIXTURE_CHILD_STATUS),
            "the first session must launch and finish after holding at \
             {checkpoint}: {holder_diagnostics}"
        );

        assert_eq!(
            contender.status.code(),
            Some(75),
            "a second Codex session on the same store must wait or report a temporary failure at \
             {checkpoint}: {}",
            String::from_utf8_lossy(&contender.stderr)
        );
        let stderr = String::from_utf8_lossy(&contender.stderr);
        assert!(
            stderr.contains("another SkillMount session holds"),
            "{checkpoint}: {stderr}"
        );
        assert!(
            stderr.contains("nothing was changed"),
            "{checkpoint}: {stderr}"
        );
        assert!(stderr.contains("asm doctor"), "{checkpoint}: {stderr}");
    }
}

#[test]
fn codex_sessions_reaching_one_nested_collection_through_distinct_links_serialize() {
    let fixture = Fixture::new("codex-nested-terminal-lock");
    fixture.skill("alpha");
    let second_project = fixture.root.join("second-project");
    let first_home = fixture.root.join("first-home");
    let second_home = fixture.root.join("second-home");
    let shared = fixture.root.join("shared-collection");
    fs::create_dir_all(&second_project).expect("second project");
    let foreign = shared.join("nested/foreign");
    fs::create_dir_all(&foreign).expect("shared Skill collection");
    fs::write(
        foreign.join("SKILL.md"),
        "---\nname: foreign\ndescription: shared collection fixture\n---\n",
    )
    .expect("shared Skill metadata");

    for home in [&first_home, &second_home] {
        let root = home.join(".agents/skills");
        fs::create_dir_all(&root).expect("isolated user Skill root");
        let staged_path = root.join(".collection.skillmount-fixture");
        let backend = platform_backend();
        let staged = backend
            .create_directory_link(&LinkRequest {
                source: backend
                    .canonical_directory(&shared)
                    .expect("canonical shared collection"),
                staged_path,
                mode: LinkMode::Auto,
            })
            .expect("nested collection link fixture");
        let outcome = backend
            .place_no_replace(&staged, &root.join("collection"))
            .expect("place nested collection link fixture");
        assert!(matches!(outcome, PlacementOutcome::Placed(_)));
    }

    let mut holder: Child = fixture
        .command_for("codex", &[], &fixture.project, &first_home)
        .env("SKILLMOUNT_HOLD_AT", "journal-active")
        .env("SKILLMOUNT_HOLD_MS", "4000")
        .stderr(Stdio::piped())
        .spawn()
        .expect("the first session should start");
    let mut holder_stderr = wait_for_hold(&mut holder, "journal-active");

    let contender = fixture
        .command_for("codex", &[], &second_project, &second_home)
        .output()
        .expect("the second session should report contention");
    let holder_status = holder.wait().expect("the first session remains waitable");
    let mut holder_diagnostics = String::new();
    holder_stderr
        .read_to_string(&mut holder_diagnostics)
        .expect("the first session stderr remains readable");

    assert_eq!(
        holder_status.code(),
        Some(FIXTURE_CHILD_STATUS),
        "{holder_diagnostics}"
    );
    assert_eq!(
        contender.status.code(),
        Some(75),
        "distinct logical roots reaching one collection must share its physical lock: {}",
        String::from_utf8_lossy(&contender.stderr)
    );
    assert!(
        String::from_utf8_lossy(&contender.stderr).contains("another SkillMount session holds"),
        "{}",
        String::from_utf8_lossy(&contender.stderr)
    );
    assert!(
        !exists(&second_project.join(".agents")),
        "the contending project must remain untouched"
    );
}

#[test]
fn two_isolated_claude_sessions_do_not_serialize() {
    let fixture = Fixture::new("claude-concurrent");
    fixture.skill("alpha");

    let mut holder: Child = fixture
        .command("claude", &[])
        .env("SKILLMOUNT_HOLD_AT", "journal-active")
        .env("SKILLMOUNT_HOLD_MS", "4000")
        .spawn()
        .expect("the first session should start");
    wait_for(|| !fixture.journals().is_empty());

    let concurrent = fixture.run("claude", &[]);
    let _ = holder.wait();

    assert_eq!(
        concurrent.status.code(),
        Some(FIXTURE_CHILD_STATUS),
        "two Claude sessions with separate staging roots share no mutable resource: {}",
        String::from_utf8_lossy(&concurrent.stderr)
    );
    assert!(
        !String::from_utf8_lossy(&concurrent.stderr).contains("another SkillMount session holds"),
        "isolated staging must not serialize child execution"
    );
}

#[test]
fn a_claude_session_stages_under_its_own_identifier() {
    let fixture = Fixture::new("claude-staging-id");
    fixture.skill("alpha");

    let output = fixture.run("claude", &["--keep-mounts"]);

    assert_eq!(
        output.status.code(),
        Some(FIXTURE_CHILD_STATUS),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let sessions = fs::read_dir(fixture.state.join("sessions"))
        .expect("a staging root was created")
        .filter_map(Result::ok)
        .map(|entry| entry.file_name().to_string_lossy().into_owned())
        .collect::<Vec<_>>();
    assert_eq!(sessions.len(), 1, "{sessions:?}");
    assert!(
        !sessions[0].contains('<'),
        "a mutating run must replace the planning placeholder with a real identifier: {sessions:?}"
    );
    assert!(
        exists(
            &fixture
                .state
                .join("sessions")
                .join(&sessions[0])
                .join("root/.claude/skills/alpha")
        ),
        "the Skill is staged inside that root"
    );
}

#[test]
fn a_claude_session_removes_its_whole_staging_root_at_cleanup() {
    let fixture = Fixture::new("claude-cleanup");
    fixture.skill("alpha").skill("beta");

    let output = fixture.run("claude", &[]);

    assert_eq!(
        output.status.code(),
        Some(FIXTURE_CHILD_STATUS),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    // Claude staging creates a four-deep chain — `<id>`, `<id>/root`, `<id>/root/.claude`,
    // `<id>/root/.claude/skills` — so reverse-order removal carries more weight here than in the
    // Codex layout every other cleanup test covers. Its lock resources are unanchored too, which
    // is a different derivation from the anchored Codex ones.
    let staged = fs::read_dir(fixture.state.join("sessions"))
        .expect("the shared sessions directory survives")
        .filter_map(Result::ok)
        .map(|entry| entry.path())
        .collect::<Vec<_>>();
    assert!(
        staged.is_empty(),
        "every directory the session created must be gone: {staged:?}"
    );
    assert!(
        fixture.state.join("sessions").is_dir(),
        "the shared parent is not the session's to remove"
    );
    assert!(
        fixture.journals().is_empty(),
        "a completed transaction leaves no journal: {:?}",
        fixture.journals()
    );
    assert!(
        fixture.project_tree().is_empty(),
        "staging never touches the project: {:?}",
        fixture.project_tree()
    );
    for name in ["alpha", "beta"] {
        assert!(
            exists(&fixture.sources.join(name).join("SKILL.md")),
            "removing a staged link must never reach the source it pointed at"
        );
    }
}

/// The three planned OMP entries, in the order the plan creates them.
///
/// Written as components rather than a literal path so the assertion text matches the platform's
/// own separator, which is what the renderer and the project walk both produce.
fn omp_scope_entries() -> [PathBuf; 3] {
    [
        PathBuf::from(".omp"),
        PathBuf::from(".omp").join("skills"),
        PathBuf::from(".omp").join("skills").join("alpha"),
    ]
}

/// Writes a project-owned Skill directly into an already-existing OMP project scope.
fn install_existing_omp_scope(project: &Path) {
    let owned = project.join(".omp/skills/rasen");
    fs::create_dir_all(&owned).expect("project-owned OMP Skill");
    fs::write(
        owned.join("SKILL.md"),
        "---\nname: rasen\ndescription: project fixture\n---\n",
    )
    .expect("project-owned Skill metadata");
}

#[test]
fn an_omp_session_creates_its_project_scope_marks_it_active_and_releases_all_of_it() {
    let fixture = Fixture::new("omp-apply-and-release");
    fixture.skill("alpha");
    let mounted = fixture.project.join(".omp/skills/alpha");

    let mut holder = fixture
        .command("omp", &[])
        .env("SKILLMOUNT_HOLD_AT", "journal-active")
        .env("SKILLMOUNT_HOLD_MS", "4000")
        .stderr(Stdio::piped())
        .spawn()
        .expect("the OMP session should reach its active transaction");
    let mut holder_stderr = wait_for_hold(&mut holder, "journal-active");

    assert_eq!(
        fixture.project_tree(),
        omp_scope_entries()
            .iter()
            .map(|entry| entry.display().to_string())
            .collect::<Vec<_>>(),
        "applying creates exactly the OMP scope, its store, and the selected link"
    );
    assert_eq!(
        fs::read_to_string(mounted.join("SKILL.md")).expect("the mount resolves to its source"),
        fs::read_to_string(fixture.sources.join("alpha/SKILL.md")).expect("the source is readable"),
        "the mounted entry must reach the canonical Skill source"
    );

    // An operator cleanup proves the journal is durably active rather than merely planned: an
    // active transaction is reported and left strictly alone.
    let reported = fixture.cleanup(true);

    assert_eq!(
        reported.status.code(),
        Some(75),
        "{}",
        String::from_utf8_lossy(&reported.stdout)
    );
    let rendered = String::from_utf8_lossy(&reported.stdout);
    assert!(
        rendered.contains("[ACTIVE] omp transaction"),
        "the active journal names its own Agent: {rendered}"
    );
    assert!(exists(&mounted), "an active OMP mount is left untouched");
    assert_eq!(fixture.journals().len(), 1);

    let status = holder.wait().expect("the held session remains waitable");
    let mut diagnostics = String::new();
    holder_stderr
        .read_to_string(&mut diagnostics)
        .expect("the held session diagnostics remain readable");

    assert_eq!(status.code(), Some(FIXTURE_CHILD_STATUS), "{diagnostics}");
    assert!(
        fixture.project_tree().is_empty(),
        "cleanup removes the whole OMP scope it created: {:?}",
        fixture.project_tree()
    );
    assert!(
        fixture.journals().is_empty(),
        "a completed OMP session retires its journal: {:?}",
        fixture.journals()
    );
    assert!(fixture.sources.join("alpha/SKILL.md").is_file());
}

/// Every automatically recoverable boundary an OMP session reaches survives a second invocation.
///
/// The three remaining names in `Checkpoint::ALL` are deliberately outside this loop and are
/// covered elsewhere: `journal-supervising` is quarantined rather than recovered,
/// `journal-completed` describes an already-finished cleanup, and `journal-retired` is only
/// reachable on the platform that needs a write-through rename.
#[test]
fn a_second_omp_invocation_recovers_every_boundary_and_leaves_the_project_clean() {
    for boundary in BOUNDARIES {
        let fixture = Fixture::new(&format!("recover-omp-{boundary}"));
        fixture.skill("alpha");
        let source_body = fs::read_to_string(fixture.sources.join("alpha/SKILL.md"))
            .expect("the Skill source is readable");

        let killed = fixture.run_stopping_at("omp", boundary, &[]);
        assert!(
            String::from_utf8_lossy(&killed.stderr)
                .contains(&format!("stopping at {boundary} occurrence")),
            "an OMP session never reaches {boundary}, so nothing about it is under test: {}",
            String::from_utf8_lossy(&killed.stderr)
        );

        // A real second invocation, against whatever the first one left behind.
        let recovered = fixture.run("omp", &[]);

        assert_eq!(
            recovered.status.code(),
            Some(FIXTURE_CHILD_STATUS),
            "the recovering OMP session must launch and clean up after its fixture child at \
             {boundary}: {}",
            String::from_utf8_lossy(&recovered.stderr)
        );
        // Removing a link must never be followed into the directory it pointed at, so the source
        // has to be byte-identical rather than merely present.
        assert_eq!(
            fs::read_to_string(fixture.sources.join("alpha/SKILL.md"))
                .expect("no boundary may ever cost a Skill source"),
            source_body,
            "recovery followed the OMP mount into its source at {boundary}"
        );

        let residue = fixture.project_tree();
        if boundary == "temporary-created" {
            // The one boundary that cannot be reconciled: the temporary entry exists and nothing
            // proves which transaction made it, so both it and its journal stay, reported.
            assert!(
                residue.iter().any(|entry| entry.contains(".skillmount-")),
                "the unprovable staged entry must still be present at {boundary}: {residue:?}"
            );
            assert!(
                String::from_utf8_lossy(&recovered.stderr).contains("retained"),
                "retained residue must be reported: {}",
                String::from_utf8_lossy(&recovered.stderr)
            );
            assert_eq!(fixture.journals().len(), 1);
        } else {
            assert!(
                residue.is_empty(),
                "an OMP session stopped at {boundary} must leave the project clean once \
                 recovered: {residue:?}"
            );
            assert!(
                fixture.journals().is_empty(),
                "recovery plus a completed session must leave no journal at {boundary}: {:?}",
                fixture.journals()
            );
        }
    }
}

#[test]
fn an_existing_omp_project_scope_survives_every_reachable_recovery_boundary() {
    for boundary in CURRENT_LAYOUT_BOUNDARIES {
        let fixture = Fixture::new(&format!("recover-omp-existing-{boundary}"));
        fixture.skill("alpha");
        install_existing_omp_scope(&fixture.project);

        let owned = fixture.project.join(".omp/skills/rasen/SKILL.md");
        let owned_body = fs::read_to_string(&owned).expect("read the project-owned Skill");
        let baseline = fixture.project_tree();

        let killed = fixture.run_stopping_at("omp", boundary, &[]);
        assert!(
            String::from_utf8_lossy(&killed.stderr)
                .contains(&format!("stopping at {boundary} occurrence")),
            "an existing OMP scope never reaches {boundary}: {}",
            String::from_utf8_lossy(&killed.stderr)
        );

        let recovered = fixture.run("omp", &[]);

        assert_eq!(
            recovered.status.code(),
            Some(FIXTURE_CHILD_STATUS),
            "the existing OMP scope must recover and launch at {boundary}: {}",
            String::from_utf8_lossy(&recovered.stderr)
        );
        assert_eq!(
            fs::read_to_string(&owned).expect("the project-owned Skill survives"),
            owned_body,
            "recovery rewrote a Skill the transaction never created at {boundary}"
        );

        let recovered_tree = fixture.project_tree();
        if boundary == "temporary-created" {
            assert!(
                baseline.iter().all(|entry| recovered_tree.contains(entry)),
                "the existing scope must remain a subset of retained residue at {boundary}: \
                 {recovered_tree:?}"
            );
            assert!(
                recovered_tree
                    .iter()
                    .any(|entry| entry.contains(".skillmount-")),
                "the unrecorded staged entry must be retained at {boundary}: {recovered_tree:?}"
            );
            assert_eq!(fixture.journals().len(), 1);
        } else {
            assert_eq!(
                recovered_tree, baseline,
                "only the pre-existing OMP scope may remain after {boundary}"
            );
            assert!(fixture.journals().is_empty());
        }
    }
}

#[test]
fn omp_recovery_never_removes_an_entry_a_user_replaced_after_the_crash() {
    let fixture = Fixture::new("omp-replaced-after-crash");
    fixture.skill("alpha");
    fixture.run_stopping_at("omp", "journal-active", &[]);
    let crashed = fixture.journals();
    assert_eq!(
        crashed.len(),
        1,
        "the crashed session must leave exactly one journal: {crashed:?}"
    );

    // The operator replaces the mount with work of their own before the next session runs.
    let mounted = fixture.project.join(".omp/skills/alpha");
    if cfg!(windows) {
        fs::remove_dir(&mounted)
    } else {
        fs::remove_file(&mounted)
    }
    .expect("the crashed session left a link here");
    fs::create_dir_all(mounted.join("their-own-work")).expect("replacement");

    let recovered = fixture.run("omp", &[]);
    let stderr = String::from_utf8_lossy(&recovered.stderr);

    assert!(
        exists(&mounted.join("their-own-work")),
        "recovery must never delete something it cannot prove it created"
    );
    let reported = platform_backend()
        .canonical_directory(&fixture.project)
        .expect("canonical project root")
        .join(".omp/skills/alpha");
    assert!(
        stderr.contains(&format!("retained {}", reported.display())),
        "the mismatch must be reported against the replaced entry: {stderr}"
    );
    assert!(
        crashed[0].is_file(),
        "the journal describing unreconciled residue is kept: {:?}",
        fixture.journals()
    );
}

#[test]
fn a_corrupt_journal_blocks_a_new_omp_session_before_any_project_mutation() {
    let fixture = Fixture::new("omp-corrupt-journal");
    fixture.skill("alpha");
    fs::create_dir_all(fixture.transactions()).expect("transaction directory");
    let corrupt = fixture.transactions().join("aaaa-bbbb.journal");
    fs::write(&corrupt, "skillmount-journal 99 unix deadbeef\n").expect("corrupt journal");

    let output = fixture.run("omp", &[]);
    let stderr = String::from_utf8_lossy(&output.stderr);

    assert_eq!(
        output.status.code(),
        Some(75),
        "unknown recovery state must fail closed before an OMP scope is planned: {stderr}"
    );
    assert!(stderr.contains("cannot be interpreted"), "{stderr}");
    assert_eq!(
        fs::read_to_string(&corrupt).expect("still readable"),
        "skillmount-journal 99 unix deadbeef\n",
        "a journal that cannot be read is never rewritten or removed"
    );
    assert!(
        !exists(&fixture.project.join(".omp")),
        "the unknown journal must block the OMP scope itself: {:?}",
        fixture.project_tree()
    );
    assert!(
        !fixture.state.join("locks").exists(),
        "the read-only rejection preflight must run before new lock-state mutation"
    );
}

#[test]
fn a_kept_omp_transaction_stays_terminal_until_an_explicit_cleanup_releases_it() {
    let fixture = Fixture::new("omp-kept");
    fixture.skill("alpha");

    let kept = fixture.run("omp", &["--keep-mounts"]);
    let kept_stderr = String::from_utf8_lossy(&kept.stderr);

    assert_eq!(
        kept.status.code(),
        Some(FIXTURE_CHILD_STATUS),
        "{kept_stderr}"
    );
    assert!(
        kept_stderr.contains("retained because --keep-mounts was requested"),
        "intentional retention must be diagnosed as requested: {kept_stderr}"
    );
    assert!(
        !kept_stderr.contains("cleanup could not finish"),
        "intentional retention is not a cleanup failure: {kept_stderr}"
    );
    let mounted = fixture.project.join(".omp/skills/alpha");
    assert!(exists(&mounted), "--keep-mounts retains the OMP mount");
    assert_eq!(fixture.journals().len(), 1, "the kept journal is retained");

    // A later session reuses the kept scope and must not treat its journal as stale.
    let later = fixture.run("omp", &[]);

    assert_eq!(
        later.status.code(),
        Some(FIXTURE_CHILD_STATUS),
        "{}",
        String::from_utf8_lossy(&later.stderr)
    );
    assert!(
        exists(&mounted),
        "a terminal kept transaction is never recovered automatically"
    );
    assert_eq!(
        fixture.journals().len(),
        1,
        "only the kept journal remains: {:?}",
        fixture.journals()
    );

    let released = fixture.cleanup(false);
    let rendered = String::from_utf8_lossy(&released.stdout);

    assert!(released.status.success(), "{rendered}");
    assert!(
        rendered.contains("[RECOVERED] omp transaction"),
        "{rendered}"
    );
    assert!(
        fixture.project_tree().is_empty(),
        "an explicit cleanup releases the kept OMP scope: {:?}",
        fixture.project_tree()
    );
    assert!(fixture.journals().is_empty(), "{:?}", fixture.journals());
    assert!(fixture.sources.join("alpha/SKILL.md").is_file());
}

#[test]
fn two_omp_sessions_on_one_project_destination_serialize() {
    for checkpoint in ["journal-active", "journal-cleaning"] {
        let fixture = Fixture::new(&format!("omp-serialized-{checkpoint}"));
        fixture.skill("alpha");

        // The first session pauses while holding its locks, once during apply and once immediately
        // before cleanup/removal. A second session must not enter either mutation interval.
        let mut holder: Child = fixture
            .command("omp", &[])
            .env("SKILLMOUNT_HOLD_AT", checkpoint)
            .env("SKILLMOUNT_HOLD_MS", "4000")
            .stderr(Stdio::piped())
            .spawn()
            .expect("the first OMP session should start");
        let mut holder_stderr = wait_for_hold(&mut holder, checkpoint);
        let mounted = fixture.project.join(".omp/skills/alpha");
        let held = platform_backend()
            .inspect_no_follow(&mounted)
            .expect("inspect the held OMP mount");

        let contender = fixture.run("omp", &[]);

        assert_eq!(
            platform_backend()
                .inspect_no_follow(&mounted)
                .expect("reinspect the held OMP mount"),
            held,
            "the contender replaced or removed the holder's entry at {checkpoint}"
        );

        let holder_status = holder.wait().expect("the first session remains waitable");
        let mut holder_diagnostics = String::new();
        holder_stderr
            .read_to_string(&mut holder_diagnostics)
            .expect("the first session stderr remains readable");

        assert_eq!(
            holder_status.code(),
            Some(FIXTURE_CHILD_STATUS),
            "the first session must launch and finish after holding at \
             {checkpoint}: {holder_diagnostics}"
        );
        assert_eq!(
            contender.status.code(),
            Some(75),
            "a second OMP session on the same destination must report a temporary failure at \
             {checkpoint}: {}",
            String::from_utf8_lossy(&contender.stderr)
        );
        let stderr = String::from_utf8_lossy(&contender.stderr);
        assert!(
            stderr.contains("another SkillMount session holds"),
            "{checkpoint}: {stderr}"
        );
        assert!(
            stderr.contains("nothing was changed"),
            "{checkpoint}: {stderr}"
        );
        assert!(stderr.contains("asm doctor"), "{checkpoint}: {stderr}");
        assert!(
            fixture.project_tree().is_empty(),
            "the holder released its own scope and only its own: {:?}",
            fixture.project_tree()
        );
    }
}

#[test]
fn omp_sessions_reaching_one_destination_through_distinct_links_serialize() {
    let fixture = Fixture::new("omp-shared-destination-lock");
    fixture.skill("alpha");
    let second_project = fixture.root.join("second-project");
    let first_home = fixture.root.join("first-home");
    let second_home = fixture.root.join("second-home");
    let shared = fixture.root.join("shared-destination");
    fs::create_dir_all(&second_project).expect("second project");
    fs::create_dir_all(&shared).expect("shared physical destination");
    let backend = platform_backend();
    let canonical = backend
        .canonical_directory(&shared)
        .expect("canonical shared destination");

    for project in [&fixture.project, &second_project] {
        // OMP walks ancestors only up to the nearest repository root. Without a boundary of its
        // own each project would also share every ancestor-derived provider scope, and contention
        // would prove nothing about the destination key.
        fs::create_dir_all(project.join(".git")).expect("repository boundary");
        fs::create_dir_all(project.join(".omp")).expect("OMP scope");
        let staged = backend
            .create_directory_link(&LinkRequest {
                source: canonical.clone(),
                staged_path: project.join(".omp/.skills.skillmount-fixture"),
                mode: LinkMode::Auto,
            })
            .expect("shared destination link fixture");
        let outcome = backend
            .place_no_replace(&staged, &project.join(".omp/skills"))
            .expect("place shared destination link fixture");
        assert!(matches!(outcome, PlacementOutcome::Placed(_)));
    }

    let mut holder: Child = fixture
        .command_for("omp", &[], &fixture.project, &first_home)
        .env("SKILLMOUNT_HOLD_AT", "journal-active")
        .env("SKILLMOUNT_HOLD_MS", "4000")
        .stderr(Stdio::piped())
        .spawn()
        .expect("the first session should start");
    let mut holder_stderr = wait_for_hold(&mut holder, "journal-active");
    assert!(
        exists(&shared.join("alpha")),
        "the holder must own the shared destination entry while it is held"
    );

    let contender = fixture
        .command_for("omp", &[], &second_project, &second_home)
        .output()
        .expect("the second session should report contention");
    let contender_stderr = String::from_utf8_lossy(&contender.stderr);

    assert_eq!(
        contender.status.code(),
        Some(75),
        "distinct launch roots reaching one destination must share its physical lock: \
         {contender_stderr}"
    );
    assert!(
        contender_stderr.contains("another SkillMount session holds"),
        "{contender_stderr}"
    );
    assert!(
        contender_stderr.contains(&canonical.display().to_string()),
        "contention must name the shared physical destination: {contender_stderr}"
    );
    assert!(
        exists(&shared.join("alpha")),
        "the contender must not replace or remove the holder's entry"
    );

    let holder_status = holder.wait().expect("the first session remains waitable");
    let mut holder_diagnostics = String::new();
    holder_stderr
        .read_to_string(&mut holder_diagnostics)
        .expect("the first session stderr remains readable");

    assert_eq!(
        holder_status.code(),
        Some(FIXTURE_CHILD_STATUS),
        "{holder_diagnostics}"
    );
    let mut remaining = Vec::new();
    collect(&shared, &shared, &mut remaining);
    assert!(
        remaining.is_empty(),
        "the holder released the shared destination and the contender left nothing: {remaining:?}"
    );
}

/// Unsettled OMP global state appearing after apply must stop the child and release everything.
///
/// Unlike the Codex and Claude markers this one is a real OMP condition: a `settings.json` with no
/// `config.yml` beside it means the settings OMP will actually use are in no file `SkillMount` can
/// read, so the plan it just applied can no longer be proved correct.
#[test]
fn an_unsettled_omp_configuration_after_apply_prevents_the_child_and_forces_owned_cleanup() {
    let fixture = Fixture::new("omp-unsettled-after-apply");
    fixture.skill("alpha");
    let agent_dir = fixture.root.join("home/.omp/agent");
    let release = fixture.root.join("release-omp-configuration");
    let mut session = fixture
        .command("omp", &["--keep-mounts"])
        .env("SKILLMOUNT_HOLD_AT", "journal-active")
        .env("SKILLMOUNT_HOLD_MS", "10000")
        .env("SKILLMOUNT_HOLD_UNTIL", &release)
        .stderr(Stdio::piped())
        .spawn()
        .expect("the session should reach its active transaction");
    let mut diagnostics = wait_for_hold(&mut session, "journal-active");

    assert!(exists(&fixture.project.join(".omp/skills/alpha")));
    fs::create_dir_all(&agent_dir).expect("OMP agent directory");
    fs::write(agent_dir.join("settings.json"), b"{\"skills\":{}}\n")
        .expect("introduce unmigrated OMP global state");
    fs::write(&release, b"continue\n").expect("release the spawn-boundary check");

    let status = session.wait().expect("the held session remains waitable");
    let mut remaining = String::new();
    diagnostics
        .read_to_string(&mut remaining)
        .expect("the held session diagnostics remain readable");

    assert_eq!(status.code(), Some(64), "{remaining}");
    assert!(remaining.contains("has not yet migrated"), "{remaining}");
    assert!(
        !remaining.contains("Launching"),
        "the Agent child must not start: {remaining}"
    );
    assert!(
        agent_dir.join("settings.json").is_file(),
        "the operator's OMP state is not transaction-owned"
    );
    assert!(
        fixture.journals().is_empty(),
        "matching-evidence cleanup retires the OMP transaction despite --keep-mounts: {:?}",
        fixture.journals()
    );
    assert!(
        !exists(&fixture.project.join(".omp")),
        "the whole applied OMP scope is released: {:?}",
        fixture.project_tree()
    );
}

/// Explicit cleanup of an OMP journal reads the journal, never OMP itself.
///
/// Cleanup runs long after the session that recorded the journal, on a machine where OMP may have
/// been upgraded, reconfigured, or removed. If it needed the current version banner or the current
/// configuration to decide what it owns, a reconfigured OMP would strand mounts forever.
#[test]
fn omp_cleanup_needs_neither_the_omp_version_nor_its_configuration() {
    let fixture = Fixture::new("omp-cleanup-without-agent-state");
    fixture.skill("alpha");
    let home = fixture.root.join("home");
    let agent_dir = home.join(".omp/agent");
    fs::create_dir_all(&agent_dir).expect("OMP agent directory");
    fs::write(agent_dir.join("config.yml"), "skills:\n  enabled: true\n")
        .expect("settled OMP configuration");

    let killed = fixture.run_stopping_at("omp", "journal-active", &[]);
    assert!(
        !killed.status.success(),
        "the session must stop while active"
    );
    assert!(exists(&fixture.project.join(".omp/skills/alpha")));
    assert_eq!(fixture.journals().len(), 1);

    // Everything the OMP adapter reads is now gone: no configuration, no version evidence.
    fs::remove_dir_all(home.join(".omp")).expect("remove the OMP global state");

    let cleaned = fixture
        .cleanup_command_for(&fixture.project, false)
        .env("HOME", &home)
        .env("USERPROFILE", &home)
        .env_remove("SKILLMOUNT_TEST_OMP_VERSION")
        .output()
        .expect("asm cleanup should run");
    let rendered = String::from_utf8_lossy(&cleaned.stdout);

    assert!(cleaned.status.success(), "{rendered}");
    assert!(
        rendered.contains("[RECOVERED] omp transaction"),
        "cleanup names the journal's own Agent without consulting it: {rendered}"
    );
    // The renderer prints resolved absolute paths, so the expectation has to be resolved too.
    let resolved = platform_backend()
        .canonical_directory(&fixture.project)
        .expect("canonical project root");
    for entry in omp_scope_entries() {
        let removed = resolved.join(&entry);
        assert!(
            rendered.contains(&format!("removed {}", removed.display())),
            "cleanup must name {} as removed: {rendered}",
            entry.display()
        );
    }
    assert!(
        fixture.project_tree().is_empty(),
        "the whole OMP scope is reconciled from the journal alone: {:?}",
        fixture.project_tree()
    );
    assert!(fixture.journals().is_empty(), "{:?}", fixture.journals());
    assert!(fixture.sources.join("alpha/SKILL.md").is_file());
}

/// Waits up to two seconds for `condition`, so a spawned session is observably underway.
fn wait_for(condition: impl Fn() -> bool) {
    let deadline = Instant::now() + Duration::from_secs(2);
    while Instant::now() < deadline {
        if condition() {
            return;
        }
        std::thread::sleep(Duration::from_millis(10));
    }
    panic!("the spawned session never reached an observable state");
}

fn wait_for_hold(child: &mut Child, checkpoint: &str) -> BufReader<ChildStderr> {
    let stderr = child.stderr.take().expect("the holder stderr is piped");
    let expected = format!("failure injection holding at {checkpoint}");
    let (sender, receiver) = std::sync::mpsc::sync_channel(1);
    let reader = std::thread::spawn(move || {
        let mut stderr = BufReader::new(stderr);
        let outcome = loop {
            let mut line = String::new();
            match stderr.read_line(&mut line) {
                Ok(0) => break Err("the holder exited before reaching the checkpoint".to_owned()),
                Ok(_) if line.contains(&expected) => break Ok(stderr),
                Ok(_) => {}
                Err(error) => {
                    break Err(format!("the holder stderr became unreadable: {error}"));
                }
            }
        };
        let _ = sender.send(outcome);
    });

    match receiver.recv_timeout(HOLD_START_TIMEOUT) {
        Ok(Ok(stderr)) => {
            reader.join().expect("the stderr reader must not panic");
            stderr
        }
        Ok(Err(reason)) => {
            let kill_result = child.kill();
            let status = child.wait();
            reader.join().expect("the stderr reader must not panic");
            panic!("{reason} {checkpoint}; kill: {kill_result:?}; status: {status:?}");
        }
        Err(std::sync::mpsc::RecvTimeoutError::Timeout) => {
            let kill_result = child.kill();
            let status = child.wait();
            reader.join().expect("the stderr reader must unblock");
            panic!(
                "the holder did not reach {checkpoint} within {HOLD_START_TIMEOUT:?}; kill: \
                 {kill_result:?}; status: {status:?}"
            );
        }
        Err(std::sync::mpsc::RecvTimeoutError::Disconnected) => {
            let kill_result = child.kill();
            let status = child.wait();
            reader
                .join()
                .expect("the stderr reader failure is observable");
            panic!(
                "the stderr reader disconnected before {checkpoint}; kill: {kill_result:?}; \
                 status: {status:?}"
            );
        }
    }
}
