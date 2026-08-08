//! Proves that every read-only command leaves the filesystem and process state untouched.
//!
//! The unit suite asserts this at the planning boundary. This suite asserts it for the shipped
//! executable, including the error paths, because that is what an operator actually runs.

use std::collections::BTreeMap;
use std::fs::{self, OpenOptions};
use std::path::{Path, PathBuf};
use std::process::{Command, Output};
use std::time::{SystemTime, UNIX_EPOCH};

const ASM: &str = env!("CARGO_BIN_EXE_asm");
const SKILLMOUNT: &str = env!("CARGO_BIN_EXE_skillmount");
#[cfg(feature = "test-fixtures")]
const FAKE_AGENT: &str = env!("CARGO_BIN_EXE_skillmount-fake-agent");

/// A fixture holding a project, a Skill source, and a private home directory.
///
/// The home directory is redirected so a Claude staging plan resolves inside the fixture. That
/// makes "the session root was not created" an assertion this suite can actually make, instead of
/// a claim about the developer's real home.
struct Fixture {
    root: PathBuf,
    project: PathBuf,
    sources: PathBuf,
    home: PathBuf,
    launch_sentinel: PathBuf,
    agent_bin: PathBuf,
}

impl Fixture {
    fn new(label: &str) -> Self {
        let nonce = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .expect("clock after epoch")
            .as_nanos();
        let root = std::env::temp_dir().join(format!(
            "skillmount-readonly-{label}-{}-{nonce}",
            std::process::id()
        ));
        let project = root.join("project");
        let sources = root.join("sources");
        let home = root.join("home");
        for path in [&project, &sources, &home, &root.join("codex-home")] {
            fs::create_dir_all(path).expect("fixture directory");
        }
        // OMP's Skill-root ancestor walk stops at the nearest directory holding a `.git` entry
        // (`capability/fs.ts:84-95`), so an unanchored project walks to the filesystem root and
        // lets a directory above the shared temporary root decide what the fixture discovers.
        fs::create_dir(project.join(".git")).expect("project repository anchor");
        let launch_sentinel = root.join("agent-was-launched");
        let agent_bin = fake_agent_executable(&root, &launch_sentinel);
        Self {
            root,
            project,
            sources,
            home,
            launch_sentinel,
            agent_bin,
        }
    }

    fn skill(&self, name: &str) -> PathBuf {
        let path = self.sources.join(name);
        fs::create_dir_all(&path).expect("skill directory");
        fs::write(
            path.join("SKILL.md"),
            format!("---\nname: {name}\ndescription: {name} description\n---\n"),
        )
        .expect("SKILL.md");
        path
    }

    fn command(&self, arguments: &[&str]) -> Command {
        self.command_for(ASM, arguments)
    }

    fn command_for(&self, binary: &str, arguments: &[&str]) -> Command {
        let mut command = Command::new(binary);
        command
            .current_dir(&self.project)
            .args(arguments)
            .env("HOME", &self.home)
            .env("USERPROFILE", &self.home)
            .env("SKILLMOUNT_TEST_CODEX_USER_HOME", &self.home)
            .env("SKILLMOUNT_TEST_CODEX_MANAGED_CONFIG", "absent")
            .env("LOCALAPPDATA", self.home.join("AppData/Local"))
            .env("CODEX_HOME", self.root.join("codex-home"))
            .env("SKILLMOUNT_TEST_CODEX_VERSION", "codex-cli 0.146.0")
            .env("CLAUDE_CONFIG_DIR", self.home.join(".claude"))
            .env(
                "SKILLMOUNT_CLAUDE_MANAGED_SKILLS_DIR",
                self.root.join("claude-managed/skills"),
            )
            .env_remove("CLAUDE_CODE_SAFE_MODE")
            .env_remove("CLAUDE_CODE_SIMPLE")
            .env("SKILLMOUNT_TEST_OMP_VERSION", "omp/17.2.9")
            // OMP resolves its roots from the environment, so the developer's real profile,
            // configuration overlay, and XDG bases must never reach a fixture.
            .env_remove("OMP_PROFILE")
            .env_remove("PI_PROFILE")
            .env_remove("PI_CONFIG_FILES")
            .env_remove("PI_CONFIG_DIR")
            .env_remove("PI_CODING_AGENT_DIR")
            .env_remove("XDG_DATA_HOME")
            .env("SKILLMOUNT_STATE_DIR", self.root.join("state"))
            .env(
                "SKILLMOUNT_CODEX_ADMIN_SKILLS_DIR",
                self.root.join("admin-skills"),
            );
        command
    }

    fn run(&self, arguments: &[&str]) -> Output {
        self.command(arguments).output().expect("asm should run")
    }

    /// Roots that must be byte-identical before and after a read-only command.
    fn watched(&self) -> Vec<PathBuf> {
        vec![
            self.project.clone(),
            self.sources.clone(),
            self.home.clone(),
        ]
    }

    /// Roots holding data that belongs to the operator rather than to `SkillMount`.
    ///
    /// A mutating session legitimately creates `SkillMount`'s own state directories — journals and
    /// lock files — before it can decide whether it may proceed. Those are not the operator's data,
    /// so a test about a failing session watches only the project and the Skill sources.
    fn user_data(&self) -> Vec<PathBuf> {
        vec![self.project.clone(), self.sources.clone()]
    }

    fn assert_unchanged(&self, arguments: &[&str]) -> Output {
        self.assert_roots_unchanged(&self.watched(), arguments)
    }

    fn assert_roots_unchanged(&self, roots: &[PathBuf], arguments: &[&str]) -> Output {
        self.assert_command_unchanged(roots, self.command(arguments), &arguments.join(" "))
    }

    /// The same guarantee for a command the caller had to configure itself.
    fn assert_command_unchanged(
        &self,
        roots: &[PathBuf],
        mut command: Command,
        label: &str,
    ) -> Output {
        let before = roots
            .iter()
            .map(|root| (root.clone(), snapshot(root)))
            .collect::<Vec<_>>();

        let output = command.output().expect("asm should run");

        for (root, expected) in before {
            assert_eq!(
                snapshot(&root),
                expected,
                "`asm {label}` modified {}",
                root.display()
            );
        }
        assert!(
            !self.launch_sentinel.exists(),
            "`asm {label}` launched a child process"
        );
        output
    }

    /// Asserts an inspection changed nothing, with every Agent name resolving inside the fixture.
    ///
    /// `inspect` takes no `--agent-bin`, so a fixture-owned `PATH` is the only way to hand an
    /// inspection a launchable Agent. Without one nothing can write `launch_sentinel`, so "launched
    /// no child" would assert nothing, and the developer's own installed Agent would be the process
    /// a regression actually started.
    fn assert_inspection_unchanged(&self, arguments: &[&str]) -> Output {
        let mut command = self.command(arguments);
        command.env("PATH", self.agent_search_path());
        self.assert_command_unchanged(&self.watched(), command, &arguments.join(" "))
    }

    /// A search path resolving every Agent executable name to the launch sentinel.
    ///
    /// The path holds nothing else, so an `inspect` given it cannot reach the developer's installed
    /// Agents, and `fake_agent_executable` records the launch with a shell redirection rather than
    /// an external utility precisely so a fixture-only search path stays enough to write it.
    fn agent_search_path(&self) -> std::ffi::OsString {
        let bin = self.root.join("agent-path");
        fs::create_dir_all(&bin).expect("PATH fixture");
        for agent in ["codex", "claude", "omp"] {
            let executable = bin.join(format!("{agent}{}", std::env::consts::EXE_SUFFIX));
            if !executable.exists() {
                fs::copy(&self.agent_bin, &executable).expect("fixture Agent executable");
            }
        }
        std::env::join_paths([&bin]).expect("PATH fixture encoding")
    }
}

impl Drop for Fixture {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.root);
    }
}

fn assert_access_aware_lock_plan(rendered: &str) {
    let has = |access: &str, kind: &str| {
        rendered.lines().any(|line| {
            let mut fields = line.split_whitespace();
            fields.next() == Some(access) && fields.next() == Some(kind)
        })
    };
    assert!(
        has("observe", "discovery-entry"),
        "the read-only plan must label observed discovery evidence: {rendered}"
    );
    assert!(
        has("mutate", "discovery-entry"),
        "the read-only plan must label its mutation-capable namespace: {rendered}"
    );
    assert!(
        has("mutate", "backing-store"),
        "the read-only plan must label its mutation-capable backing store: {rendered}"
    );
}

/// Provides a native executable for the mutation-boundary launch sentinel.
fn fake_agent_executable(root: &Path, sentinel: &Path) -> PathBuf {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;

        let path = root.join("fake-agent");
        // A shell redirection, not `touch`: a copy of this script placed on a fixture-only `PATH`
        // has no external utility to resolve, so the sentinel still records the launch.
        fs::write(
            &path,
            format!("#!/bin/sh\n: > \"{}\"\n", sentinel.display()),
        )
        .expect("fake agent");
        fs::set_permissions(&path, fs::Permissions::from_mode(0o755)).expect("fake agent mode");
        path
    }
    #[cfg(windows)]
    {
        let _ = (root, sentinel);
        std::env::var_os("COMSPEC")
            .map(PathBuf::from)
            .expect("Windows provides a native command interpreter")
    }
}

/// Creates a broken directory link, returning whether the host allowed it.
///
/// Windows needs Developer Mode or an elevated process, so a contributor without either still gets
/// a usable suite while `SKILLMOUNT_REQUIRE_LINKS` keeps CI from silently losing link coverage.
#[must_use]
fn create_broken_directory_link(link: &Path, missing_target: &Path) -> bool {
    #[cfg(unix)]
    let result = std::os::unix::fs::symlink(missing_target, link);
    #[cfg(windows)]
    let result = std::os::windows::fs::symlink_dir(missing_target, link);

    if let Err(error) = result {
        assert!(
            std::env::var_os("SKILLMOUNT_REQUIRE_LINKS").is_none(),
            "required broken-link fixture could not be created at {}: {error}",
            link.display()
        );
        return false;
    }
    true
}

