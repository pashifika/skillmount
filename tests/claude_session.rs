//! End-to-end Claude session acceptance through the real `asm` process.

#![cfg(feature = "test-fixtures")]

use std::collections::BTreeMap;
use std::ffi::{OsStr, OsString};
use std::fs;
use std::io::Write as _;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Output, Stdio};
use std::thread;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

const ASM: &str = env!("CARGO_BIN_EXE_asm");
const SKILLMOUNT: &str = env!("CARGO_BIN_EXE_skillmount");
const FAKE_CLAUDE: &str = env!("CARGO_BIN_EXE_skillmount-fake-agent");

struct Fixture {
    root: PathBuf,
    project: PathBuf,
    left: PathBuf,
    right: PathBuf,
    home: PathBuf,
    state: PathBuf,
    record: PathBuf,
    version_record: PathBuf,
}

impl Fixture {
    fn new(label: &str) -> Self {
        let nonce = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .expect("clock after epoch")
            .as_nanos();
        let root = std::env::temp_dir().join(format!(
            "skillmount-claude-session-{label}-{}-{nonce}",
            std::process::id()
        ));
        let fixture = Self {
            project: root.join("project"),
            left: root.join("left"),
            right: root.join("right"),
            home: root.join("home"),
            state: root.join("state"),
            record: root.join("fake-claude.record"),
            version_record: root.join("fake-claude-version.record"),
            root,
        };
        for directory in [
            &fixture.project,
            &fixture.left,
            &fixture.right,
            &fixture.home,
        ] {
            fs::create_dir_all(directory).expect("fixture directory");
        }
        fixture
    }

    fn skill(&self, source: &Path, name: &str, marker: &str) -> PathBuf {
        assert!(
            source.starts_with(&self.root),
            "fixture Skills must stay inside the isolated root"
        );
        let skill = source.join(name);
        fs::create_dir_all(&skill).expect("Skill fixture");
        fs::write(
            skill.join("SKILL.md"),
            format!("---\nname: {name}\ndescription: {name} fixture\n---\n{marker}\n"),
        )
        .expect("Skill metadata");
        skill
    }

    fn command(&self) -> Command {
        self.wrapper_command(ASM)
    }

    fn wrapper_command(&self, wrapper: &str) -> Command {
        let mut command = Command::new(wrapper);
        command.arg("claude").arg("--skills-dir").arg(&self.left);
        if fs::read_dir(&self.right)
            .expect("right source fixture")
            .next()
            .is_some()
        {
            command.arg("--skills-dir").arg(&self.right);
        }
        command
            .arg("--project-root")
            .arg(&self.project)
            .arg("--cwd")
            .arg(&self.project)
            .arg("--agent-bin")
            .arg(FAKE_CLAUDE)
            .env("HOME", &self.home)
            .env("USERPROFILE", &self.home)
            .env("CLAUDE_CONFIG_DIR", self.home.join(".claude"))
            .env(
                "SKILLMOUNT_CLAUDE_MANAGED_SKILLS_DIR",
                self.root.join("claude-managed/skills"),
            )
            .env_remove("CLAUDE_CODE_SAFE_MODE")
            .env_remove("CLAUDE_CODE_SIMPLE")
            .env("LOCALAPPDATA", self.home.join("AppData/Local"))
            .env("SKILLMOUNT_STATE_DIR", &self.state)
            .env("SKILLMOUNT_FAKE_RECORD", &self.record)
            .env("SKILLMOUNT_FAKE_VERSION_RECORD", &self.version_record)
            .env("SKILLMOUNT_FAKE_VERSION_OUTPUT", "2.1.220 (Claude Code)")
            .env(
                "SKILLMOUNT_FAKE_UNSUPPORTED_VERSION_OUTPUT",
                "2.1.221 (Claude Code)",
            )
            .env("SKILLMOUNT_FAKE_BEHAVIOR", "exit")
            .current_dir(&self.project);
        command
    }

    fn sessions(&self) -> Vec<PathBuf> {
        let mut sessions = fs::read_dir(self.state.join("sessions")).map_or_else(
            |_| Vec::new(),
            |entries| {
                entries
                    .filter_map(Result::ok)
                    .map(|entry| entry.path())
                    .collect()
            },
        );
        sessions.sort();
        sessions
    }

    fn journal_count(&self) -> usize {
        fs::read_dir(self.state.join("transactions")).map_or(0, |entries| {
            entries
                .filter_map(Result::ok)
                .filter(|entry| {
                    entry
                        .path()
                        .extension()
                        .is_some_and(|extension| extension == "journal")
                })
                .count()
        })
    }
}

impl Drop for Fixture {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.root);
    }
}

#[test]
fn three_source_overrides_count_once_normally_and_list_every_origin_verbose() {
    let fixture = Fixture::new("three-source-provenance");
    fixture.skill(&fixture.left, "alpha", "first");
    fixture.skill(&fixture.right, "alpha", "second");
    let third = fixture.root.join("third");
    fs::create_dir(&third).expect("third source");
    fixture.skill(&third, "alpha", "third winner");
    let expected_names = std::env::join_paths(["alpha"]).expect("Skill name list");

    let output = fixture
        .command()
        .arg("--skills-dir")
        .arg(&third)
        .arg("--verbose")
        .arg("--")
        .arg("prompt")
        .env("SKILLMOUNT_FAKE_EXPECT_ADD_DIR_SKILLS", expected_names)
        .output()
        .expect("three-source Claude session");

    assert_success(&output);
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        stderr.contains("Mounted 1 skill from 3 source arguments for Claude (1 source override)."),
        "{stderr}"
    );
    assert_eq!(
        stderr.matches("(different source)").count(),
        2,
        "both displaced origins remain visible: {stderr}"
    );
    assert!(
        stderr.contains("[3]"),
        "the rightmost winner is identified: {stderr}"
    );
    assert!(output.stdout.is_empty());
}

#[test]
fn selected_winners_are_visible_only_in_the_injected_root_then_cleanup_succeeds() {
    let fixture = Fixture::new("happy-path");
    fixture.skill(&fixture.left, "alpha", "left shadow");
    let beta = fixture.skill(&fixture.left, "beta", "left winner");
    let alpha = fixture.skill(&fixture.right, "alpha", "right winner");
    let project_skill = fixture.skill(&fixture.project.join(".claude/skills"), "rasen", "project");
    let user_skill = fixture.skill(&fixture.home.join(".claude/skills"), "user-only", "user");
    let extra_root = fixture.root.join("extra");
    let extra_skill = fixture.skill(&extra_root.join(".claude/skills"), "extra-only", "extra");
    let watched =
        [&fixture.project, &fixture.home, &extra_root].map(|root| (root.clone(), snapshot(root)));
    let expected_names = std::env::join_paths(["alpha", "beta"]).expect("Skill name list");
    let expected_existing = std::env::join_paths([&project_skill, &user_skill, &extra_skill])
        .expect("existing discovery paths");

    let output = fixture
        .command()
        .arg("--")
        .arg("--add-dir")
        .arg(&extra_root)
        .arg("--model")
        .arg("sonnet")
        .arg("prompt with spaces")
        .env("SKILLMOUNT_FAKE_EXPECT_ADD_DIR_SKILLS", expected_names)
        .env("SKILLMOUNT_FAKE_EXPECT_PATHS", expected_existing)
        .output()
        .expect("asm should launch fake Claude");

    assert_success(&output);
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        stderr.contains("Mounted 2 skills from 2 source arguments for Claude (1 source override)."),
        "{stderr}"
    );
    assert!(stderr.contains("  alpha\n"), "{stderr}");
    assert!(stderr.contains("  beta\n"), "{stderr}");
    assert!(stderr.contains("Launching claude..."), "{stderr}");
    assert!(output.stdout.is_empty());
    let record = fs::read_to_string(&fixture.record).expect("fake Claude launch record");
    let arguments = recorded_os_values(&record, "arg");
    assert_eq!(arguments[0], OsStr::new("--add-dir"));
    let injected = PathBuf::from(&arguments[1]);
    assert!(injected.starts_with(fixture.state.join("sessions")));
    assert_eq!(injected.file_name(), Some(OsStr::new("root")));
    assert_eq!(arguments[2], OsStr::new("--settings"));
    let visibility: serde_json::Value = serde_json::from_str(
        arguments[3]
            .to_str()
            .expect("generated settings are Unicode JSON"),
    )
    .expect("generated settings JSON");
    assert_eq!(visibility["skillOverrides"]["alpha"], "on");
    assert_eq!(visibility["skillOverrides"]["beta"], "on");
    assert_eq!(
        &arguments[4..],
        &[
            OsString::from("--add-dir"),
            extra_root.clone().into_os_string(),
            OsString::from("--model"),
            OsString::from("sonnet"),
            OsString::from("prompt with spaces"),
        ]
    );
    assert_eq!(
        canonical_recorded_values(&record, "visible-target"),
        [
            fs::canonicalize(&project_skill).expect("project-owned rasen Skill"),
            fs::canonicalize(&user_skill).expect("user Skill"),
            fs::canonicalize(&extra_skill).expect("extra-directory Skill"),
            fs::canonicalize(&alpha).expect("rightmost alpha"),
            fs::canonicalize(&beta).expect("beta winner"),
        ]
    );
    assert_eq!(
        fs::canonicalize(PathBuf::from(recorded_os_value(&record, "cwd")))
            .expect("canonical child CWD"),
        fs::canonicalize(&fixture.project).expect("canonical project")
    );
    assert!(!arguments.iter().any(|argument| {
        argument == "--permission-mode" || argument == "--dangerously-skip-permissions"
    }));
    assert_eq!(fixture.sessions(), Vec::<PathBuf>::new());
    assert_eq!(fixture.journal_count(), 0);
    assert_eq!(
        fs::read_to_string(&fixture.version_record)
            .expect("version probe record")
            .lines()
            .count(),
        3
    );
    for (root, before) in watched {
        assert_eq!(snapshot(&root), before, "{} changed", root.display());
    }
    assert!(project_skill.is_dir() && user_skill.is_dir() && extra_skill.is_dir());
}