/// Records a tree without following links, so a replaced link shows up as a difference.
fn snapshot(root: &Path) -> BTreeMap<PathBuf, String> {
    let mut entries = BTreeMap::new();
    collect(root, root, &mut entries);
    entries
}

fn collect(root: &Path, current: &Path, entries: &mut BTreeMap<PathBuf, String>) {
    let Ok(metadata) = fs::symlink_metadata(current) else {
        return;
    };
    let file_type = metadata.file_type();
    let descriptor = if file_type.is_symlink() {
        format!(
            "link -> {}",
            fs::read_link(current).map_or_else(|_| "?".into(), |t| t.display().to_string())
        )
    } else if file_type.is_dir() {
        "dir".to_owned()
    } else {
        fs::read(current).map_or_else(
            |_| "unreadable file".to_owned(),
            |bytes| format!("file {bytes:?}"),
        )
    };
    if let Ok(relative) = current.strip_prefix(root) {
        entries.insert(relative.to_path_buf(), descriptor);
    }
    if file_type.is_dir() && !file_type.is_symlink() {
        let Ok(children) = fs::read_dir(current) else {
            return;
        };
        for child in children.flatten() {
            collect(root, &child.path(), entries);
        }
    }
}

#[test]
fn completions_ignore_invalid_state_and_leave_every_sentinel_unchanged() {
    use fs4::FileExt;

    let fixture = Fixture::new("completions-invalid-state");
    let arguments = ["completions", "powershell"];
    let baseline = [ASM, SKILLMOUNT].map(|binary| {
        fixture
            .command_for(binary, &arguments)
            .output()
            .expect("baseline completion should run")
    });
    for output in &baseline {
        assert!(output.status.success());
        assert!(!output.stdout.is_empty());
        assert!(output.stderr.is_empty());
    }

    fixture.skill("alpha");
    fs::write(fixture.project.join("project-sentinel"), b"project bytes")
        .expect("project sentinel");
    fs::write(fixture.sources.join("source-sentinel"), b"source bytes").expect("source sentinel");
    let discovery = fixture.project.join(".agents/skills");
    fs::create_dir_all(&discovery).expect("discovery directory");
    // Completion generation must ignore this entry whether or not the host admits the link, so a
    // skipped fixture only narrows the state this case presents, never its expectation.
    let _ = create_broken_directory_link(
        &discovery.join("broken-skill"),
        &fixture.root.join("missing-discovery-target"),
    );

    let transactions = fixture.root.join("state/transactions");
    fs::create_dir_all(&transactions).expect("transaction directory");
    fs::write(
        transactions.join("corrupt.journal"),
        b"not a SkillMount journal\n",
    )
    .expect("corrupt journal");
    let locks = fixture.root.join("state/locks");
    fs::create_dir_all(&locks).expect("lock directory");
    let active_lock_path = locks.join("active.lock");
    let active_lock = OpenOptions::new()
        .create(true)
        .truncate(false)
        .read(true)
        .write(true)
        .open(&active_lock_path)
        .expect("active lock file");
    fs::write(&active_lock_path, b"held by read-only completion test\n").expect("lock owner");
    FileExt::lock(&active_lock).expect("active advisory lock");

    let before = snapshot(&fixture.root);
    for ((binary, expected), product) in [ASM, SKILLMOUNT]
        .into_iter()
        .zip(baseline)
        .zip(["asm", "skillmount"])
    {
        let output = fixture
            .command_for(binary, &arguments)
            .output()
            .expect("completion should ignore invalid state");
        assert!(output.status.success(), "{product}");
        assert_eq!(output.stdout, expected.stdout, "{product}");
        assert!(output.stderr.is_empty(), "{product}");
        assert_eq!(snapshot(&fixture.root), before, "{product}");
        assert!(!fixture.launch_sentinel.exists(), "{product}");
    }

    FileExt::unlock(&active_lock).expect("active advisory lock should release");
}

#[test]
fn inspect_reports_every_registered_agent_without_touching_anything() {
    let fixture = Fixture::new("inspect");
    fixture.skill("alpha");
    let sources = fixture.sources.to_string_lossy().into_owned();

    let output = fixture.assert_inspection_unchanged(&["inspect", "--skills-dir", &sources]);

    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let rendered = String::from_utf8_lossy(&output.stdout);
    assert!(rendered.contains("Agent:          codex"));
    assert!(rendered.contains("Agent:          claude"));
    assert!(rendered.contains("Agent:          omp"));
    assert!(rendered.contains("Overlay: 1 Skill(s)"));
    assert!(rendered.contains("codex-cli 0.146.0"));
    assert!(rendered.contains("2.1.220 (Claude Code)"));
    assert!(rendered.contains("omp/17.2.9"));
    assert_eq!(rendered.matches("executable not queried").count(), 3);
    assert_access_aware_lock_plan(&rendered);
}

#[test]
fn inspect_can_be_filtered_to_omp() {
    let fixture = Fixture::new("inspect-omp");
    fixture.skill("alpha");
    let sources = fixture.sources.to_string_lossy().into_owned();

    let output = fixture.assert_inspection_unchanged(&[
        "inspect",
        "--skills-dir",
        &sources,
        "--agent",
        "omp",
    ]);

    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let rendered = String::from_utf8_lossy(&output.stdout);
    assert!(rendered.contains("Agent:          omp"), "{rendered}");
    assert!(rendered.contains("LINK"), "{rendered}");
    assert!(!rendered.contains("Agent:          codex"));
    assert!(!rendered.contains("Agent:          claude"));
}

#[test]
fn inspect_can_be_filtered_to_one_agent() {
    let fixture = Fixture::new("inspect-filter");
    fixture.skill("alpha");
    let sources = fixture.sources.to_string_lossy().into_owned();

    let output =
        fixture.assert_unchanged(&["inspect", "--skills-dir", &sources, "--agent", "claude"]);

    assert!(output.status.success());
    let rendered = String::from_utf8_lossy(&output.stdout);
    assert!(rendered.contains("Agent:          claude"));
    assert!(!rendered.contains("Agent:          codex"));
}

/// A selected session must not be decided by configuration belonging to the other Agent.
///
/// The variable names a process this run will never launch, so honouring it would report a failure
/// the operator cannot act on. The same value must still be fatal when its own Agent is selected,
/// which is what keeps this isolation rather than a weakened check.
#[test]
fn a_selected_claude_session_ignores_codex_configuration_that_fails_codex() {
    let fixture = Fixture::new("isolate-codex-home");
    fixture.skill("alpha");
    let sources = fixture.sources.to_string_lossy().into_owned();
    let missing = fixture.root.join("no-such-codex-home");

    let claude = fixture
        .command(&["claude", "--dry-run", "--skills-dir", &sources])
        .env("CODEX_HOME", &missing)
        .output()
        .expect("asm should run");
    assert!(
        claude.status.success(),
        "Claude must ignore CODEX_HOME: {}",
        String::from_utf8_lossy(&claude.stderr)
    );

    let codex = fixture
        .command(&[
            "codex",
            "--dry-run",
            "--skills-dir",
            &sources,
            "--",
            "exec",
            "fixture",
        ])
        .env("CODEX_HOME", &missing)
        .output()
        .expect("asm should run");
    assert_eq!(
        codex.status.code(),
        Some(66),
        "Codex must still reject its own unusable configuration: {}",
        String::from_utf8_lossy(&codex.stderr)
    );
    assert!(
        String::from_utf8_lossy(&codex.stderr).contains("CODEX_HOME"),
        "the selected Agent's diagnostic must name the variable"
    );
    assert_no_read_only_residue(&fixture);
}

#[cfg(windows)]
#[test]
fn a_selected_codex_session_ignores_a_drive_relative_claude_config_dir() {
    let fixture = Fixture::new("isolate-claude-config");
    fixture.skill("alpha");
    let sources = fixture.sources.to_string_lossy().into_owned();

    let codex = fixture
        .command(&[
            "codex",
            "--dry-run",
            "--skills-dir",
            &sources,
            "--",
            "exec",
            "fixture",
        ])
        .env("CLAUDE_CONFIG_DIR", "C:relative-claude")
        .output()
        .expect("asm should run");
    assert!(
        codex.status.success(),
        "Codex must ignore CLAUDE_CONFIG_DIR: {}",
        String::from_utf8_lossy(&codex.stderr)
    );

    let claude = fixture
        .command(&["claude", "--dry-run", "--skills-dir", &sources])
        .env("CLAUDE_CONFIG_DIR", "C:relative-claude")
        .output()
        .expect("asm should run");
    assert_eq!(
        claude.status.code(),
        Some(64),
        "Claude must still reject an ambiguous drive-relative root: {}",
        String::from_utf8_lossy(&claude.stderr)
    );
    assert_no_read_only_residue(&fixture);
}

/// Proves isolation without weakening the selected Agent's platform-native handling.
///
/// The relocated Claude user root holds a conflicting `alpha`, so Claude must fail on it while
/// Codex succeeds — the value is honoured verbatim for its own Agent and never read for the other.
// macOS rejects a non-UTF-8 filename outright, so the platform-native case is asserted where the
// filesystem admits one. The Windows drive-relative case above covers that platform's own spelling.
#[cfg(target_os = "linux")]
#[test]
fn a_non_unicode_claude_config_dir_reaches_claude_only() {
    use std::ffi::OsString;
    use std::os::unix::ffi::OsStringExt;

    let fixture = Fixture::new("isolate-non-unicode-claude");
    fixture.skill("alpha");
    let sources = fixture.sources.to_string_lossy().into_owned();

    let mut relocated = fixture.root.clone().into_os_string().into_vec();
    relocated.extend_from_slice(b"/claude-\xFF");
    let relocated = PathBuf::from(OsString::from_vec(relocated));
    let existing = relocated.join("skills/alpha");
    fs::create_dir_all(&existing).expect("relocated Claude user scope");
    fs::write(
        existing.join("SKILL.md"),
        "---\nname: alpha\ndescription: relocated user alpha\n---\n",
    )
    .expect("relocated SKILL.md");

    let codex = fixture
        .command(&[
            "codex",
            "--dry-run",
            "--skills-dir",
            &sources,
            "--",
            "exec",
            "fixture",
        ])
        .env("CLAUDE_CONFIG_DIR", &relocated)
        .output()
        .expect("asm should run");
    assert!(
        codex.status.success(),
        "Codex must never read CLAUDE_CONFIG_DIR: {}",
        String::from_utf8_lossy(&codex.stderr)
    );

    let claude = fixture
        .command(&["claude", "--dry-run", "--skills-dir", &sources])
        .env("CLAUDE_CONFIG_DIR", &relocated)
        .output()
        .expect("asm should run");
    assert_eq!(
        claude.status.code(),
        Some(73),
        "Claude must honour the non-Unicode root it was given: {}",
        String::from_utf8_lossy(&claude.stderr)
    );
    assert_no_read_only_residue(&fixture);
}