#[test]
fn machine_readable_claude_stdout_is_not_prefixed_by_wrapper_diagnostics() {
    let fixture = Fixture::new("machine-readable-stdout");
    fixture.skill(&fixture.left, "alpha", "fixture");

    let output = fixture
        .command()
        .arg("--")
        .arg("prompt")
        .env("SKILLMOUNT_FAKE_BEHAVIOR", "json")
        .output()
        .expect("machine-readable fake Claude session");

    assert_success(&output);
    assert_eq!(output.stdout, b"{\"type\":\"fixture\"}\n");
    let parsed: serde_json::Value =
        serde_json::from_slice(&output.stdout).expect("stdout is one valid JSON value");
    assert_eq!(parsed["type"], "fixture");
    let diagnostics = String::from_utf8_lossy(&output.stderr);
    assert!(diagnostics.contains("Mounted 1 skill"), "{diagnostics}");
    assert!(diagnostics.contains("Launching claude"), "{diagnostics}");
}

#[test]
fn disabling_flags_and_unsupported_versions_fail_before_state_or_child_launch() {
    for rejected in ["--bare", "--safe-mode", "--disable-slash-commands"] {
        let fixture = Fixture::new(rejected.trim_start_matches('-'));
        fixture.skill(&fixture.left, "alpha", "fixture");
        let output = fixture
            .command()
            .arg("--")
            .arg(rejected)
            .output()
            .expect("asm should reject the disabling flag");

        assert_eq!(output.status.code(), Some(64));
        assert!(String::from_utf8_lossy(&output.stderr).contains(rejected));
        assert!(!fixture.state.exists());
        assert!(!fixture.record.exists());
    }

    for (index, (rejected, expected_name)) in [
        ("--bg", "--bg"),
        ("--background", "--background"),
        ("--worktree", "--worktree"),
        ("--worktree=review", "--worktree"),
        ("-w", "-w"),
        ("-wreview", "-w"),
        ("--tmux", "--tmux"),
        ("--tmux=classic", "--tmux"),
    ]
    .into_iter()
    .enumerate()
    {
        let fixture = Fixture::new(&format!("unsupported-lifecycle-{index}"));
        fixture.skill(&fixture.left, "alpha", "fixture");
        let output = fixture
            .command()
            .arg("--")
            .arg(rejected)
            .output()
            .expect("asm should reject detached or root-relocating sessions");

        assert_eq!(output.status.code(), Some(64));
        assert!(String::from_utf8_lossy(&output.stderr).contains(expected_name));
        assert!(!fixture.state.exists());
        assert!(!fixture.record.exists());
    }

    for (rejected, value) in [
        ("--settings", "{}"),
        ("--managed-settings", "{}"),
        ("--setting-sources", "user"),
    ] {
        let fixture = Fixture::new(rejected.trim_start_matches('-'));
        fixture.skill(&fixture.left, "alpha", "fixture");
        let output = fixture
            .command()
            .arg("--")
            .arg(rejected)
            .arg(value)
            .output()
            .expect("asm should reject passthrough visibility settings");

        assert_eq!(output.status.code(), Some(64));
        assert!(String::from_utf8_lossy(&output.stderr).contains(rejected));
        assert!(!fixture.state.exists());
        assert!(!fixture.record.exists());
    }

    let fixture = Fixture::new("safe-mode-environment");
    fixture.skill(&fixture.left, "alpha", "fixture");
    let output = fixture
        .command()
        .arg("--")
        .arg("prompt")
        .env("CLAUDE_CODE_SAFE_MODE", "1")
        .output()
        .expect("asm should reject inherited safe mode");

    assert_eq!(output.status.code(), Some(64));
    assert!(String::from_utf8_lossy(&output.stderr).contains("CLAUDE_CODE_SAFE_MODE"));
    assert!(!fixture.state.exists());
    assert!(!fixture.record.exists());

    let fixture = Fixture::new("simple-mode-environment");
    fixture.skill(&fixture.left, "alpha", "fixture");
    let output = fixture
        .command()
        .arg("--")
        .arg("prompt")
        .env("CLAUDE_CODE_SIMPLE", "1")
        .output()
        .expect("asm should reject inherited bare mode");

    assert_eq!(output.status.code(), Some(64));
    assert!(String::from_utf8_lossy(&output.stderr).contains("CLAUDE_CODE_SIMPLE"));
    assert!(!fixture.state.exists());
    assert!(!fixture.record.exists());

    let fixture = Fixture::new("unsupported-version");
    fixture.skill(&fixture.left, "alpha", "fixture");
    let output = fixture
        .command()
        .arg("--")
        .arg("prompt")
        .env("SKILLMOUNT_FAKE_VERSION_OUTPUT", "2.1.219 (Claude Code)")
        .output()
        .expect("asm should reject an unsupported Claude release");

    assert_eq!(output.status.code(), Some(64));
    assert!(String::from_utf8_lossy(&output.stderr).contains("2.1.220"));
    assert_eq!(fixture.sessions(), Vec::<PathBuf>::new());
    assert_eq!(fixture.journal_count(), 0);
    assert!(!fixture.record.exists());
}

#[test]
fn non_session_subcommands_fail_before_state_or_child_launch() {
    for (index, rejected) in [
        "agents",
        "auth",
        "auto-mode",
        "doctor",
        "gateway",
        "install",
        "mcp",
        "plugin",
        "plugins",
        "project",
        "setup-token",
        "ultrareview",
        "update",
        "upgrade",
    ]
    .into_iter()
    .enumerate()
    {
        let fixture = Fixture::new(&format!("unsupported-subcommand-{index}"));
        fixture.skill(&fixture.left, "alpha", "fixture");
        let output = fixture
            .command()
            .arg("--")
            .arg(rejected)
            .output()
            .expect("asm should reject a non-session Claude subcommand");

        assert_eq!(output.status.code(), Some(64));
        assert!(String::from_utf8_lossy(&output.stderr).contains(rejected));
        assert!(!fixture.state.exists());
        assert!(!fixture.record.exists());
    }

    let fixture = Fixture::new("unsupported-subcommand-after-separator");
    fixture.skill(&fixture.left, "alpha", "fixture");
    let output = fixture
        .command()
        .arg("--")
        .arg("--")
        .arg("agents")
        .output()
        .expect("asm should reject Claude subcommand dispatch after its separator");

    assert_eq!(output.status.code(), Some(64));
    assert!(String::from_utf8_lossy(&output.stderr).contains("agents"));
    assert!(!fixture.state.exists());
    assert!(!fixture.record.exists());
}