/// Asserts that a read-only path created no `SkillMount` state and launched no child.
///
/// A dry run resolves configuration, so a test that changes an Agent variable must still prove the
/// resolution stayed read-only rather than only checking the exit code.
fn assert_no_read_only_residue(fixture: &Fixture) {
    assert!(
        !fixture.root.join("state").exists(),
        "a read-only path must not create SkillMount state"
    );
    assert!(
        !fixture.launch_sentinel.exists(),
        "a read-only path must not launch a child process"
    );
    assert!(
        !fixture.project.join(".agents").exists(),
        "a read-only path must not create a destination directory"
    );
    assert!(
        !fixture.project.join(".claude").exists(),
        "a read-only path must not create a destination directory"
    );
}

#[cfg(feature = "test-fixtures")]
#[test]
fn inspect_and_both_dry_runs_launch_neither_version_observation_nor_child_process() {
    let fixture = Fixture::new("all-read-only-process-free");
    fixture.skill("alpha");
    let sources = fixture.sources.to_string_lossy().into_owned();
    let fake_agent = Path::new(FAKE_AGENT).to_string_lossy().into_owned();
    let version_record = fixture.root.join("version-process.record");
    let child_record = fixture.root.join("child-process.record");
    let bin = fixture.root.join("agent-bin");
    fs::create_dir(&bin).expect("PATH fixture");
    for name in ["codex", "claude"] {
        fs::copy(
            FAKE_AGENT,
            bin.join(format!("{name}{}", std::env::consts::EXE_SUFFIX)),
        )
        .expect("copy native fake Agent");
    }
    let search_path = std::env::join_paths([&bin]).expect("PATH fixture encoding");

    let mut commands = [
        fixture.command(&["inspect", "--skills-dir", &sources]),
        fixture.command(&[
            "codex",
            "--skills-dir",
            &sources,
            "--agent-bin",
            &fake_agent,
            "--dry-run",
            "--",
            "exec",
            "fixture",
        ]),
        fixture.command(&[
            "claude",
            "--skills-dir",
            &sources,
            "--agent-bin",
            &fake_agent,
            "--dry-run",
        ]),
    ];
    for (index, command) in commands.iter_mut().enumerate() {
        let before = snapshot(&fixture.root);
        let output = command
            .env("PATH", &search_path)
            .env_remove("SKILLMOUNT_TEST_CODEX_VERSION")
            .env_remove("SKILLMOUNT_TEST_CLAUDE_VERSION")
            .env("SKILLMOUNT_FAKE_VERSION_RECORD", &version_record)
            .env("SKILLMOUNT_FAKE_RECORD", &child_record)
            .output()
            .expect("read-only command should run");

        assert!(
            output.status.success(),
            "read-only case {index}: {}",
            String::from_utf8_lossy(&output.stderr)
        );
        assert_eq!(
            snapshot(&fixture.root),
            before,
            "read-only case {index} changed the fixture"
        );
        assert!(
            !version_record.exists(),
            "read-only case {index} launched --version"
        );
        assert!(
            !child_record.exists(),
            "read-only case {index} launched the Agent child"
        );
        let rendered = String::from_utf8_lossy(&output.stdout);
        assert!(rendered.contains("advisory evidence; executable not queried"));
    }
}

#[test]
fn a_codex_dry_run_plans_the_whole_layout_without_creating_it() {
    let fixture = Fixture::new("dry-run-codex");
    fixture.skill("alpha");
    let sources = fixture.sources.to_string_lossy().into_owned();

    let output = fixture.assert_unchanged(&[
        "codex",
        "--skills-dir",
        &sources,
        "--dry-run",
        "--",
        "exec",
        "fixture",
    ]);

    assert!(output.status.success());
    let rendered = String::from_utf8_lossy(&output.stdout);
    assert!(rendered.contains("MKDIR  .agents"));
    assert!(
        rendered.contains("MKDIR  .agents/skills") || rendered.contains("MKDIR  .agents\\skills")
    );
    assert!(
        rendered.contains("LINK   .agents/skills/alpha")
            || rendered.contains("LINK   .agents\\skills\\alpha")
    );
    assert!(!fixture.project.join(".codex").exists());
    assert!(!fixture.project.join(".agents").exists());
    assert!(rendered.contains("codex-cli 0.146.0"));
    assert!(rendered.contains("advisory evidence; executable not queried"));
    assert_access_aware_lock_plan(&rendered);
}

/// A verbose read-only plan says which entries a later cleanup is obliged to reconcile.
///
/// The disposition is part of the plan an operator reads before anything is mutated: a created Skill
/// link is cleanup-critical, the discovery chain beneath it is scaffolding a later pass may leave
/// behind. Rendering it must still create nothing.
#[test]
fn a_verbose_dry_run_marks_only_created_skill_links_as_cleanup_critical() {
    let fixture = Fixture::new("dry-run-disposition");
    fixture.skill("alpha");
    let sources = fixture.sources.to_string_lossy().into_owned();

    let output = fixture.assert_unchanged(&[
        "codex",
        "--skills-dir",
        &sources,
        "--dry-run",
        "--verbose",
        "--",
        "exec",
        "fixture",
    ]);

    assert!(output.status.success());
    let rendered = String::from_utf8_lossy(&output.stdout);
    // Every action carries a disposition, and the counts pin which kind gets which: the observed
    // plan is `MKDIR .agents`, `MKDIR .agents/skills`, then the mounted Skill link, so exactly one
    // action is cleanup-critical and the two directories beneath it are scaffolding.
    assert_eq!(
        rendered.matches("cleanup=cleanup-critical").count(),
        1,
        "{rendered}"
    );
    assert_eq!(
        rendered.matches("cleanup=scaffolding").count(),
        2,
        "{rendered}"
    );
    // The directories are established before the link that needs them, so the first disposition the
    // plan reports is scaffolding.
    assert!(
        rendered.find("cleanup=scaffolding") < rendered.find("cleanup=cleanup-critical"),
        "{rendered}"
    );
    assert!(!fixture.project.join(".agents").exists());
    assert!(!fixture.project.join(".codex").exists());
}