#[test]
fn an_invalid_rightmost_claude_winner_never_falls_back_or_launches() {
    let fixture = Fixture::new("invalid-rightmost");
    fixture.skill(&fixture.left, "alpha", "valid lower-precedence source");
    let invalid = fixture.right.join("alpha");
    fs::create_dir(&invalid).expect("invalid winner directory");
    fs::write(invalid.join("SKILL.md"), "missing frontmatter\n").expect("invalid winner");

    let output = fixture
        .command()
        .arg("--")
        .arg("prompt")
        .output()
        .expect("asm should reject the selected winner");

    assert_eq!(output.status.code(), Some(65));
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(stderr.contains("invalid selected Skill"), "{stderr}");
    assert!(stderr.contains("rightmost selected winner"), "{stderr}");
    assert!(stderr.contains("no selected Skill was mounted"), "{stderr}");
    assert_eq!(fixture.sessions(), Vec::<PathBuf>::new());
    assert_eq!(fixture.journal_count(), 0);
    assert!(!fixture.record.exists());
}

#[test]
fn a_version_change_at_the_spawn_boundary_overrides_keep_and_cleans_staging() {
    let fixture = Fixture::new("upgrade-after-apply");
    fixture.skill(&fixture.left, "alpha", "fixture");

    let output = fixture
        .command()
        .arg("--keep-mounts")
        .arg("--")
        .arg("prompt")
        .env("SKILLMOUNT_FAKE_UNSUPPORTED_VERSION_AT", "3")
        .output()
        .expect("asm should reject a changed child release");

    assert_eq!(output.status.code(), Some(64));
    assert!(String::from_utf8_lossy(&output.stderr).contains("2.1.220"));
    assert!(
        !fixture.record.exists(),
        "the incompatible child must not start"
    );
    assert_eq!(fixture.sessions(), Vec::<PathBuf>::new());
    assert_eq!(fixture.journal_count(), 0);
    assert_eq!(
        fs::read_to_string(&fixture.version_record)
            .expect("version probe record")
            .lines()
            .count(),
        3
    );
}

#[test]
fn child_and_cleanup_exit_precedence_preserves_unowned_session_content() {
    let clean = Fixture::new("child-nonzero");
    clean.skill(&clean.left, "alpha", "fixture");
    let output = clean
        .command()
        .arg("--")
        .arg("prompt")
        .env("SKILLMOUNT_FAKE_EXIT", "2")
        .output()
        .expect("asm should preserve the child status");
    assert_eq!(output.status.code(), Some(2));
    assert_eq!(clean.sessions(), Vec::<PathBuf>::new());
    assert_eq!(clean.journal_count(), 0);

    for (label, child_code, expected_code) in
        [("cleanup-primary", "0", 73), ("child-primary", "2", 2)]
    {
        let fixture = Fixture::new(label);
        fixture.skill(&fixture.left, "alpha", "fixture");
        let output = fixture
            .command()
            .arg("--")
            .arg("prompt")
            .env("SKILLMOUNT_FAKE_CREATE_IN_ADD_DIR", "user-note.txt")
            .env("SKILLMOUNT_FAKE_EXIT", child_code)
            .output()
            .expect("asm should retain unexpected session content");
        let stderr = String::from_utf8_lossy(&output.stderr);

        assert_eq!(output.status.code(), Some(expected_code), "{stderr}");
        let record = fs::read_to_string(&fixture.record).expect("fake Claude record");
        let created = PathBuf::from(recorded_os_value(&record, "created"));
        assert_eq!(
            fs::read_to_string(&created).expect("unowned file survives cleanup"),
            "created by fake agent\n"
        );
        assert_eq!(fixture.journal_count(), 1);
        if child_code == "0" {
            assert!(stderr.contains("error: session cleanup failed"), "{stderr}");
        } else {
            assert!(
                stderr.contains("warning: session cleanup failed"),
                "{stderr}"
            );
        }
    }
}

#[test]
fn inherited_streams_and_launch_cwd_reach_fake_claude_unchanged() {
    let fixture = Fixture::new("streams");
    fixture.skill(&fixture.left, "alpha", "fixture");
    let mut child = fixture
        .command()
        .arg("--")
        .arg("prompt")
        .env("SKILLMOUNT_FAKE_BEHAVIOR", "streams")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("asm should spawn");
    child
        .stdin
        .take()
        .expect("piped stdin")
        .write_all(b"hello claude\n")
        .expect("write inherited stdin");
    let output = child.wait_with_output().expect("asm should finish");

    assert_success(&output);
    assert_eq!(output.stdout, b"fake-stdout:hello claude\n");
    assert!(String::from_utf8_lossy(&output.stderr).contains("fake-stderr:hello claude\n"));
    let record = fs::read_to_string(&fixture.record).expect("fake Claude record");
    assert_eq!(recorded_bytes(&record, "stdin"), b"hello claude\n");
}

#[cfg(unix)]
#[test]
fn an_interrupted_claude_child_exits_before_cleanup_without_touching_user_scopes() {
    let fixture = Fixture::new("interrupt");
    fixture.skill(&fixture.left, "alpha", "fixture");
    let watched = [&fixture.project, &fixture.home].map(|root| (root.clone(), snapshot(root)));
    let child = fixture
        .command()
        .arg("--")
        .arg("prompt")
        .env("SKILLMOUNT_FAKE_BEHAVIOR", "wait")
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("spawn interruptible Claude session");
    wait_until_ready(&fixture.record);

    nix::sys::signal::kill(
        nix::unistd::Pid::from_raw(i32::try_from(child.id()).expect("PID fits i32")),
        nix::sys::signal::Signal::SIGINT,
    )
    .expect("interrupt the wrapper");
    let output = child.wait_with_output().expect("interrupted wrapper exits");

    assert_success(&output);
    let record = fs::read_to_string(&fixture.record).expect("fake Claude record");
    assert!(record.contains("event=signal:2\n"), "{record}");
    assert_eq!(fixture.sessions(), Vec::<PathBuf>::new());
    assert_eq!(fixture.journal_count(), 0);
    for (root, before) in watched {
        assert_eq!(snapshot(&root), before, "{} changed", root.display());
    }
}

#[test]
fn two_fake_claude_children_overlap_with_distinct_owned_roots() {
    let fixture = Fixture::new("concurrent");
    fixture.skill(&fixture.left, "alpha", "fixture");
    let first_record = fixture.root.join("first.record");
    let second_record = fixture.root.join("second.record");
    let release = fixture.root.join("release");

    let first = spawn_waiting(&fixture, &first_record, "first.version", &release);
    wait_until_ready(&first_record);
    let second = spawn_waiting(&fixture, &second_record, "second.version", &release);
    wait_until_ready(&second_record);

    let first_root = recorded_injected_root(&first_record);
    let second_root = recorded_injected_root(&second_record);
    assert_ne!(first_root, second_root);
    assert!(first_root.join(".claude/skills/alpha").is_dir());
    assert!(second_root.join(".claude/skills/alpha").is_dir());
    assert_eq!(fixture.sessions().len(), 2);

    fs::write(&release, b"release\n").expect("release both fake children");
    assert_success(&wait_output(first));
    assert_success(&wait_output(second));
    assert_eq!(fixture.sessions(), Vec::<PathBuf>::new());
    assert_eq!(fixture.journal_count(), 0);
}

#[test]
fn both_installed_binary_names_run_the_same_claude_session_contract() {
    let fixture = Fixture::new("binary-parity");
    fixture.skill(&fixture.left, "alpha", "fixture");
    let asm = fixture
        .wrapper_command(ASM)
        .arg("--")
        .arg("prompt")
        .output()
        .expect("asm Claude session");
    assert_success(&asm);

    let fallback_record = fixture.root.join("fallback.record");
    let fallback_version = fixture.root.join("fallback.version");
    let fallback = fixture
        .wrapper_command(SKILLMOUNT)
        .arg("--")
        .arg("prompt")
        .env("SKILLMOUNT_FAKE_RECORD", &fallback_record)
        .env("SKILLMOUNT_FAKE_VERSION_RECORD", &fallback_version)
        .output()
        .expect("skillmount Claude session");
    assert_success(&fallback);

    assert_eq!(asm.status, fallback.status);
    assert_eq!(asm.stdout, fallback.stdout);
    assert_eq!(asm.stderr, fallback.stderr);
    assert!(fixture.record.is_file() && fallback_record.is_file());
    assert_eq!(fixture.sessions(), Vec::<PathBuf>::new());
    assert_eq!(fixture.journal_count(), 0);
}

fn spawn_waiting(fixture: &Fixture, record: &Path, version_name: &str, release: &Path) -> Child {
    fixture
        .command()
        .arg("--")
        .arg("prompt")
        .env("SKILLMOUNT_FAKE_RECORD", record)
        .env(
            "SKILLMOUNT_FAKE_VERSION_RECORD",
            fixture.root.join(version_name),
        )
        .env("SKILLMOUNT_FAKE_BEHAVIOR", "wait-for-file")
        .env("SKILLMOUNT_FAKE_RELEASE_FILE", release)
        .spawn()
        .expect("spawn waiting Claude session")
}