#[test]
fn codex_rejects_every_plugin_namespace_spelling_above_a_selected_source() {
    for manifest_directory in [".codex-plugin", ".claude-plugin", ".cursor-plugin"] {
        let fixture = Fixture::new(manifest_directory);
        fixture.skill("alpha");
        let manifest = fixture.sources.join(manifest_directory).join("plugin.json");
        fs::create_dir_all(manifest.parent().expect("manifest parent"))
            .expect("manifest directory");
        fs::write(&manifest, r#"{"name":"fixture-plugin"}"#).expect("plugin manifest");
        let canonical_manifest = fs::canonicalize(&manifest).expect("canonical plugin manifest");
        let sources = fixture.sources.to_string_lossy().into_owned();

        let output = fixture.assert_unchanged(&[
            "codex",
            "--skills-dir",
            &sources,
            "--dry-run",
            "--",
            "exec",
            "fixture",
        ]);
        let stderr = String::from_utf8_lossy(&output.stderr);

        assert_eq!(
            output.status.code(),
            Some(73),
            "{manifest_directory}: {stderr}"
        );
        assert!(stderr.contains("namespace-qualify"), "{stderr}");
        assert!(
            stderr.contains(canonical_manifest.to_string_lossy().as_ref()),
            "{stderr}"
        );
    }
}

#[test]
fn an_invalid_higher_precedence_plugin_manifest_masks_lower_spellings_like_codex() {
    let fixture = Fixture::new("invalid-plugin-manifest-precedence");
    fixture.skill("alpha");
    let invalid = fixture.sources.join(".codex-plugin/plugin.json");
    let valid_lower = fixture.sources.join(".claude-plugin/plugin.json");
    fs::create_dir_all(invalid.parent().expect("invalid manifest parent"))
        .expect("invalid manifest directory");
    fs::create_dir_all(valid_lower.parent().expect("valid manifest parent"))
        .expect("valid manifest directory");
    fs::write(invalid, r#"{"name":42}"#).expect("invalid higher-precedence manifest");
    fs::write(valid_lower, r#"{"name":"ignored-plugin"}"#)
        .expect("valid lower-precedence manifest");
    let sources = fixture.sources.to_string_lossy().into_owned();

    let output = fixture.assert_unchanged(&[
        "codex",
        "--skills-dir",
        &sources,
        "--dry-run",
        "--",
        "exec",
        "fixture",
    ]);

    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
}

#[test]
fn an_oversized_plugin_manifest_fails_closed_without_launching_codex() {
    let fixture = Fixture::new("oversized-plugin-manifest");
    fixture.skill("alpha");
    let manifest = fixture.sources.join(".codex-plugin/plugin.json");
    fs::create_dir_all(manifest.parent().expect("manifest parent")).expect("manifest directory");
    let contents = format!(
        r#"{{"padding":"{}","name":"fixture-plugin"}}"#,
        "a".repeat(64 * 1024)
    );
    fs::write(&manifest, contents).expect("oversized plugin manifest");
    let sources = fixture.sources.to_string_lossy().into_owned();

    let output = fixture.assert_unchanged(&[
        "codex",
        "--skills-dir",
        &sources,
        "--dry-run",
        "--",
        "exec",
        "fixture",
    ]);
    let stderr = String::from_utf8_lossy(&output.stderr);

    assert_eq!(output.status.code(), Some(73), "{stderr}");
    assert!(stderr.contains("exceeds 65536 bytes"), "{stderr}");
}

// APFS rejects an unpaired native filename before the adapter can observe it. Linux and NTFS
// permit the respective byte/WTF-16 spellings, so exercise the shipped boundary there; the
// platform-independent predicate also has unit coverage on every Unix and Windows target.
#[cfg(any(target_os = "linux", windows))]
#[test]
fn a_non_unicode_codex_directory_entry_fails_closed_without_launching_codex() {
    let fixture = Fixture::new("non-unicode-codex-entry");
    fixture.skill("alpha");
    let discovery = fixture.project.join(".agents/skills");
    fs::create_dir_all(&discovery).expect("Codex discovery root");
    fs::create_dir(discovery.join(non_unicode_entry_name()))
        .expect("native non-Unicode directory entry");
    let sources = fixture.sources.to_string_lossy().into_owned();

    let output = fixture.assert_unchanged(&[
        "codex",
        "--skills-dir",
        &sources,
        "--dry-run",
        "--",
        "exec",
        "fixture",
    ]);
    let stderr = String::from_utf8_lossy(&output.stderr);

    assert_eq!(output.status.code(), Some(73), "{stderr}");
    assert!(
        stderr.contains("non-Unicode directory-entry name"),
        "{stderr}"
    );
}

#[cfg(target_os = "linux")]
fn non_unicode_entry_name() -> std::ffi::OsString {
    use std::os::unix::ffi::OsStringExt as _;

    std::ffi::OsString::from_vec(vec![b's', b'k', b'i', b'l', b'l', 0xff])
}

#[cfg(windows)]
fn non_unicode_entry_name() -> std::ffi::OsString {
    use std::os::windows::ffi::OsStringExt as _;

    std::ffi::OsString::from_wide(&[
        u16::from(b's'),
        u16::from(b'k'),
        u16::from(b'i'),
        u16::from(b'l'),
        u16::from(b'l'),
        0xd800,
    ])
}

#[test]
fn a_claude_dry_run_never_creates_a_session_root() {
    let fixture = Fixture::new("dry-run-claude");
    fixture.skill("alpha");
    let sources = fixture.sources.to_string_lossy().into_owned();

    let output = fixture.assert_unchanged(&["claude", "--skills-dir", &sources, "--dry-run"]);

    assert!(output.status.success());
    let rendered = String::from_utf8_lossy(&output.stdout);
    assert!(rendered.contains("<session-id>"), "{rendered}");
    assert!(rendered.contains("argv[1] = --add-dir"));
    assert!(
        !fixture.home.join("Library/Caches/skillmount").exists()
            && !fixture.home.join("AppData/Local/skillmount").exists(),
        "no session or transaction storage may be created"
    );
    assert!(rendered.contains("2.1.220 (Claude Code)"));
    assert!(rendered.contains("advisory evidence; executable not queried"));
    assert_access_aware_lock_plan(&rendered);
}

#[test]
fn a_destination_conflict_fails_without_changing_the_project() {
    let fixture = Fixture::new("conflict");
    fixture.skill("alpha");
    fs::create_dir_all(fixture.project.join(".agents/skills/alpha")).expect("conflicting entry");
    let sources = fixture.sources.to_string_lossy().into_owned();

    let output = fixture.assert_unchanged(&[
        "codex",
        "--skills-dir",
        &sources,
        "--dry-run",
        "--",
        "exec",
        "fixture",
    ]);

    assert_eq!(
        output.status.code(),
        Some(73),
        "a destination conflict is a filesystem-state failure"
    );
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(stderr.contains("conflicts with"), "{stderr}");
    assert!(stderr.contains("was not replaced"), "{stderr}");
    assert!(stderr.contains("--conflict=skip"), "{stderr}");
}

#[test]
fn a_skip_policy_preserves_the_existing_entry_and_reports_it() {
    let fixture = Fixture::new("skip");
    fixture.skill("alpha");
    fs::create_dir_all(fixture.project.join(".agents/skills/alpha")).expect("existing entry");
    let sources = fixture.sources.to_string_lossy().into_owned();

    let output = fixture.assert_unchanged(&[
        "codex",
        "--skills-dir",
        &sources,
        "--dry-run",
        "--conflict",
        "skip",
        "--",
        "exec",
        "fixture",
    ]);

    assert!(output.status.success());
    assert!(String::from_utf8_lossy(&output.stdout).contains("KEEP"));
}

#[test]
fn input_and_catalog_failures_keep_their_stable_codes_and_change_nothing() {
    let fixture = Fixture::new("failures");
    let missing = fixture.root.join("absent");
    let missing_output =
        fixture.assert_unchanged(&["inspect", "--skills-dir", &missing.to_string_lossy()]);
    assert_eq!(missing_output.status.code(), Some(66));

    let invalid = fixture.root.join("invalid");
    fs::create_dir_all(&invalid).expect("invalid fixture");
    fs::write(invalid.join("SKILL.md"), "no frontmatter\n").expect("invalid SKILL.md");
    let invalid_output =
        fixture.assert_unchanged(&["inspect", "--skills-dir", &invalid.to_string_lossy()]);
    assert_eq!(invalid_output.status.code(), Some(65));
}

#[test]
fn a_skill_disabling_claude_argument_is_rejected_before_planning() {
    let fixture = Fixture::new("claude-rejected-args");
    fixture.skill("alpha");
    let sources = fixture.sources.to_string_lossy().into_owned();

    let output = fixture.assert_unchanged(&[
        "claude",
        "--skills-dir",
        &sources,
        "--dry-run",
        "--",
        "--bare",
    ]);

    assert_eq!(output.status.code(), Some(64));
    assert!(String::from_utf8_lossy(&output.stderr).contains("--bare"));
}

#[test]
fn codex_root_changing_arguments_are_rejected_before_discovery_can_diverge() {
    let fixture = Fixture::new("codex-rejected-root-args");
    fixture.skill("alpha");
    let sources = fixture.sources.to_string_lossy().into_owned();

    for forwarded in [
        vec!["-C", "other"],
        vec!["-Cother"],
        vec!["--cd", "other"],
        vec!["--cd=other"],
    ] {
        let mut arguments = vec!["codex", "--skills-dir", &sources, "--dry-run", "--"];
        arguments.extend(forwarded.iter().copied());

        let output = fixture.assert_unchanged(&arguments);
        let stderr = String::from_utf8_lossy(&output.stderr);

        assert_eq!(output.status.code(), Some(64), "{forwarded:?}: {stderr}");
        assert!(
            stderr.contains("changes the child discovery root"),
            "{stderr}"
        );
        assert!(stderr.contains("--cwd"), "{stderr}");
    }
}

#[test]
fn mutating_codex_rejects_root_changing_arguments_before_creating_state() {
    let fixture = Fixture::new("codex-rejected-root-args-mutation");
    fixture.skill("alpha");
    let sources = fixture.sources.to_string_lossy().into_owned();
    let agent_bin = fixture.agent_bin.to_string_lossy().into_owned();

    let output = fixture.assert_unchanged(&[
        "codex",
        "--skills-dir",
        &sources,
        "--agent-bin",
        &agent_bin,
        "--",
        "-Cother",
    ]);

    assert_eq!(output.status.code(), Some(64));
    assert!(String::from_utf8_lossy(&output.stderr).contains("changes the child discovery root"));
    assert!(
        !fixture.root.join("state").exists(),
        "argument validation must precede lock and journal storage"
    );
}

#[test]
fn codex_remote_arguments_are_rejected_before_local_discovery_can_diverge() {
    let fixture = Fixture::new("codex-rejected-remote-args");
    fixture.skill("alpha");
    let sources = fixture.sources.to_string_lossy().into_owned();

    for forwarded in [
        vec!["--remote", "ws://127.0.0.1:9"],
        vec!["--remote=ws://127.0.0.1:9"],
        vec!["--remote-auth-token-env", "CODEX_REMOTE_TOKEN"],
        vec!["--remote-auth-token-env=CODEX_REMOTE_TOKEN"],
    ] {
        let mut arguments = vec!["codex", "--skills-dir", &sources, "--dry-run", "--"];
        arguments.extend(forwarded.iter().copied());

        let output = fixture.assert_unchanged(&arguments);
        let stderr = String::from_utf8_lossy(&output.stderr);

        assert_eq!(output.status.code(), Some(64), "{forwarded:?}: {stderr}");
        assert!(stderr.contains("remote app server"), "{stderr}");
    }
}

#[test]
fn mutating_codex_rejects_remote_before_creating_state() {
    let fixture = Fixture::new("codex-rejected-remote-mutation");
    fixture.skill("alpha");
    let sources = fixture.sources.to_string_lossy().into_owned();
    let agent_bin = fixture.agent_bin.to_string_lossy().into_owned();

    let output = fixture.assert_unchanged(&[
        "codex",
        "--skills-dir",
        &sources,
        "--agent-bin",
        &agent_bin,
        "--",
        "--remote=ws://127.0.0.1:9",
    ]);

    assert_eq!(output.status.code(), Some(64));
    assert!(String::from_utf8_lossy(&output.stderr).contains("remote app server"));
    assert!(
        !fixture.root.join("state").exists(),
        "remote validation must precede lock and journal storage"
    );
}

#[test]
fn codex_config_profile_and_resume_arguments_are_rejected() {
    let fixture = Fixture::new("codex-rejected-discovery-contract-args");
    fixture.skill("alpha");
    let sources = fixture.sources.to_string_lossy().into_owned();

    for forwarded in [
        vec!["-c", "project_root_markers=[]"],
        vec!["-cskills.bundled.enabled=false"],
        vec!["--config", "skills.bundled.enabled=false"],
        vec!["--config=skills.bundled.enabled=false"],
        vec!["-p", "alternate"],
        vec!["-palternate"],
        vec!["--profile", "alternate"],
        vec!["--profile=alternate"],
        vec!["--enable", "plugins"],
        vec!["--enable=plugins"],
        vec!["--disable", "plugins"],
        vec!["--disable=plugins"],
        vec!["exec", "--ignore-user-config", "prompt"],
        vec!["resume", "session-id"],
        vec!["fork", "session-id"],
        vec!["exec", "resume", "session-id"],
        vec!["exec", "--color", "always", "resume", "session-id"],
        vec!["exec", "--image=fixture.png", "resume", "session-id"],
        vec!["exec", "-ifixture.png", "resume", "session-id"],
    ] {
        let mut arguments = vec!["codex", "--skills-dir", &sources, "--dry-run", "--"];
        arguments.extend(forwarded.iter().copied());

        let output = fixture.assert_unchanged(&arguments);
        let stderr = String::from_utf8_lossy(&output.stderr);

        assert_eq!(output.status.code(), Some(64), "{forwarded:?}: {stderr}");
        assert!(
            stderr.contains("discovery contract") || stderr.contains("discovery CWD"),
            "{forwarded:?}: {stderr}"
        );
    }
}

#[test]
fn command_shaped_option_values_and_prompts_are_not_misclassified() {
    let fixture = Fixture::new("codex-command-shaped-values");
    fixture.skill("alpha");
    let sources = fixture.sources.to_string_lossy().into_owned();

    for forwarded in [
        vec!["review", "--base", "resume"],
        vec!["review", "resume"],
        vec!["exec", "--output-schema", "login"],
        vec!["-m", "doctor", "review", "resume"],
        vec!["--image=fixture.png", "exec", "prompt"],
        vec!["-ifixture.png", "exec", "prompt"],
    ] {
        let mut arguments = vec!["codex", "--skills-dir", &sources, "--dry-run", "--"];
        arguments.extend(forwarded.iter().copied());
        let output = fixture.assert_unchanged(&arguments);
        assert!(
            output.status.success(),
            "{forwarded:?}: {}",
            String::from_utf8_lossy(&output.stderr)
        );
    }
}

#[test]
fn bare_variadic_images_are_rejected_before_they_can_hide_a_nested_resume() {
    let fixture = Fixture::new("codex-rejected-bare-image");
    fixture.skill("alpha");
    let sources = fixture.sources.to_string_lossy().into_owned();

    for forwarded in [
        vec!["exec", "-i", "foo", "--json", "resume", "--help"],
        vec!["exec", "--image", "foo", "--ephemeral", "resume", "--help"],
        vec!["exec", "-i", "foo", "-m", "gpt-5.2", "resume", "--help"],
        vec!["exec", "-i", "foo"],
        vec!["-i", "foo", "exec", "prompt"],
    ] {
        let mut arguments = vec!["codex", "--skills-dir", &sources, "--dry-run", "--"];
        arguments.extend(forwarded.iter().copied());
        let output = fixture.assert_unchanged(&arguments);
        let stderr = String::from_utf8_lossy(&output.stderr);

        assert_eq!(output.status.code(), Some(64), "{forwarded:?}: {stderr}");
        assert!(stderr.contains("variadic"), "{forwarded:?}: {stderr}");
        assert!(stderr.contains("--image=VALUE"), "{forwarded:?}: {stderr}");
    }
}

#[test]
fn interactive_codex_tui_modes_are_rejected_before_planning() {
    let fixture = Fixture::new("codex-rejected-interactive-tui");
    fixture.skill("alpha");
    let sources = fixture.sources.to_string_lossy().into_owned();

    for forwarded in [
        Vec::<&str>::new(),
        vec!["initial prompt"],
        vec!["-m", "fixture-model", "initial prompt"],
    ] {
        let mut arguments = vec!["codex", "--skills-dir", &sources, "--dry-run", "--"];
        arguments.extend(forwarded.iter().copied());
        let output = fixture.assert_unchanged(&arguments);
        let stderr = String::from_utf8_lossy(&output.stderr);

        assert_eq!(output.status.code(), Some(64), "{forwarded:?}: {stderr}");
        assert!(
            stderr.contains("interactive Codex TUI"),
            "{forwarded:?}: {stderr}"
        );
        assert!(!fixture.root.join("state").exists());
    }
}

#[test]
fn codex_service_and_operator_commands_are_rejected() {
    let fixture = Fixture::new("codex-rejected-non-session-command");
    fixture.skill("alpha");
    let sources = fixture.sources.to_string_lossy().into_owned();

    for command in [
        "login",
        "mcp-server",
        "app-server",
        "remote-control",
        "doctor",
        "sandbox",
        "apply",
        "archive",
        "cloud",
        "cloud-tasks",
        "exec-server",
        "execpolicy",
        "responses-api-proxy",
        "stdio-to-uds",
        "features",
        "help",
    ] {
        let output = fixture.assert_unchanged(&[
            "codex",
            "--skills-dir",
            &sources,
            "--dry-run",
            "--",
            command,
        ]);
        let stderr = String::from_utf8_lossy(&output.stderr);
        assert_eq!(output.status.code(), Some(64), "{command}: {stderr}");
        assert!(stderr.contains("single bounded"), "{command}: {stderr}");
    }

    let output = fixture.assert_unchanged(&[
        "codex",
        "--skills-dir",
        &sources,
        "--dry-run",
        "--",
        "-m",
        "doctor",
        "app-server",
    ]);
    assert_eq!(output.status.code(), Some(64));

    for forwarded in [
        vec!["exec", "help"],
        vec!["--help"],
        vec!["--version"],
        vec!["-Vattached", "exec", "prompt"],
        vec!["-hattached", "exec", "prompt"],
        vec!["exec", "-Vattached"],
        vec!["exec", "-hattached"],
    ] {
        let mut arguments = vec!["codex", "--skills-dir", &sources, "--dry-run", "--"];
        arguments.extend(forwarded.iter().copied());
        let output = fixture.assert_unchanged(&arguments);
        assert_eq!(output.status.code(), Some(64), "{forwarded:?}");
    }
}

#[test]
fn mutating_codex_rejects_higher_precedence_managed_configuration_before_state() {
    let fixture = Fixture::new("codex-rejected-managed-config");
    fixture.skill("alpha");
    let sources = fixture.sources.to_string_lossy().into_owned();
    let agent_bin = fixture.agent_bin.to_string_lossy().into_owned();
    let arguments = [
        "codex",
        "--skills-dir",
        &sources,
        "--agent-bin",
        &agent_bin,
        "--",
        "exec",
        "fixture",
    ];
    let before = snapshot(&fixture.project);

    let output = fixture
        .command(&arguments)
        .env("SKILLMOUNT_TEST_CODEX_MANAGED_CONFIG", "present")
        .output()
        .expect("asm should reject managed configuration");
    let stderr = String::from_utf8_lossy(&output.stderr);

    assert_eq!(output.status.code(), Some(64), "{stderr}");
    assert!(stderr.contains("managed configuration"), "{stderr}");
    assert_eq!(snapshot(&fixture.project), before);
    assert!(!fixture.root.join("state").exists());
    assert!(!fixture.launch_sentinel.exists());
}

#[test]
fn mutating_codex_rejects_config_overrides_before_creating_state() {
    let fixture = Fixture::new("codex-rejected-config-mutation");
    fixture.skill("alpha");
    let sources = fixture.sources.to_string_lossy().into_owned();
    let agent_bin = fixture.agent_bin.to_string_lossy().into_owned();

    let output = fixture.assert_unchanged(&[
        "codex",
        "--skills-dir",
        &sources,
        "--agent-bin",
        &agent_bin,
        "--",
        "--config=skills.bundled.enabled=false",
    ]);

    assert_eq!(output.status.code(), Some(64));
    assert!(String::from_utf8_lossy(&output.stderr).contains("discovery contract"));
    assert!(
        !fixture.root.join("state").exists(),
        "config validation must precede lock and journal storage"
    );
}

#[test]
fn interactive_prompt_after_codex_option_termination_is_rejected() {
    let fixture = Fixture::new("codex-option-termination");
    fixture.skill("alpha");
    let sources = fixture.sources.to_string_lossy().into_owned();

    let output = fixture.assert_unchanged(&[
        "codex",
        "--skills-dir",
        &sources,
        "--dry-run",
        "--",
        "--",
        "-Cprompt-text",
    ]);

    assert_eq!(output.status.code(), Some(64));
    assert!(String::from_utf8_lossy(&output.stderr).contains("interactive Codex TUI"));
}

#[test]
fn a_session_that_fails_before_the_mutation_boundary_changes_nothing() {
    let fixture = Fixture::new("mutation-boundary");
    fixture.skill("alpha");
    // A destination the plan cannot resolve, so the run fails while it is still read-only. A
    // session that gets past planning is no longer a read-only path and belongs to
    // `tests/transaction.rs`; what this suite still owns is the guarantee that a failure on the
    // way there costs nothing.
    fs::create_dir_all(fixture.project.join(".agents/skills/alpha")).expect("conflicting entry");
    let sources = fixture.sources.to_string_lossy().into_owned();
    let agent_bin = fixture.agent_bin.to_string_lossy().into_owned();

    let mut arguments = vec!["codex", "--skills-dir", &sources, "--agent-bin", &agent_bin];
    #[cfg(windows)]
    let sentinel_command = format!("type nul > \"{}\"", fixture.launch_sentinel.display());
    #[cfg(windows)]
    let agent_arguments = ["--", "exec", "/d", "/c", sentinel_command.as_str()];
    #[cfg(not(windows))]
    let agent_arguments = ["--", "exec", "fixture"];
    arguments.extend(agent_arguments);

    let output = fixture.assert_roots_unchanged(&fixture.user_data(), &arguments);

    assert_eq!(output.status.code(), Some(73));
    assert!(String::from_utf8_lossy(&output.stderr).contains("conflicts with"));
}

#[test]
fn verbose_output_names_the_rightmost_winner_and_every_shadowed_origin() {
    let fixture = Fixture::new("provenance");
    let base = fixture.root.join("base-skills");
    let team = fixture.root.join("team-skills");
    for root in [&base, &team] {
        let skill = root.join("alpha");
        fs::create_dir_all(&skill).expect("source");
        fs::write(
            skill.join("SKILL.md"),
            "---\nname: alpha\ndescription: alpha description\n---\n",
        )
        .expect("SKILL.md");
    }

    let output = fixture.assert_unchanged(&[
        "codex",
        "--skills-dir",
        &base.to_string_lossy(),
        "--skills-dir",
        &team.to_string_lossy(),
        "--dry-run",
        "--verbose",
        "--",
        "exec",
    ]);

    assert!(output.status.success());
    let rendered = String::from_utf8_lossy(&output.stdout);
    assert!(
        rendered.contains("Overlay: 1 Skill(s), 1 source override(s)"),
        "{rendered}"
    );
    assert!(rendered.contains("OVERRIDE  alpha"), "{rendered}");
    assert!(
        rendered.contains("different source"),
        "the displaced origin and its reason must be listed: {rendered}"
    );
    // The selected origin is reported canonically, so the expectation is canonicalized too: a
    // temporary directory sits behind a symbolic link on macOS.
    let winner = fs::canonicalize(team.join("alpha")).expect("canonical winner");
    assert!(
        rendered.contains(&format!("-> [2] {}", winner.display())),
        "the rightmost occurrence wins: {rendered}"
    );
}

#[test]
fn an_existing_transaction_record_is_reported_and_left_alone() {
    let fixture = Fixture::new("recovery");
    fixture.skill("alpha");
    let transactions = fixture.root.join("state/transactions");
    fs::create_dir_all(&transactions).expect("transaction directory");
    let record = transactions.join("01JEXAMPLE.journal");
    fs::write(&record, "not a journal this build wrote\n").expect("transaction record");
    let sources = fixture.sources.to_string_lossy().into_owned();

    let output = fixture.assert_unchanged(&[
        "codex",
        "--skills-dir",
        &sources,
        "--dry-run",
        "--",
        "exec",
        "fixture",
    ]);

    assert!(output.status.success());
    assert!(
        String::from_utf8_lossy(&output.stdout).contains("WOULD RETAIN"),
        "a journal this build cannot interpret must be reported rather than ignored: {}",
        String::from_utf8_lossy(&output.stdout)
    );
    assert_eq!(
        fs::read_to_string(&record).expect("record still readable"),
        "not a journal this build wrote\n",
        "a dry run must not recover, rewrite, or remove a transaction journal"
    );
}

#[test]
fn passthrough_values_with_shell_metacharacters_stay_separate_indexed_values() {
    let fixture = Fixture::new("metacharacters");
    fixture.skill("alpha");
    let sources = fixture.sources.to_string_lossy().into_owned();
    let awkward = ["--flag", "a b\"c'd;e", "$(id)", "x && y", "back\\slash"];

    let mut arguments = vec![
        "codex",
        "--skills-dir",
        &sources,
        "--dry-run",
        "--verbose",
        "--",
        "exec",
    ];
    arguments.extend(awkward);
    let output = fixture.assert_unchanged(&arguments);

    assert!(output.status.success());
    let rendered = String::from_utf8_lossy(&output.stdout);
    let effective_passthrough_offset = 1 + 6; // executable plus the pinned session arguments
    for (index, value) in awkward.iter().enumerate() {
        let forwarded_index = index + 1; // `exec` is passthrough argv[0]
        assert!(
            rendered.contains(&format!("[{forwarded_index}] {value}")),
            "forwarded value {index} must appear verbatim on its own line: {rendered}"
        );
        assert!(
            rendered.contains(&format!(
                "argv[{}] = {value}",
                forwarded_index + effective_passthrough_offset
            )),
            "effective argv must index the same value: {rendered}"
        );
    }
    assert!(
        !rendered
            .lines()
            .any(|line| line.contains(awkward[0]) && line.contains(awkward[1])),
        "two forwarded values must never share a line, which is what a joined command string \
         would produce: {rendered}"
    );
}

#[test]
fn read_only_output_is_identical_across_runs() {
    let fixture = Fixture::new("deterministic");
    fixture.skill("gamma");
    fixture.skill("alpha");
    let sources = fixture.sources.to_string_lossy().into_owned();

    let first = fixture.run(&["inspect", "--skills-dir", &sources]);
    let second = fixture.run(&["inspect", "--skills-dir", &sources]);

    assert_eq!(first.stdout, second.stdout);
    assert_eq!(first.stderr, second.stderr);
}

/// Asserts that no OMP read-only path left a destination, state, or child behind.
fn assert_no_omp_residue(fixture: &Fixture) {
    assert!(
        !fixture.project.join(".omp").exists(),
        "an OMP read-only path must not create the project scope it plans"
    );
    assert!(
        !fixture.root.join("state").exists(),
        "an OMP read-only path must not create SkillMount state"
    );
    assert!(
        !fixture.launch_sentinel.exists(),
        "an OMP read-only path must not launch a child process"
    );
}

#[test]
fn an_omp_dry_run_plans_the_project_scope_without_creating_it() {
    let fixture = Fixture::new("dry-run-omp");
    let source = fixture.skill("alpha");
    let sources = fixture.sources.to_string_lossy().into_owned();
    let canonical_source = fs::canonicalize(&source).expect("canonical Skill source");
    let agent_bin = fixture.agent_bin.to_string_lossy().into_owned();

    let output = fixture.assert_unchanged(&[
        "omp",
        "--skills-dir",
        &sources,
        "--agent-bin",
        &agent_bin,
        "--dry-run",
    ]);

    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let rendered = String::from_utf8_lossy(&output.stdout);
    // The whole chain is planned in dependency order: the `.omp` scope, its `skills` directory,
    // then the Skill itself. Nothing else is created, because OMP receives no injected argument.
    assert!(rendered.contains("MKDIR  .omp\n"), "{rendered}");
    assert!(
        rendered.contains("MKDIR  .omp/skills") || rendered.contains("MKDIR  .omp\\skills"),
        "{rendered}"
    );
    assert!(
        rendered.contains("LINK   .omp/skills/alpha")
            || rendered.contains("LINK   .omp\\skills\\alpha"),
        "{rendered}"
    );
    assert!(
        rendered.contains(canonical_source.to_string_lossy().as_ref()),
        "the planned link must name the canonical source: {rendered}"
    );
    assert!(rendered.contains("omp/17.2.9"), "{rendered}");
    assert!(rendered.contains("advisory evidence; executable not queried"));
    assert_access_aware_lock_plan(&rendered);
    // A dry run records the executable an operator named and never resolves, validates, or queries
    // it, so the rendered path is the one that was passed while the banner stays advisory.
    assert!(
        rendered.contains(&format!("Executable:\n  {agent_bin}")),
        "{rendered}"
    );
    // The fixture's `.git` anchor is the repository root, so the ancestor walk contributes nothing
    // above the project. An `omp ancestor` scope here would mean the walk escaped the fixture and a
    // directory on the developer's machine is deciding what this test observes.
    assert!(
        !rendered.contains("omp ancestor"),
        "the repository anchor must keep the ancestor walk inside the fixture: {rendered}"
    );
    assert_no_omp_residue(&fixture);
}

/// A destination reached through a directory link must report its canonical target too.
///
/// The logical path and the directory the mount is actually applied to are different paths here.
/// Printing only the logical one would hide which project the mount became visible to, which is the
/// shared-backing hazard ADR 0034 records.
#[test]
fn an_omp_dry_run_reports_the_canonical_backing_of_a_linked_scope() {
    let fixture = Fixture::new("omp-linked-scope");
    fixture.skill("alpha");
    let sources = fixture.sources.to_string_lossy().into_owned();
    let agent_bin = fixture.agent_bin.to_string_lossy().into_owned();

    let shared = fixture.root.join("shared-store");
    fs::create_dir_all(&shared).expect("shared backing directory");
    let scope = fixture.project.join(".omp");
    fs::create_dir_all(&scope).expect("OMP project scope");
    if !create_broken_directory_link(&scope.join("skills"), &shared) {
        return;
    }
    let canonical_shared = fs::canonicalize(&shared).expect("canonical shared backing");

    let output = fixture.assert_unchanged(&[
        "omp",
        "--skills-dir",
        &sources,
        "--agent-bin",
        &agent_bin,
        "--dry-run",
    ]);

    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let rendered = String::from_utf8_lossy(&output.stdout);
    // The logical entry stays the path OMP reads, and the canonical backing is reported beside it.
    assert!(
        rendered.contains("Backing store:  .omp/skills")
            || rendered.contains("Backing store:  .omp\\skills"),
        "{rendered}"
    );
    assert!(
        rendered.contains("Store state:    directory link"),
        "{rendered}"
    );
    assert!(
        rendered.contains(&format!(
            "Store target:   {}",
            canonical_shared.to_string_lossy()
        )),
        "the canonical backing directory must be named: {rendered}"
    );
    // The link itself is never a planned mutation, so only the Skill is linked.
    assert!(!rendered.contains("MKDIR  .omp"), "{rendered}");
    assert!(
        rendered.contains("LINK   .omp/skills/alpha")
            || rendered.contains("LINK   .omp\\skills\\alpha"),
        "{rendered}"
    );
    assert!(
        !fixture.root.join("state").exists(),
        "a read-only path must not create SkillMount state"
    );
    assert!(
        !fixture.launch_sentinel.exists(),
        "a read-only path must not launch a child process"
    );
}

#[test]
fn both_executables_render_an_identical_omp_dry_run() {
    let fixture = Fixture::new("omp-dry-run-both-names");
    fixture.skill("alpha");
    let sources = fixture.sources.to_string_lossy().into_owned();
    let agent_bin = fixture.agent_bin.to_string_lossy().into_owned();
    let arguments = [
        "omp",
        "--skills-dir",
        &sources,
        "--agent-bin",
        &agent_bin,
        "--dry-run",
    ];

    let [asm, skillmount] = [ASM, SKILLMOUNT].map(|binary| {
        fixture
            .command_for(binary, &arguments)
            .output()
            .expect("an OMP dry run should run under either installed name")
    });

    assert!(
        asm.status.success() && skillmount.status.success(),
        "{}",
        String::from_utf8_lossy(&asm.stderr)
    );
    // The plan describes the OMP session, never the wrapper that printed it, so the installed name
    // may not leak into a single byte of it.
    assert_eq!(asm.stdout, skillmount.stdout);
    assert_eq!(asm.stderr, skillmount.stderr);
    assert_no_omp_residue(&fixture);
}

#[test]
fn omp_read_only_output_is_identical_across_runs() {
    let fixture = Fixture::new("omp-deterministic");
    fixture.skill("gamma");
    fixture.skill("alpha");
    let sources = fixture.sources.to_string_lossy().into_owned();
    let agent_bin = fixture.agent_bin.to_string_lossy().into_owned();
    let search_path = fixture.agent_search_path();

    for arguments in [
        ["inspect", "--skills-dir", &sources, "--agent", "omp"].as_slice(),
        [
            "omp",
            "--skills-dir",
            &sources,
            "--agent-bin",
            &agent_bin,
            "--dry-run",
        ]
        .as_slice(),
    ] {
        // `inspect` has no `--agent-bin`, so the fixture search path is what makes an accidental
        // child launch land on the sentinel instead of the developer's installed OMP.
        let run = || {
            fixture
                .command(arguments)
                .env("PATH", &search_path)
                .output()
                .expect("asm should run")
        };
        let first = run();
        let second = run();

        assert!(
            first.status.success(),
            "{}",
            String::from_utf8_lossy(&first.stderr)
        );
        assert_eq!(first.stdout, second.stdout, "{arguments:?}");
        assert_eq!(first.stderr, second.stderr, "{arguments:?}");
    }
    assert_no_omp_residue(&fixture);
}

#[test]
fn a_missing_omp_executable_fails_before_any_project_or_state_is_created() {
    let fixture = Fixture::new("omp-missing-executable");
    fixture.skill("alpha");
    let sources = fixture.sources.to_string_lossy().into_owned();
    let empty_bin = fixture.root.join("empty-bin");
    fs::create_dir(&empty_bin).expect("PATH fixture");
    let search_path = std::env::join_paths([&empty_bin]).expect("PATH fixture encoding");
    let before = fixture
        .watched()
        .into_iter()
        .map(|root| (root.clone(), snapshot(&root)))
        .collect::<Vec<_>>();

    // A mutating session resolves the executable, and OMP is the only Agent whose destination lives
    // in the launch CWD, so an unresolvable executable must be refused before that scope appears.
    let output = fixture
        .command(&["omp", "--skills-dir", &sources])
        .env("PATH", &search_path)
        .output()
        .expect("asm should run");
    let stderr = String::from_utf8_lossy(&output.stderr);

    assert_eq!(output.status.code(), Some(66), "{stderr}");
    assert!(stderr.contains("input omp is unavailable"), "{stderr}");
    for (root, expected) in before {
        assert_eq!(snapshot(&root), expected, "{}", root.display());
    }
    assert_no_omp_residue(&fixture);
}

/// One OMP passthrough rejection class and the tokens that must reach it.
struct OmpRejection {
    /// Tokens forwarded after `--`, in the spelling an operator would type.
    forwarded: &'static [&'static str],
    /// The offending token exactly as the diagnostic names it.
    ///
    /// A flag is reported without its value, `--mode` is reported with the selector that made it a
    /// protocol server, and a subcommand is reported quoted. Asserting this spelling is what proves
    /// the operator is told which token to remove rather than only which contract refused it.
    named: &'static str,
    /// The diagnostic fragment naming the contract that refused them.
    fragment: &'static str,
}

/// Every OMP passthrough class `SkillMount` refuses, with both value spellings where they differ.
const OMP_REJECTED_PASSTHROUGH: &[OmpRejection] = &[
    OmpRejection {
        forwarded: &["--cwd", "other"],
        named: "--cwd",
        fragment: "relocates the discovery root",
    },
    OmpRejection {
        forwarded: &["--cwd=other"],
        named: "--cwd",
        fragment: "relocates the discovery root",
    },
    OmpRejection {
        forwarded: &["--profile", "work"],
        named: "--profile",
        fragment: "relocates the discovery root",
    },
    OmpRejection {
        forwarded: &["--alias", "shortcut"],
        named: "--alias",
        fragment: "relocates the discovery root",
    },
    OmpRejection {
        forwarded: &["--config", "skills.enabled=false"],
        named: "--config",
        fragment: "relocates the discovery root",
    },
    OmpRejection {
        forwarded: &["--config=skills.enabled=false"],
        named: "--config",
        fragment: "relocates the discovery root",
    },
    OmpRejection {
        forwarded: &["--no-skills"],
        named: "--no-skills",
        fragment: "changes the Skill",
    },
    OmpRejection {
        forwarded: &["--skills", "other-skills"],
        named: "--skills",
        fragment: "changes the Skill",
    },
    OmpRejection {
        forwarded: &["-e", "extension"],
        named: "-e",
        fragment: "changes the Skill",
    },
    OmpRejection {
        forwarded: &["--extension", "extension"],
        named: "--extension",
        fragment: "changes the Skill",
    },
    OmpRejection {
        forwarded: &["--hook", "hook"],
        named: "--hook",
        fragment: "changes the Skill",
    },
    OmpRejection {
        forwarded: &["--no-extensions"],
        named: "--no-extensions",
        fragment: "changes the Skill",
    },
    OmpRejection {
        forwarded: &["--plugin-dir", "packages"],
        named: "--plugin-dir",
        fragment: "changes the Skill",
    },
    OmpRejection {
        forwarded: &["-c"],
        named: "-c",
        fragment: "resumes, forks, imports",
    },
    OmpRejection {
        forwarded: &["--continue"],
        named: "--continue",
        fragment: "resumes, forks, imports",
    },
    OmpRejection {
        forwarded: &["-r"],
        named: "-r",
        fragment: "resumes, forks, imports",
    },
    OmpRejection {
        forwarded: &["--resume"],
        named: "--resume",
        fragment: "resumes, forks, imports",
    },
    OmpRejection {
        forwarded: &["--session"],
        named: "--session",
        fragment: "resumes, forks, imports",
    },
    OmpRejection {
        forwarded: &["--fork", "01JEXAMPLE"],
        named: "--fork",
        fragment: "resumes, forks, imports",
    },
    OmpRejection {
        forwarded: &["--from-claude"],
        named: "--from-claude",
        fragment: "resumes, forks, imports",
    },
    OmpRejection {
        forwarded: &["--from-codex"],
        named: "--from-codex",
        fragment: "resumes, forks, imports",
    },
    OmpRejection {
        forwarded: &["--export", "transcript.md"],
        named: "--export",
        fragment: "resumes, forks, imports",
    },
    OmpRejection {
        forwarded: &["--mode", "rpc"],
        named: "--mode=rpc",
        fragment: "protocol server",
    },
    OmpRejection {
        forwarded: &["--mode=rpc-ui"],
        named: "--mode=rpc-ui",
        fragment: "protocol server",
    },
    OmpRejection {
        forwarded: &["--mode", "acp"],
        named: "--mode=acp",
        fragment: "protocol server",
    },
    OmpRejection {
        forwarded: &["acp"],
        named: "\"acp\"",
        fragment: "does not start a supervised foreground session",
    },
    OmpRejection {
        forwarded: &["config"],
        named: "\"config\"",
        fragment: "does not start a supervised foreground session",
    },
    OmpRejection {
        forwarded: &["plugin"],
        named: "\"plugin\"",
        fragment: "does not start a supervised foreground session",
    },
    OmpRejection {
        forwarded: &["shell"],
        named: "\"shell\"",
        fragment: "does not start a supervised foreground session",
    },
    OmpRejection {
        forwarded: &["worktree"],
        named: "\"worktree\"",
        fragment: "does not start a supervised foreground session",
    },
    OmpRejection {
        forwarded: &["wt"],
        named: "\"wt\"",
        fragment: "does not start a supervised foreground session",
    },
    OmpRejection {
        forwarded: &["q"],
        named: "\"q\"",
        fragment: "does not start a supervised foreground session",
    },
    OmpRejection {
        forwarded: &["gc"],
        named: "\"gc\"",
        fragment: "does not start a supervised foreground session",
    },
];

/// Both spellings of a session must refuse the same passthrough token at the same point.
///
/// A dry run never resolves an executable and a mutating run does, so only asserting the read-only
/// path would leave open that validation moved behind lock and journal storage on the path that can
/// actually mutate.
#[test]
fn omp_refuses_every_passthrough_class_on_the_read_only_and_the_mutating_path() {
    let fixture = Fixture::new("omp-rejected-passthrough");
    fixture.skill("alpha");
    let sources = fixture.sources.to_string_lossy().into_owned();
    let agent_bin = fixture.agent_bin.to_string_lossy().into_owned();
    let read_only = ["--dry-run", "--agent-bin", agent_bin.as_str()];
    let mutating = ["--agent-bin", agent_bin.as_str()];

    for case in OMP_REJECTED_PASSTHROUGH {
        for mode in [read_only.as_slice(), mutating.as_slice()] {
            let mut arguments = vec!["omp", "--skills-dir", &sources];
            arguments.extend(mode.iter().copied());
            arguments.push("--");
            arguments.extend(case.forwarded.iter().copied());

            let output = fixture.assert_unchanged(&arguments);
            let stderr = String::from_utf8_lossy(&output.stderr);

            assert_eq!(
                output.status.code(),
                Some(64),
                "{:?} under {mode:?}: {stderr}",
                case.forwarded
            );
            assert!(
                stderr.contains(case.named),
                "the refusal must name {} under {mode:?}: {stderr}",
                case.named
            );
            assert!(
                stderr.contains(case.fragment),
                "{:?} under {mode:?}: {stderr}",
                case.forwarded
            );
            assert_no_omp_residue(&fixture);
        }
    }
}

/// The variables that relocate every OMP root must be fatal before anything is reported.
///
/// `inspect` and `--dry-run` only describe a namespace, which is exactly why they have to refuse:
/// a relocated root would make them describe a namespace the child never reads.
#[test]
fn omp_refuses_its_root_relocating_environment_on_every_read_only_path() {
    let fixture = Fixture::new("omp-rejected-environment");
    fixture.skill("alpha");
    let sources = fixture.sources.to_string_lossy().into_owned();
    let agent_bin = fixture.agent_bin.to_string_lossy().into_owned();
    let search_path = fixture.agent_search_path();

    for variable in ["OMP_PROFILE", "PI_PROFILE", "PI_CONFIG_FILES"] {
        for arguments in [
            ["inspect", "--skills-dir", &sources, "--agent", "omp"].as_slice(),
            [
                "omp",
                "--skills-dir",
                &sources,
                "--agent-bin",
                &agent_bin,
                "--dry-run",
            ]
            .as_slice(),
        ] {
            let output = fixture
                .command(arguments)
                .env("PATH", &search_path)
                .env(variable, "relocated")
                .output()
                .expect("asm should run");
            let stderr = String::from_utf8_lossy(&output.stderr);

            assert_eq!(
                output.status.code(),
                Some(64),
                "{variable} under {arguments:?}: {stderr}"
            );
            assert!(stderr.contains(variable), "{stderr}");
            assert!(
                stderr.contains("unset it or run the agent directly"),
                "{stderr}"
            );
            assert_no_omp_residue(&fixture);
        }
    }
}

/// One Agent's refusal must not discard the reports the other Agents already produced.
///
/// The default `inspect` selection is every Agent, and OMP is the only one with an environment
/// gate inside its discovery. Propagating that refusal blanked the Codex and Claude sections too,
/// so an operator who simply exports `OMP_PROFILE` lost the whole diagnostic.
#[test]
fn one_agents_inspect_refusal_keeps_the_other_agents_reports() {
    let fixture = Fixture::new("inspect-agent-isolation");
    fixture.skill("alpha");
    let sources = fixture.sources.to_string_lossy().into_owned();
    let search_path = fixture.agent_search_path();

    for variable in ["OMP_PROFILE", "PI_PROFILE", "PI_CONFIG_FILES"] {
        let output = fixture
            .command(["inspect", "--skills-dir", &sources].as_slice())
            .env("PATH", &search_path)
            .env(variable, "relocated")
            .output()
            .expect("asm should run");
        let stdout = String::from_utf8_lossy(&output.stdout);
        let stderr = String::from_utf8_lossy(&output.stderr);

        assert!(stdout.contains("Agent:          codex"), "{stdout}");
        assert!(stdout.contains("Agent:          claude"), "{stdout}");
        assert!(
            !stdout.contains("Agent:          omp"),
            "the refusing Agent must not be reported: {stdout}"
        );
        assert!(
            stderr.contains("OMP inspection was skipped") && stderr.contains(variable),
            "the refusal must still be named: {stderr}"
        );
        // The refusal keeps its own exit category, so a script still sees the failure.
        assert_eq!(output.status.code(), Some(64), "{stderr}");
        assert_no_omp_residue(&fixture);
    }
}

#[test]
fn a_corrupt_omp_global_configuration_is_a_data_error_that_changes_nothing() {
    let fixture = Fixture::new("omp-corrupt-global-config");
    fixture.skill("alpha");
    let agent_dir = fixture.home.join(".omp/agent");
    fs::create_dir_all(&agent_dir).expect("OMP agent directory");
    fs::write(agent_dir.join("config.yml"), "skills: {enabled\n").expect("corrupt OMP config");
    let sources = fixture.sources.to_string_lossy().into_owned();
    let agent_bin = fixture.agent_bin.to_string_lossy().into_owned();

    let output = fixture.assert_unchanged(&[
        "omp",
        "--skills-dir",
        &sources,
        "--agent-bin",
        &agent_bin,
        "--dry-run",
    ]);
    let stderr = String::from_utf8_lossy(&output.stderr);

    // OMP's own global file is trusted configuration it quarantines and then throws on, so planning
    // against an empty reading of it would model the wrong namespace.
    assert_eq!(output.status.code(), Some(65), "{stderr}");
    assert!(
        stderr.contains("OMP settings input cannot be interpreted"),
        "{stderr}"
    );
    assert!(stderr.contains("config.yml"), "{stderr}");
    assert_no_omp_residue(&fixture);
}

/// A third-party provider file OMP only warns about must not fail a session OMP itself starts.
#[test]
fn a_malformed_third_party_omp_project_settings_layer_is_only_skipped() {
    let fixture = Fixture::new("omp-malformed-third-party-settings");
    fixture.skill("alpha");
    fs::create_dir_all(fixture.project.join(".cursor")).expect("third-party project scope");
    fs::write(fixture.project.join(".cursor/settings.json"), "{not json")
        .expect("malformed third-party settings");
    let sources = fixture.sources.to_string_lossy().into_owned();
    let agent_bin = fixture.agent_bin.to_string_lossy().into_owned();

    let output = fixture.assert_unchanged(&[
        "omp",
        "--skills-dir",
        &sources,
        "--agent-bin",
        &agent_bin,
        "--dry-run",
    ]);

    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let rendered = String::from_utf8_lossy(&output.stdout);
    assert!(
        rendered.contains("LINK   .omp/skills/alpha")
            || rendered.contains("LINK   .omp\\skills\\alpha"),
        "{rendered}"
    );
    assert_no_omp_residue(&fixture);
}

/// Proves the Codex project layer really is one of OMP's own settings inputs.
///
/// A `[skills] enabled = false` written for Codex disables every OMP Skill, so a mount would be
/// applied and then ignored. Refusing it is what keeps the OMP path from succeeding silently.
#[test]
fn a_codex_project_configuration_disabling_skills_fails_the_omp_plan() {
    let fixture = Fixture::new("omp-codex-project-gate");
    fixture.skill("alpha");
    fs::create_dir_all(fixture.project.join(".codex")).expect("Codex project scope");
    fs::write(
        fixture.project.join(".codex/config.toml"),
        "[skills]\nenabled = false\n",
    )
    .expect("Codex project configuration");
    let sources = fixture.sources.to_string_lossy().into_owned();
    let agent_bin = fixture.agent_bin.to_string_lossy().into_owned();

    let output = fixture.assert_unchanged(&[
        "omp",
        "--skills-dir",
        &sources,
        "--agent-bin",
        &agent_bin,
        "--dry-run",
    ]);
    let stderr = String::from_utf8_lossy(&output.stderr);

    assert_eq!(output.status.code(), Some(65), "{stderr}");
    assert!(stderr.contains("invalid selected Skill"), "{stderr}");
    assert!(stderr.contains("skills.enabled is false"), "{stderr}");
    assert_no_omp_residue(&fixture);
}

/// An unresolvable destination has no identity a later mutation could rely on.
///
/// Planning a directory over a broken link would describe a change apply must then refuse, so the
/// session fails here with the exact state instead, and the link is left exactly as it was.
#[test]
fn a_broken_omp_destination_link_fails_closed_without_mutation() {
    let fixture = Fixture::new("omp-broken-destination-link");
    fixture.skill("alpha");
    let scope = fixture.project.join(".omp");
    fs::create_dir(&scope).expect("OMP project scope");
    if !create_broken_directory_link(&scope.join("skills"), &fixture.root.join("no-such-store")) {
        return;
    }
    let sources = fixture.sources.to_string_lossy().into_owned();
    let agent_bin = fixture.agent_bin.to_string_lossy().into_owned();

    let output = fixture.assert_unchanged(&[
        "omp",
        "--skills-dir",
        &sources,
        "--agent-bin",
        &agent_bin,
        "--dry-run",
    ]);
    let stderr = String::from_utf8_lossy(&output.stderr);

    assert_eq!(output.status.code(), Some(73), "{stderr}");
    assert!(stderr.contains("resolves as broken link"), "{stderr}");
    assert!(
        stderr.contains("no safe mount destination exists"),
        "{stderr}"
    );
    assert!(
        !fixture.root.join("state").exists(),
        "reporting an unresolvable destination must not create SkillMount state"
    );
}

#[test]
fn an_existing_transaction_record_is_reported_by_omp_inspect_and_left_alone() {
    let fixture = Fixture::new("omp-recovery");
    fixture.skill("alpha");
    let transactions = fixture.root.join("state/transactions");
    fs::create_dir_all(&transactions).expect("transaction directory");
    let record = transactions.join("01JEXAMPLE.journal");
    fs::write(&record, "not a journal this build wrote\n").expect("transaction record");
    let sources = fixture.sources.to_string_lossy().into_owned();

    let output = fixture.assert_inspection_unchanged(&[
        "inspect",
        "--skills-dir",
        &sources,
        "--agent",
        "omp",
    ]);

    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(
        String::from_utf8_lossy(&output.stdout).contains("WOULD RETAIN"),
        "a journal this build cannot interpret must be reported rather than ignored: {}",
        String::from_utf8_lossy(&output.stdout)
    );
    assert_eq!(
        fs::read_to_string(&record).expect("record still readable"),
        "not a journal this build wrote\n",
        "an inspection must not recover, rewrite, or remove a transaction journal"
    );
    assert!(
        !fixture.project.join(".omp").exists(),
        "an inspection must not create the project scope it plans"
    );
}