fn wait_until_ready(record: &Path) {
    let deadline = Instant::now() + Duration::from_secs(10);
    loop {
        if fs::read_to_string(record).is_ok_and(|contents| contents.contains("event=ready\n")) {
            return;
        }
        assert!(
            Instant::now() < deadline,
            "{} never became ready",
            record.display()
        );
        thread::sleep(Duration::from_millis(10));
    }
}

fn recorded_injected_root(record: &Path) -> PathBuf {
    let contents = fs::read_to_string(record).expect("fake Claude record");
    let arguments = recorded_os_values(&contents, "arg");
    arguments
        .windows(2)
        .find(|pair| pair[0] == OsStr::new("--add-dir"))
        .map(|pair| PathBuf::from(&pair[1]))
        .expect("fake Claude record has an injected --add-dir pair")
}

fn wait_output(child: Child) -> Output {
    child
        .wait_with_output()
        .expect("waiting Claude session exits")
}

fn assert_success(output: &Output) {
    assert_eq!(
        output.status.code(),
        Some(0),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
}

fn snapshot(root: &Path) -> BTreeMap<PathBuf, Vec<u8>> {
    let mut entries = BTreeMap::new();
    collect_snapshot(root, root, &mut entries);
    entries
}

fn collect_snapshot(root: &Path, current: &Path, entries: &mut BTreeMap<PathBuf, Vec<u8>>) {
    let Ok(metadata) = fs::symlink_metadata(current) else {
        return;
    };
    let relative = current
        .strip_prefix(root)
        .expect("snapshot root")
        .to_path_buf();
    if metadata.file_type().is_symlink() {
        entries.insert(
            relative,
            fs::read_link(current)
                .expect("snapshot link")
                .into_os_string()
                .to_string_lossy()
                .into_owned()
                .into_bytes(),
        );
    } else if metadata.is_file() {
        entries.insert(relative, fs::read(current).expect("snapshot file"));
    } else {
        entries.insert(relative, b"directory".to_vec());
        for child in fs::read_dir(current).expect("snapshot directory") {
            collect_snapshot(root, &child.expect("snapshot entry").path(), entries);
        }
    }
}

fn canonical_recorded_values(record: &str, name: &str) -> Vec<PathBuf> {
    recorded_os_values(record, name)
        .into_iter()
        .map(PathBuf::from)
        .collect()
}

fn recorded_os_value(record: &str, name: &str) -> OsString {
    recorded_os_values(record, name)
        .into_iter()
        .next()
        .unwrap_or_else(|| panic!("fake Claude record has no {name} entry"))
}

fn recorded_os_values(record: &str, name: &str) -> Vec<OsString> {
    let prefix = format!("{name}=");
    record
        .lines()
        .filter_map(|line| line.strip_prefix(&prefix))
        .map(native_from_hex)
        .collect()
}

fn recorded_bytes(record: &str, name: &str) -> Vec<u8> {
    let prefix = format!("{name}=");
    let encoded = record
        .lines()
        .find_map(|line| line.strip_prefix(&prefix))
        .unwrap_or_else(|| panic!("fake Claude record has no {name} entry"));
    decode_hex(encoded)
}

fn decode_hex(value: &str) -> Vec<u8> {
    assert_eq!(value.len() % 2, 0, "odd hexadecimal record");
    value
        .as_bytes()
        .chunks_exact(2)
        .map(|pair| {
            let pair = std::str::from_utf8(pair).expect("ASCII hexadecimal pair");
            u8::from_str_radix(pair, 16).expect("valid hexadecimal pair")
        })
        .collect()
}

#[cfg(unix)]
fn native_from_hex(value: &str) -> OsString {
    use std::os::unix::ffi::OsStringExt as _;

    OsString::from_vec(decode_hex(value))
}

#[cfg(windows)]
fn native_from_hex(value: &str) -> OsString {
    use std::os::windows::ffi::OsStringExt as _;

    let bytes = decode_hex(value);
    assert_eq!(bytes.len() % 2, 0, "odd UTF-16 byte record");
    OsString::from_wide(
        &bytes
            .chunks_exact(2)
            .map(|pair| u16::from_le_bytes([pair[0], pair[1]]))
            .collect::<Vec<_>>(),
    )
}
