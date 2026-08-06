//! End-to-end OMP session acceptance through the real `asm` process.

#![cfg(feature = "test-fixtures")]

use std::ffi::OsString;
use std::fmt::Write as _;
use std::fs;
use std::path::{Path, PathBuf};
use std::process::{Command, Output};
use std::time::{SystemTime, UNIX_EPOCH};

const ASM: &str = env!("CARGO_BIN_EXE_asm");
const FAKE_OMP: &str = env!("CARGO_BIN_EXE_skillmount-fake-agent");

/// Banner the OMP adapter carries as its last-tested compatibility evidence.
const LAST_TESTED_BANNER: &str = "omp/17.2.9";

struct Fixture {
    root: PathBuf,
    project: PathBuf,
    left: PathBuf,
    right: PathBuf,
    home: PathBuf,
    state: PathBuf,
    record: PathBuf,
    version_record: PathBuf,
    inherited_variable: PathBuf,
}

impl Fixture {
    fn new(label: &str) -> Self {
        let nonce = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .expect("clock after epoch")
            .as_nanos();
        let root = std::env::temp_dir().join(format!(
            "skillmount-omp-session-{label}-{}-{nonce}",
            std::process::id()
        ));
        fs::create_dir_all(&root).expect("fixture root");
        // OMP compares the launch CWD against the home directory literally, while SkillMount
        // canonicalizes the launch CWD. A fixture whose temporary root is reached through a symlink
        // could therefore never reach the home-escape invariant, so every fixture path is canonical
        // from the start.
        let root = fs::canonicalize(&root).expect("canonical fixture root");
        let fixture = Self {
            project: root.join("project"),
            left: root.join("left"),
            right: root.join("right"),
            home: root.join("home"),
            state: root.join("state"),
            record: root.join("fake-omp.record"),
            version_record: root.join("fake-omp-version.record"),
            inherited_variable: root.join("inherited-environment"),
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
        // OMP walks project ancestors up to the nearest repository root. Anchoring the project keeps
        // the inspected namespace inside the fixture instead of reaching the directories above the
        // shared temporary root.
        fs::create_dir(fixture.project.join(".git")).expect("project repository anchor");
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

    /// Creates a Skill whose frontmatter carries no `description`.
    ///
    /// Whether that entry loads at all is a per-provider decision in OMP, so a fixture that pins
    /// the requirement needs both shapes side by side in the same root.
    fn skill_without_description(&self, source: &Path, name: &str) -> PathBuf {
        assert!(
            source.starts_with(&self.root),
            "fixture Skills must stay inside the isolated root"
        );
        let skill = source.join(name);
        fs::create_dir_all(&skill).expect("Skill fixture");
        fs::write(
            skill.join("SKILL.md"),
            format!("---\nname: {name}\n---\n{name} without a description\n"),
        )
        .expect("Skill metadata");
        skill
    }

    /// Declares `directories` as `skills.customDirectories` in the global settings layer.
    ///
    /// The global layer lives in the agent directory rather than the project, so a fixture can
    /// configure custom directories without also populating a project settings provider whose own
    /// merge precedence would then be part of the result.
    fn custom_directories(&self, directories: &[&Path]) {
        let mut body = String::from("skills:\n  enabled: true\n  customDirectories:\n");
        for directory in directories {
            // A single-quoted YAML scalar keeps a Windows path's backslashes literal.
            let _ = writeln!(body, "    - '{}'", directory.display());
        }
        let path = self.home.join(".omp/agent/config.yml");
        fs::create_dir_all(path.parent().expect("agent directory")).expect("agent directory");
        fs::write(&path, body).expect("global OMP settings");
    }

    /// Builds a verbose dry run whose launch CWD may sit below the project root.
    ///
    /// [`Self::wrapper_command`] passes one path as both roots, which the CLI accepts only while
    /// the launch CWD is itself the inferred project root. An ancestor-walk fixture needs the two
    /// to differ, so it names the repository anchor as the project root explicitly.
    fn discovery_command(&self, launch_cwd: &Path) -> Command {
        let mut command = Command::new(ASM);
        command
            .arg("omp")
            .arg("--skills-dir")
            .arg(&self.left)
            .arg("--project-root")
            .arg(&self.project)
            .arg("--cwd")
            .arg(launch_cwd)
            .arg("--agent-bin")
            .arg(FAKE_OMP)
            .arg("--dry-run")
            .arg("--verbose");
        self.configure_environment(&mut command);
        command
    }

    fn command(&self) -> Command {
        self.wrapper_command(&self.project)
    }

    fn wrapper_command(&self, launch_cwd: &Path) -> Command {
        let mut command = Command::new(ASM);
        command.arg("omp").arg("--skills-dir").arg(&self.left);
        if fs::read_dir(&self.right)
            .expect("right source fixture")
            .next()
            .is_some()
        {
            command.arg("--skills-dir").arg(&self.right);
        }
        command
            .arg("--project-root")
            .arg(launch_cwd)
            .arg("--cwd")
            .arg(launch_cwd)
            .arg("--agent-bin")
            .arg(FAKE_OMP);
        self.configure_environment(&mut command);
        command
    }

    fn configure_environment(&self, command: &mut Command) {
        command
            .env("HOME", &self.home)
            .env("USERPROFILE", &self.home)
            .env("LOCALAPPDATA", self.home.join("AppData/Local"))
            .env("SKILLMOUNT_STATE_DIR", &self.state)
            .env("SKILLMOUNT_FAKE_RECORD", &self.record)
            .env("SKILLMOUNT_FAKE_VERSION_RECORD", &self.version_record)
            .env("SKILLMOUNT_FAKE_VERSION_OUTPUT", LAST_TESTED_BANNER)
            .env("SKILLMOUNT_FAKE_BEHAVIOR", "exit")
            // The fixture records one inherited variable it does not otherwise use, so a launch can
            // show that the child receives the wrapper's own environment rather than a rewritten
            // one. An OMP launch plan carries no environment override of its own.
            .env("SKILLMOUNT_FAKE_RECORD_CODEX_HOME", "1")
            .env("CODEX_HOME", &self.inherited_variable)
            // The banner must come from the child process, so a deterministic override exported by
            // a developer or another suite must never short-circuit the observation.
            .env_remove("SKILLMOUNT_TEST_OMP_VERSION")
            // OMP resolves its roots from the environment, so the developer's real profile,
            // configuration overlay, and XDG bases must never reach a fixture.
            .env_remove("OMP_PROFILE")
            .env_remove("PI_PROFILE")
            .env_remove("PI_CONFIG_FILES")
            .env_remove("PI_CONFIG_DIR")
            .env_remove("PI_CODING_AGENT_DIR")
            .env_remove("XDG_DATA_HOME")
            // The version observation runs in the invocation directory, which must differ from the
            // launch CWD for that distinction to be observable at all.
            .current_dir(&self.root);
    }

    /// Returns the OMP scope a session creates and releases in the launch CWD.
    fn omp_scope(&self) -> PathBuf {
        self.project.join(".omp")
    }

    /// Returns the discovery entry an OMP session mounts into.
    fn destination(&self) -> PathBuf {
        self.project.join(".omp/skills")
    }

    fn journals(&self) -> Vec<PathBuf> {
        let mut journals = fs::read_dir(self.state.join("transactions")).map_or_else(
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
        journals.sort();
        journals
    }
}

impl Drop for Fixture {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.root);
    }
}

#[test]
fn the_selected_skill_is_visible_in_the_launch_cwd_then_the_omp_scope_is_released() {
    let fixture = Fixture::new("happy-path");
    let alpha = fixture.skill(&fixture.left, "alpha", "left winner");
    let mounted = fixture.destination().join("alpha");
    let expected_paths = std::env::join_paths([&mounted]).expect("fixture path list");

    let output = fixture
        .command()
        .arg("--")
        .arg("prompt")
        .env("SKILLMOUNT_FAKE_EXPECT_PATHS", expected_paths)
        .output()
        .expect("asm should launch fake OMP");

    assert_success(&output);
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        stderr.contains("Mounted 1 skill from 1 source argument for OMP (0 source overrides)."),
        "{stderr}"
    );
    assert!(stderr.contains("  alpha\n"), "{stderr}");
    assert!(stderr.contains("Launching omp..."), "{stderr}");
    assert!(output.stdout.is_empty(), "child stdout stays data-only");

    let record = fs::read_to_string(&fixture.record).expect("fake OMP launch record");
    assert_eq!(
        recorded_os_values(&record, "visible"),
        [mounted.into_os_string()],
        "the child reads the mount from OMP's own project scope"
    );
    assert_eq!(
        recorded_os_values(&record, "visible-target"),
        [fs::canonicalize(&alpha)
            .expect("canonical source Skill")
            .into_os_string()],
        "the mount resolves to the canonical source"
    );
    assert!(
        !exists(&fixture.omp_scope()),
        "cleanup releases the whole .omp scope"
    );
    assert_eq!(fixture.journals(), Vec::<PathBuf>::new());
    assert_single_silent_last_tested_observation(&fixture, &stderr);
}

#[test]
fn the_launch_argv_is_exactly_the_operator_passthrough_in_the_launch_cwd() {
    let fixture = Fixture::new("launch-shape");
    fixture.skill(&fixture.left, "alpha", "fixture");

    let output = fixture
        .command()
        .arg("--")
        .arg("--print")
        .arg("--model")
        .arg("fixture-model")
        .arg("prompt text")
        .output()
        .expect("asm should launch fake OMP");

    assert_success(&output);
    let record = fs::read_to_string(&fixture.record).expect("fake OMP launch record");
    assert_eq!(
        recorded_os_values(&record, "arg"),
        [
            OsString::from("--print"),
            OsString::from("--model"),
            OsString::from("fixture-model"),
            OsString::from("prompt text"),
        ],
        "OMP receives the operator's passthrough and nothing else"
    );
    assert_eq!(
        fs::canonicalize(PathBuf::from(recorded_os(&record, "cwd"))).expect("canonical child CWD"),
        fixture.project,
        "the child runs in the launch CWD, not the invocation CWD"
    );
    assert_eq!(
        PathBuf::from(recorded_os(&record, "env:CODEX_HOME")),
        fixture.inherited_variable,
        "the child inherits the wrapper environment instead of an overridden one"
    );
}

#[test]
fn every_accepted_omp_control_reaches_the_child_unchanged() {
    let fixture = Fixture::new("accepted-controls");
    fixture.skill(&fixture.left, "alpha", "fixture");
    let extra = fixture.root.join("extra");
    fs::create_dir(&extra).expect("extra directory fixture");
    let forwarded = [
        OsString::from("--print"),
        OsString::from("-p"),
        OsString::from("--model"),
        OsString::from("fixture-model"),
        OsString::from("--no-session"),
        OsString::from("--add-dir"),
        extra.into_os_string(),
        OsString::from("--thinking"),
        OsString::from("high"),
        OsString::from("--auto-approve"),
        OsString::from("--yolo"),
        OsString::from("--approval-mode"),
        OsString::from("never"),
        OsString::from("initial prompt"),
        // Everything after OMP's own separator is literal message text, including words that would
        // otherwise name a rejected control.
        OsString::from("--"),
        OsString::from("--resume"),
        OsString::from("acp"),
    ];

    let mut command = fixture.command();
    command.arg("--").args(&forwarded);
    let output = command
        .output()
        .expect("asm should forward every accepted control");

    assert_success(&output);
    let record = fs::read_to_string(&fixture.record).expect("fake OMP launch record");
    assert_eq!(recorded_os_values(&record, "arg"), forwarded);
}

#[test]
fn a_drifted_omp_banner_warns_once_and_still_mounts_the_session() {
    let fixture = Fixture::new("drifted-banner");
    let alpha = fixture.skill(&fixture.left, "alpha", "fixture");
    let mounted = fixture.destination().join("alpha");
    let expected_paths = std::env::join_paths([&mounted]).expect("fixture path list");

    let output = fixture
        .command()
        .arg("--")
        .arg("prompt")
        .env("SKILLMOUNT_FAKE_VERSION_OUTPUT", "omp/99.0.0")
        .env("SKILLMOUNT_FAKE_EXPECT_PATHS", expected_paths)
        .output()
        .expect("asm should continue under drifted version evidence");

    let stderr = String::from_utf8_lossy(&output.stderr);
    assert_eq!(output.status.code(), Some(0), "{stderr}");
    assert_eq!(
        stderr
            .matches("version compatibility is unverified")
            .count(),
        1,
        "{stderr}"
    );
    assert!(stderr.contains("omp/99.0.0"), "{stderr}");
    assert!(stderr.contains(LAST_TESTED_BANNER), "{stderr}");
    assert!(stderr.contains("docs/compatibility.md"), "{stderr}");
    assert_eq!(
        fs::read_to_string(&fixture.version_record)
            .expect("version observation record")
            .lines()
            .count(),
        1,
        "advisory drift still costs exactly one observation"
    );
    let record = fs::read_to_string(&fixture.record).expect("fake OMP launch record");
    assert_eq!(
        recorded_os_values(&record, "visible-target"),
        [fs::canonicalize(&alpha)
            .expect("canonical source Skill")
            .into_os_string()],
        "an unverified banner still mounts the selected Skill"
    );
    assert!(!exists(&fixture.omp_scope()));
    assert_eq!(fixture.journals(), Vec::<PathBuf>::new());
}

#[test]
fn a_nonzero_omp_status_is_returned_unchanged_after_the_mount_is_released() {
    let fixture = Fixture::new("child-nonzero");
    fixture.skill(&fixture.left, "alpha", "fixture");

    let output = fixture
        .command()
        .arg("--")
        .arg("prompt")
        .env("SKILLMOUNT_FAKE_EXIT", "3")
        .output()
        .expect("asm should preserve the child status");

    let stderr = String::from_utf8_lossy(&output.stderr);
    assert_eq!(output.status.code(), Some(3), "{stderr}");
    assert!(fixture.record.is_file(), "the child must start");
    assert!(
        !exists(&fixture.omp_scope()),
        "cleanup still runs after a failing child"
    );
    assert_eq!(fixture.journals(), Vec::<PathBuf>::new());
}

#[test]
fn the_rightmost_source_occurrence_is_the_mounted_winner() {
    let fixture = Fixture::new("rightmost-wins");
    fixture.skill(&fixture.left, "alpha", "shadowed");
    let winner = fixture.skill(&fixture.right, "alpha", "rightmost winner");
    let mounted = fixture.destination().join("alpha");
    let expected_paths = std::env::join_paths([&mounted]).expect("fixture path list");

    let output = fixture
        .command()
        .arg("--")
        .arg("prompt")
        .env("SKILLMOUNT_FAKE_EXPECT_PATHS", expected_paths)
        .output()
        .expect("asm should mount the rightmost winner");

    assert_success(&output);
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        stderr.contains("Mounted 1 skill from 2 source arguments for OMP (1 source override)."),
        "{stderr}"
    );
    let record = fs::read_to_string(&fixture.record).expect("fake OMP launch record");
    assert_eq!(
        recorded_os_values(&record, "visible-target"),
        [fs::canonicalize(&winner)
            .expect("canonical rightmost winner")
            .into_os_string()]
    );
}

#[test]
fn an_invalid_rightmost_omp_winner_never_falls_back_or_launches() {
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

    let stderr = String::from_utf8_lossy(&output.stderr);
    assert_eq!(output.status.code(), Some(65), "{stderr}");
    assert!(stderr.contains("invalid selected Skill"), "{stderr}");
    assert!(stderr.contains("rightmost selected winner"), "{stderr}");
    assert!(stderr.contains("no selected Skill was mounted"), "{stderr}");
    assert!(!exists(&fixture.omp_scope()));
    assert!(!fixture.record.exists(), "no child is launched");
    assert_eq!(fixture.journals(), Vec::<PathBuf>::new());
}

#[test]
fn keep_mounts_retains_the_mount_in_a_terminal_kept_transaction() {
    let fixture = Fixture::new("keep-mounts");
    let alpha = fixture.skill(&fixture.left, "alpha", "fixture");
    let mounted = fixture.destination().join("alpha");
    let expected_paths = std::env::join_paths([&mounted]).expect("fixture path list");

    let output = fixture
        .command()
        .arg("--keep-mounts")
        .arg("--")
        .arg("prompt")
        .env("SKILLMOUNT_FAKE_EXPECT_PATHS", expected_paths)
        .output()
        .expect("asm should retain the mount");

    let stderr = String::from_utf8_lossy(&output.stderr);
    assert_eq!(output.status.code(), Some(0), "{stderr}");
    assert!(
        stderr.contains("retained because --keep-mounts was requested"),
        "{stderr}"
    );
    assert!(
        !stderr.contains("cleanup could not finish"),
        "intentional retention is not a cleanup failure: {stderr}"
    );
    assert!(exists(&mounted), "--keep-mounts retains the mount");
    assert_eq!(
        fs::canonicalize(&mounted).expect("canonical retained mount"),
        fs::canonicalize(&alpha).expect("canonical source Skill")
    );
    let journals = fixture.journals();
    assert_eq!(journals.len(), 1, "{journals:?}");
    let journal = fs::read_to_string(&journals[0]).expect("retained journal");
    assert!(journal.contains("agent=omp"), "{journal}");
    assert!(
        journal.contains("status=kept"),
        "the retained transaction is terminal: {journal}"
    );
}

#[test]
fn shell_metacharacters_and_a_unicode_prompt_reach_omp_as_verbatim_argv_values() {
    let fixture = Fixture::new("verbatim-argv");
    fixture.skill(&fixture.left, "alpha", "fixture");
    let escape = fixture.root.join("escaped");
    let forwarded = [
        OsString::from("スキルの説明を書いてください"),
        OsString::from("a && b || c ; d | e > f"),
        OsString::from(format!(
            "$(touch {0}) `touch {0}` %PATH% $HOME",
            escape.display()
        )),
        OsString::from("quote \" and ' and \\ backslash"),
    ];

    let mut command = fixture.command();
    command.arg("--").args(&forwarded);
    let output = command
        .output()
        .expect("asm should forward hostile values verbatim");

    assert_success(&output);
    let record = fs::read_to_string(&fixture.record).expect("fake OMP launch record");
    assert_eq!(
        recorded_os_values(&record, "arg"),
        forwarded,
        "every forwarded value stays one verbatim argv entry"
    );
    assert!(
        !exists(&escape),
        "no shell ever interpreted a forwarded value"
    );
}

#[test]
fn root_relocating_omp_arguments_fail_before_any_project_or_state_mutation() {
    for (index, forwarded) in [
        vec!["--cwd", "/tmp"],
        vec!["--cwd=/tmp"],
        vec!["--profile", "fixture"],
        vec!["--profile=fixture"],
        vec!["--alias", "fixture"],
        vec!["--config", "fixture.yml"],
    ]
    .into_iter()
    .enumerate()
    {
        let fixture = Fixture::new(&format!("root-relocating-{index}"));
        fixture.skill(&fixture.left, "alpha", "fixture");

        let mut command = fixture.command();
        command.arg("--").args(&forwarded);
        let output = command
            .output()
            .expect("asm should reject a relocated OMP root");

        assert_rejected_before_any_mutation(&fixture, &output, "relocates the discovery root");
    }
}

#[test]
fn skill_set_changing_omp_arguments_fail_before_any_project_or_state_mutation() {
    for (index, forwarded) in [
        vec!["--no-skills"],
        vec!["--skills", "other"],
        vec!["-e", "package"],
        vec!["--extension", "package"],
        vec!["--hook", "hook.js"],
        vec!["--no-extensions"],
        vec!["--plugin-dir", "packages"],
    ]
    .into_iter()
    .enumerate()
    {
        let fixture = Fixture::new(&format!("skill-set-changing-{index}"));
        fixture.skill(&fixture.left, "alpha", "fixture");

        let mut command = fixture.command();
        command.arg("--").args(&forwarded);
        let output = command
            .output()
            .expect("asm should reject a changed Skill or provider set");

        assert_rejected_before_any_mutation(&fixture, &output, "changes the Skill");
    }
}

#[test]
fn session_reusing_omp_arguments_fail_before_any_project_or_state_mutation() {
    for (index, forwarded) in [
        vec!["-c"],
        vec!["--continue"],
        vec!["-r"],
        vec!["--resume"],
        vec!["--session", "identifier"],
        vec!["--fork", "identifier"],
        vec!["--from-claude"],
        vec!["--from-codex"],
        vec!["--export", "transcript.md"],
    ]
    .into_iter()
    .enumerate()
    {
        let fixture = Fixture::new(&format!("session-reusing-{index}"));
        fixture.skill(&fixture.left, "alpha", "fixture");

        let mut command = fixture.command();
        command.arg("--").args(&forwarded);
        let output = command
            .output()
            .expect("asm should reject a reused, forked, or imported session");

        assert_rejected_before_any_mutation(&fixture, &output, "resumes, forks, imports");
    }
}

#[test]
fn protocol_server_modes_are_rejected_while_text_and_json_modes_launch() {
    for (index, forwarded) in [
        vec!["--mode", "rpc"],
        vec!["--mode=rpc"],
        vec!["--mode", "rpc-ui"],
        vec!["--mode=rpc-ui"],
        vec!["--mode", "acp"],
        vec!["--mode=acp"],
    ]
    .into_iter()
    .enumerate()
    {
        let fixture = Fixture::new(&format!("protocol-mode-{index}"));
        fixture.skill(&fixture.left, "alpha", "fixture");

        let mut command = fixture.command();
        command.arg("--").args(&forwarded);
        let output = command
            .output()
            .expect("asm should reject a protocol-server mode");

        assert_rejected_before_any_mutation(&fixture, &output, "protocol server");
    }

    for (index, forwarded) in [vec!["--mode", "text"], vec!["--mode=json"]]
        .into_iter()
        .enumerate()
    {
        let fixture = Fixture::new(&format!("session-mode-{index}"));
        fixture.skill(&fixture.left, "alpha", "fixture");

        let mut command = fixture.command();
        command.arg("--").args(&forwarded);
        let output = command
            .output()
            .expect("asm should launch a foreground session mode");

        assert_success(&output);
        let record = fs::read_to_string(&fixture.record).expect("fake OMP launch record");
        assert_eq!(
            recorded_os_values(&record, "arg"),
            forwarded
                .iter()
                .map(OsString::from)
                .collect::<Vec<OsString>>()
        );
    }
}

#[test]
fn non_session_omp_commands_fail_before_any_project_or_state_mutation() {
    for (index, forwarded) in [
        "acp",
        "config",
        "plugin",
        "shell",
        "worktree",
        "wt",
        "q",
        "gc",
        "update",
        "install",
        "auth-broker",
        "__complete",
    ]
    .into_iter()
    .enumerate()
    {
        let fixture = Fixture::new(&format!("non-session-command-{index}"));
        fixture.skill(&fixture.left, "alpha", "fixture");

        let output = fixture
            .command()
            .arg("--")
            .arg(forwarded)
            .arg("prompt")
            .output()
            .expect("asm should reject a non-session OMP command");

        assert_rejected_before_any_mutation(
            &fixture,
            &output,
            "does not start a supervised foreground session",
        );
        assert!(String::from_utf8_lossy(&output.stderr).contains(forwarded));
    }
}

#[test]
fn a_command_shaped_word_after_the_command_position_stays_prompt_text() {
    let fixture = Fixture::new("late-command-shaped-word");
    fixture.skill(&fixture.left, "alpha", "fixture");

    let output = fixture
        .command()
        .arg("--")
        .arg("summarize")
        .arg("config")
        .output()
        .expect("asm should treat later command-shaped words as prompt text");

    assert_success(&output);
    let record = fs::read_to_string(&fixture.record).expect("fake OMP launch record");
    assert_eq!(
        recorded_os_values(&record, "arg"),
        [OsString::from("summarize"), OsString::from("config")]
    );
}

#[test]
fn root_relocating_omp_environment_is_rejected_on_read_only_and_mutating_paths() {
    for (index, variable) in ["OMP_PROFILE", "PI_PROFILE", "PI_CONFIG_FILES"]
        .into_iter()
        .enumerate()
    {
        let fixture = Fixture::new(&format!("environment-{index}"));
        fixture.skill(&fixture.left, "alpha", "fixture");

        let session = fixture
            .command()
            .arg("--")
            .arg("prompt")
            .env(variable, "fixture")
            .output()
            .expect("asm should reject a relocated OMP environment");
        assert_rejected_before_any_mutation(
            &fixture,
            &session,
            "unset it or run the agent directly",
        );
        assert!(String::from_utf8_lossy(&session.stderr).contains(variable));

        let dry_run = fixture
            .command()
            .arg("--dry-run")
            .arg("--")
            .arg("prompt")
            .env(variable, "fixture")
            .output()
            .expect("a dry run must not describe roots OMP would never read");
        assert_rejected_before_any_mutation(
            &fixture,
            &dry_run,
            "unset it or run the agent directly",
        );

        let mut inspect = Command::new(ASM);
        inspect
            .arg("inspect")
            .arg("--skills-dir")
            .arg(&fixture.left)
            .arg("--agent")
            .arg("omp");
        fixture.configure_environment(&mut inspect);
        let inspected = inspect
            .env(variable, "fixture")
            .output()
            .expect("inspection must not describe roots OMP would never read");
        assert_rejected_before_any_mutation(
            &fixture,
            &inspected,
            "unset it or run the agent directly",
        );
    }
}

#[test]
fn a_staging_mount_mode_is_incompatible_with_omp() {
    let fixture = Fixture::new("staging-mount-mode");
    fixture.skill(&fixture.left, "alpha", "fixture");

    let output = fixture
        .command()
        .arg("--mount-mode=staging")
        .arg("--")
        .arg("prompt")
        .output()
        .expect("asm should reject a staging mount for OMP");

    assert_rejected_before_any_mutation(
        &fixture,
        &output,
        "--mount-mode=staging is incompatible with OMP",
    );
}

/// The guard has to survive a `$HOME` that names the launch CWD only after resolution.
///
/// OMP compares `normalizePathForComparison` of both sides (`startup-cwd.ts:16-20`), which resolves
/// through `fs.realpathSync`, so it escapes the home directory here. Comparing `SkillMount`'s
/// canonical launch CWD against the raw environment value would not match, the consent gate would
/// never fire, and the session would mount into the user's home scope for a child that immediately
/// moves away from it.
#[cfg(unix)]
#[test]
fn a_symlinked_home_still_needs_omps_own_allow_home() {
    let fixture = Fixture::new("home-escape-link");
    fixture.skill(&fixture.left, "alpha", "fixture");
    let linked_home = fixture.root.join("home-link");
    std::os::unix::fs::symlink(&fixture.home, &linked_home).expect("home symlink fixture");

    let rejected = fixture
        .wrapper_command(&fixture.home)
        .env("HOME", &linked_home)
        .env("USERPROFILE", &linked_home)
        .arg("--")
        .arg("prompt")
        .output()
        .expect("asm should refuse a home launch CWD reached through a link");

    let stderr = String::from_utf8_lossy(&rejected.stderr);
    assert_eq!(rejected.status.code(), Some(64), "{stderr}");
    assert!(stderr.contains("--allow-home"), "{stderr}");
    assert!(
        !exists(&fixture.home.join(".omp")),
        "no project directory is created"
    );
    assert!(!fixture.state.exists(), "no state directory is created");
    assert!(!fixture.record.exists(), "no child is launched");
}

/// A dry run must refuse what the mutating run refuses, rather than describe it.
///
/// `--dry-run` prints the plan the session would apply. Printing a plan for a namespace the child
/// relocates away from before loading any Skill would be a confident description of something that
/// never happens.
#[test]
fn a_dry_run_in_the_home_directory_is_refused_like_a_session() {
    let fixture = Fixture::new("home-escape-dry-run");
    fixture.skill(&fixture.left, "alpha", "fixture");

    let rejected = fixture
        .wrapper_command(&fixture.home)
        .arg("--dry-run")
        .arg("--")
        .arg("prompt")
        .output()
        .expect("asm should refuse a home launch CWD on the read-only path too");

    let stderr = String::from_utf8_lossy(&rejected.stderr);
    assert_eq!(rejected.status.code(), Some(64), "{stderr}");
    assert!(stderr.contains("--allow-home"), "{stderr}");
    assert!(
        !exists(&fixture.home.join(".omp")),
        "a refused dry run creates nothing"
    );
    assert!(!fixture.state.exists(), "no state directory is created");
    assert!(!fixture.version_record.exists(), "no version is probed");
}

#[test]
fn a_home_launch_cwd_needs_omps_own_allow_home_passthrough() {
    let fixture = Fixture::new("home-escape");
    fixture.skill(&fixture.left, "alpha", "fixture");
    let mounted = fixture.home.join(".omp/skills/alpha");

    let rejected = fixture
        .wrapper_command(&fixture.home)
        .arg("--")
        .arg("prompt")
        .output()
        .expect("asm should refuse a home launch CWD");

    let stderr = String::from_utf8_lossy(&rejected.stderr);
    assert_eq!(rejected.status.code(), Some(64), "{stderr}");
    assert!(stderr.contains("--allow-home"), "{stderr}");
    assert!(
        !exists(&fixture.home.join(".omp")),
        "no project directory is created"
    );
    assert!(!fixture.state.exists(), "no state directory is created");
    assert!(!fixture.record.exists(), "no child is launched");

    let expected_paths = std::env::join_paths([&mounted]).expect("fixture path list");
    let accepted = fixture
        .wrapper_command(&fixture.home)
        .arg("--")
        .arg("--allow-home")
        .arg("prompt")
        .env("SKILLMOUNT_FAKE_EXPECT_PATHS", expected_paths)
        .output()
        .expect("asm should honor the operator's own --allow-home");

    assert_success(&accepted);
    let record = fs::read_to_string(&fixture.record).expect("fake OMP launch record");
    assert_eq!(
        recorded_os_values(&record, "arg"),
        [OsString::from("--allow-home"), OsString::from("prompt")]
    );
    assert_eq!(
        recorded_os_values(&record, "visible"),
        [mounted.into_os_string()]
    );
    assert!(
        !exists(&fixture.home.join(".omp")),
        "cleanup releases the whole .omp scope"
    );
    assert_eq!(fixture.journals(), Vec::<PathBuf>::new());
}

/// Provider order is the contract every other OMP conflict answer rests on.
///
/// OMP registers providers by descending priority - `native` 100, `omp-plugins` 90, `claude` 80,
/// `claude-plugins`, `agents` and `codex` 70, `opencode` 55, `github` 30, `omp-managed` 5 - and
/// `registerProvider` inserts before the first strictly lower one (`capability/index.ts:86`), so
/// the import order in `discovery/index.ts` breaks every tie. `skills.customDirectories` is not a
/// provider and is scanned after all of them. Reordering two roots silently moves which one wins a
/// duplicated Skill name, and no other fixture would notice.
///
/// The two plugin providers are absent here because a plugin root exists only once an extension
/// package declares one; the plugin fixtures cover their placement.
#[test]
fn every_provider_root_is_scanned_in_the_recorded_priority_and_registration_order() {
    let fixture = Fixture::new("provider-order");
    fixture.skill(&fixture.left, "alpha", "fixture");
    let custom = fixture.root.join("custom");
    let roots = recorded_provider_roots(&fixture, &custom);
    for (_, root, skill) in &roots {
        fixture.skill(root, skill, "fixture");
    }
    fixture.custom_directories(&[&custom]);

    let output = fixture
        .discovery_command(&fixture.project)
        .output()
        .expect("asm should describe the OMP namespace");

    assert_success(&output);
    let observed = discovery_scopes(&String::from_utf8_lossy(&output.stdout));
    // `omp-managed` is the one root that contributes no entry: OMP defers an auto-learned Skill to
    // any authored one, so it never claims its own name.
    let expected: Vec<(&str, String, Vec<&str>)> = roots
        .iter()
        .map(|(kind, root, skill)| {
            (
                *kind,
                rendered_root(&fixture, root),
                if *kind == "omp managed" {
                    Vec::new()
                } else {
                    vec![*skill]
                },
            )
        })
        .collect();
    assert_eq!(
        observed
            .iter()
            .map(|scope| (scope.kind.as_str(), scope.root.clone(), scope.entry_names()))
            .collect::<Vec<_>>(),
        expected
    );
}

/// Every provider root this release scans, paired with the uniquely named Skill a fixture plants
/// in it, in the order OMP itself scans them.
///
/// A unique name per root is what makes a rendered entry prove the root was scanned rather than
/// merely listed: a name already claimed elsewhere would be folded away and leave the scope empty
/// for a reason unrelated to ordering.
fn recorded_provider_roots(
    fixture: &Fixture,
    custom: &Path,
) -> Vec<(&'static str, PathBuf, &'static str)> {
    vec![
        (
            "omp project",
            fixture.project.join(".omp/skills"),
            "native-project",
        ),
        (
            "omp user",
            fixture.home.join(".omp/agent/skills"),
            "native-user",
        ),
        (
            "omp compatibility",
            fixture.home.join(".claude/skills"),
            "claude-user",
        ),
        (
            "omp compatibility",
            fixture.project.join(".claude/skills"),
            "claude-project",
        ),
        (
            "omp compatibility",
            fixture.project.join(".agent/skills"),
            "agent-project",
        ),
        (
            "omp compatibility",
            fixture.project.join(".agents/skills"),
            "agents-project",
        ),
        (
            "omp compatibility",
            fixture.home.join(".agent/skills"),
            "agent-user",
        ),
        (
            "omp compatibility",
            fixture.home.join(".agents/skills"),
            "agents-user",
        ),
        (
            "omp compatibility",
            fixture.home.join(".codex/skills"),
            "codex-user",
        ),
        (
            "omp compatibility",
            fixture.project.join(".codex/skills"),
            "codex-project",
        ),
        (
            "omp compatibility",
            fixture.home.join(".config/opencode/skills"),
            "opencode-user",
        ),
        (
            "omp compatibility",
            fixture.project.join(".opencode/skills"),
            "opencode-project",
        ),
        (
            "omp compatibility",
            fixture.project.join(".github/skills"),
            "github-project",
        ),
        (
            "omp managed",
            fixture.home.join(".omp/agent/managed-skills"),
            "managed",
        ),
        ("omp custom", custom.to_path_buf(), "custom"),
    ]
}

/// `native` is the only provider that walks the project ancestors, and it walks them nearest first.
///
/// Reversing that walk would let an outer `.omp/skills` shadow the launch CWD's own Skill, which is
/// the opposite of OMP's precedence: a repository-wide Skill would silently win over the one the
/// operator is standing in, and a mount planned against the wrong winner would be applied anyway.
#[test]
fn the_native_provider_scans_project_ancestors_nearest_first_then_the_user_directory() {
    let fixture = Fixture::new("native-ancestors");
    fixture.skill(&fixture.left, "alpha", "fixture");
    let nested = fixture.project.join("nested");
    let deeper = nested.join("deeper");
    fixture.skill(&deeper.join(".omp/skills"), "nearest", "fixture");
    fixture.skill(&nested.join(".omp/skills"), "middle", "fixture");
    fixture.skill(&fixture.project.join(".omp/skills"), "outermost", "fixture");
    fixture.skill(&fixture.home.join(".omp/agent/skills"), "user", "fixture");

    let output = fixture
        .discovery_command(&deeper)
        .output()
        .expect("asm should describe the OMP namespace");

    assert_success(&output);
    let scopes = discovery_scopes(&String::from_utf8_lossy(&output.stdout));
    let user_root = rendered_root(&fixture, &fixture.home.join(".omp/agent/skills"));
    assert_eq!(
        scopes
            .iter()
            .take(4)
            .map(|scope| (
                scope.kind.as_str(),
                scope.root.as_str(),
                scope.entry_names()
            ))
            .collect::<Vec<_>>(),
        [
            ("omp project", "nested/deeper/.omp/skills", vec!["nearest"]),
            ("omp ancestor", "nested/.omp/skills", vec!["middle"]),
            ("omp ancestor", ".omp/skills", vec!["outermost"]),
            ("omp user", user_root.as_str(), vec!["user"]),
        ]
    );
}

/// A missing `description` is fatal for some providers and irrelevant for others.
///
/// `native`, `github`, `omp-managed` and every custom directory require one; the compatibility
/// providers that read another Agent's layout do not. Requiring it everywhere would drop a Skill
/// OMP loads, and requiring it nowhere would report a conflict OMP never sees.
#[test]
fn only_the_description_requiring_providers_drop_a_skill_without_a_description() {
    let fixture = Fixture::new("description-requirement");
    fixture.skill(&fixture.left, "alpha", "fixture");
    let custom = fixture.root.join("custom");
    let requiring = [
        ("native-project", fixture.project.join(".omp/skills")),
        ("native-user", fixture.home.join(".omp/agent/skills")),
        ("github", fixture.project.join(".github/skills")),
        ("custom", custom.clone()),
    ];
    let permitting = [
        ("claude", fixture.home.join(".claude/skills")),
        ("agents", fixture.project.join(".agent/skills")),
        ("codex", fixture.project.join(".codex/skills")),
        ("opencode", fixture.project.join(".opencode/skills")),
    ];
    for (provider, root) in requiring.iter().chain(&permitting) {
        fixture.skill(root, &format!("{provider}-described"), "fixture");
        fixture.skill_without_description(root, &format!("{provider}-bare"));
    }
    // `omp-managed` never claims a logical name, so its own requirement is observable only through
    // the deferral warning, which is emitted for the entries that survived the description gate.
    let managed = fixture.home.join(".omp/agent/managed-skills");
    fixture.skill(&managed, "managed-described", "fixture");
    fixture.skill_without_description(&managed, "managed-bare");
    fixture.custom_directories(&[&custom]);

    let output = fixture
        .discovery_command(&fixture.project)
        .output()
        .expect("asm should describe the OMP namespace");

    assert_success(&output);
    let scopes = discovery_scopes(&String::from_utf8_lossy(&output.stdout));
    for (provider, root) in &requiring {
        assert_eq!(
            scope_at(&scopes, &fixture, root).entry_names(),
            [format!("{provider}-described")],
            "{provider} requires a description"
        );
    }
    for (provider, root) in &permitting {
        assert_eq!(
            scope_at(&scopes, &fixture, root).entry_names(),
            [format!("{provider}-bare"), format!("{provider}-described")],
            "{provider} loads a Skill without a description"
        );
    }

    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        stderr.contains("OMP auto-learned Skill managed-described defers"),
        "{stderr}"
    );
    assert!(
        !stderr.contains("managed-bare"),
        "omp-managed requires a description too: {stderr}"
    );
}

/// Discovery is exactly one directory level of `<root>/<entry>/SKILL.md`, and nothing else loads.
///
/// Recursing would invent Skills OMP never loads and would pull paths outside the one-level scope
/// into the conflict inventory and the lock set. The dotted-name skip is what keeps a transaction's
/// own bookkeeping directory from being read back as a Skill, and an unreadable or empty `SKILL.md`
/// is dropped rather than claiming its directory name, exactly as OMP's `readFile` null path does
/// (`capability/fs.ts:23-33`, `discovery/helpers.ts:384-385`).
#[test]
fn discovery_reads_one_directory_level_and_skips_a_dotted_or_unreadable_entry() {
    let fixture = Fixture::new("entry-layout");
    fixture.skill(&fixture.left, "alpha", "fixture");
    let root = fixture.home.join(".omp/agent/skills");
    fixture.skill(&root, "plain", "fixture");
    fixture.skill(&root.join("nested"), "inner", "fixture");
    fixture.skill(&root, ".hidden", "fixture");
    fs::create_dir_all(root.join("without-metadata")).expect("entry without SKILL.md");
    fs::create_dir_all(root.join("empty-metadata")).expect("entry with an empty SKILL.md");
    fs::write(root.join("empty-metadata/SKILL.md"), "").expect("empty SKILL.md");

    let output = fixture
        .discovery_command(&fixture.project)
        .output()
        .expect("asm should describe the OMP namespace");

    assert_success(&output);
    let scopes = discovery_scopes(&String::from_utf8_lossy(&output.stdout));
    assert_eq!(
        scope_at(&scopes, &fixture, &root).entry_names(),
        ["plain"],
        "only a direct child holding a readable SKILL.md is a Skill"
    );
}

/// An entry OMP never loads still owns its path in the destination.
///
/// Occupancy answers whether a destination path is physically free, which is a filesystem question
/// rather than an OMP one, so it is recorded before every discovery filter. A plain file is not a
/// Skill and cannot be one, yet planning a link over it would describe a mutation apply must then
/// refuse - and `--conflict=skip` would silently drop a Skill whose name is not actually free.
#[test]
fn an_entry_omp_never_loads_still_occupies_its_destination_path() {
    let fixture = Fixture::new("destination-occupancy");
    fixture.skill(&fixture.left, "alpha", "fixture");
    let destination = fixture.destination();
    fs::create_dir_all(&destination).expect("destination fixture");
    fs::write(destination.join("alpha"), "not a Skill\n").expect("occupying regular file");

    for policy in ["error", "skip"] {
        let output = fixture
            .discovery_command(&fixture.project)
            .arg("--conflict")
            .arg(policy)
            .output()
            .expect("asm should refuse an occupied destination path");

        let stderr = String::from_utf8_lossy(&output.stderr);
        assert_eq!(
            output.status.code(),
            Some(73),
            "--conflict={policy}: {stderr}"
        );
        assert!(
            stderr.contains("alpha"),
            "--conflict={policy} must name the occupant: {stderr}"
        );
    }
}

/// A symbolic link inside a provider root is a first-class OMP discovery entry.
///
/// `helpers.ts:418,420` admits a link and then follows it, which is exactly what makes a
/// transaction-owned directory link loadable. Treating one as less would mean `SkillMount` plans
/// mounts OMP cannot read, and leaves a linked third-party Skill out of the conflict inventory.
#[test]
fn a_symlinked_entry_directory_is_a_first_class_discovery_entry() {
    let fixture = Fixture::new("symlinked-entry");
    fixture.skill(&fixture.left, "alpha", "fixture");
    let root = fixture.home.join(".omp/agent/skills");
    fixture.skill(&root, "plain", "fixture");
    let external = fixture.skill(&fixture.root.join("external"), "linked", "fixture");
    if !create_directory_link(&external, &root.join("linked")) {
        return;
    }

    let output = fixture
        .discovery_command(&fixture.project)
        .output()
        .expect("asm should describe the OMP namespace");

    assert_success(&output);
    let scopes = discovery_scopes(&String::from_utf8_lossy(&output.stdout));
    assert_eq!(
        scope_at(&scopes, &fixture, &root).entries,
        [
            ("linked".to_owned(), "directory link".to_owned()),
            ("plain".to_owned(), "regular directory".to_owned()),
        ]
    );
}

/// Two roots reaching one physical `SKILL.md` contribute one Skill, not a false duplicate.
///
/// The dedup key is the `realpath` of the `SKILL.md` file (`skills.ts:212` over `capSkill.path`,
/// which `helpers.ts:397` sets to that file), so two genuinely separate directories sharing one
/// linked `SKILL.md` are one Skill. Keying on the entry directory instead would let the custom
/// directory below override a provider Skill that is literally the same Skill, and the mount would
/// then be planned against a conflict that does not exist.
#[test]
fn two_roots_sharing_one_physical_skill_metadata_file_contribute_one_entry() {
    let fixture = Fixture::new("realpath-dedup");
    fixture.skill(&fixture.left, "alpha", "fixture");
    let claude = fixture.home.join(".claude/skills");
    let shared = fixture.skill(&claude, "shared", "fixture");
    let custom = fixture.root.join("custom");
    fixture.skill(&custom, "distinct", "fixture");
    // A real directory of its own, sharing only the metadata file. A custom directory overrides a
    // same-named provider Skill, so only the physical fold can keep this one out.
    let mirror = custom.join("shared");
    fs::create_dir_all(&mirror).expect("mirror entry");
    if !create_file_link(&shared.join("SKILL.md"), &mirror.join("SKILL.md")) {
        return;
    }
    fixture.custom_directories(&[&custom]);

    let output = fixture
        .discovery_command(&fixture.project)
        .output()
        .expect("asm should describe the OMP namespace");

    assert_success(&output);
    let scopes = discovery_scopes(&String::from_utf8_lossy(&output.stdout));
    assert_eq!(
        scope_at(&scopes, &fixture, &claude).entry_names(),
        ["shared"]
    );
    assert_eq!(
        scope_at(&scopes, &fixture, &custom).entry_names(),
        ["distinct"],
        "the physical fold precedes the custom-directory override"
    );
}

/// A custom directory overrides a provider's Skill of the same name, but never another custom one.
///
/// `skills.ts:303-314` lets a custom directory replace an already-loaded provider Skill, while
/// `skills.ts:316-318` keeps first-wins between two custom directories. Both halves decide which
/// on-disk entry owns a logical name, and therefore which one a mount would conflict with. The
/// first entry is spelled with a tilde, which `expandTilde` resolves against the user home
/// (`tools/path-utils.ts:142-152`); leaving it literal would drop the directory from the inventory
/// and the lock set while OMP still scanned it.
#[test]
fn a_custom_directory_overrides_a_provider_skill_but_not_another_custom_directory() {
    let fixture = Fixture::new("custom-override");
    fixture.skill(&fixture.left, "alpha", "fixture");
    let claude_user = fixture.home.join(".claude/skills");
    let claude_project = fixture.project.join(".claude/skills");
    fixture.skill(&claude_user, "shared", "fixture");
    fixture.skill(&claude_project, "shared", "fixture");
    fixture.skill(&claude_project, "own", "fixture");
    let first = fixture.home.join("custom-first");
    let second = fixture.root.join("custom-second");
    fixture.skill(&first, "shared", "fixture");
    fixture.skill(&second, "shared", "fixture");
    fixture.custom_directories(&[Path::new("~/custom-first"), &second]);

    let output = fixture
        .discovery_command(&fixture.project)
        .output()
        .expect("asm should describe the OMP namespace");

    assert_success(&output);
    let scopes = discovery_scopes(&String::from_utf8_lossy(&output.stdout));
    assert_eq!(
        scope_at(&scopes, &fixture, &claude_user).entry_names(),
        ["shared"]
    );
    assert_eq!(
        scope_at(&scopes, &fixture, &claude_project).entry_names(),
        ["own"],
        "a second ordinary provider loses a name already claimed"
    );
    assert_eq!(
        scope_at(&scopes, &fixture, &first).entry_names(),
        ["shared"],
        "a custom directory overrides a provider Skill"
    );
    assert_eq!(
        scope_at(&scopes, &fixture, &second).entry_names(),
        [] as [&str; 0],
        "a custom directory never overrides another custom directory"
    );
}

/// One row of the rendered `Discovery scopes:` block, with the entries listed beneath it.
struct RenderedScope {
    kind: String,
    root: String,
    entries: Vec<(String, String)>,
}

impl RenderedScope {
    fn entry_names(&self) -> Vec<&str> {
        self.entries.iter().map(|(name, _)| name.as_str()).collect()
    }
}

/// Parses the `Discovery scopes:` block a verbose read-only render emits.
///
/// That block is the adapter's own description of the OMP namespace, so reading it back pins
/// provider order and per-entry admission through the surface an operator actually sees rather than
/// through an internal shape a refactor could rename.
fn discovery_scopes(rendered: &str) -> Vec<RenderedScope> {
    let mut scopes: Vec<RenderedScope> = Vec::new();
    for line in rendered
        .lines()
        .skip_while(|line| *line != "Discovery scopes:")
        .skip(1)
        .take_while(|line| !line.is_empty())
    {
        // A scope row is `  {kind:<22} {state:<24} {path}`; every alias, link, terminal, and entry
        // row beneath one is indented further.
        if let Some(row) = line.strip_prefix("  ").filter(|row| !row.starts_with(' ')) {
            let (kind, rest) = row.split_at(22);
            let root = rest
                .get(26..)
                .unwrap_or_else(|| panic!("scope row is not in the rendered columns: {line}"));
            scopes.push(RenderedScope {
                kind: kind.trim_end().to_owned(),
                root: root.to_owned(),
                entries: Vec::new(),
            });
            continue;
        }
        let Some(row) = line
            .strip_prefix("      ")
            .filter(|row| !row.starts_with(' '))
        else {
            continue;
        };
        let name = row.split_whitespace().next().unwrap_or_default();
        if name == "terminal" || name.starts_with("link[") {
            continue;
        }
        scopes
            .last_mut()
            .expect("an entry row follows its scope row")
            .entries
            .push((name.to_owned(), row[name.len()..].trim().to_owned()));
    }
    scopes
}

/// Returns the single rendered scope for `root`.
fn scope_at<'a>(scopes: &'a [RenderedScope], fixture: &Fixture, root: &Path) -> &'a RenderedScope {
    let expected = rendered_root(fixture, root);
    let mut matching = scopes.iter().filter(|scope| scope.root == expected);
    let scope = matching
        .next()
        .unwrap_or_else(|| panic!("no rendered scope for {expected}"));
    assert!(
        matching.next().is_none(),
        "more than one rendered scope for {expected}"
    );
    scope
}

/// Renders a discovery root the way a report does: relative to the project root, with the host's
/// directory separator folded away.
///
/// A root is assembled from literal `a/b` suffixes on every platform, so a Windows root mixes both
/// separators. Folding keeps one expectation valid on every host.
fn rendered_root(fixture: &Fixture, root: &Path) -> String {
    root.strip_prefix(&fixture.project)
        .unwrap_or(root)
        .display()
        .to_string()
        .replace('\\', "/")
}

/// Creates a directory link, returning whether the host allowed it.
///
/// Windows needs Developer Mode or an elevated process, so a contributor without either still gets
/// a usable suite while `SKILLMOUNT_REQUIRE_LINKS` keeps CI from silently losing link coverage.
#[must_use]
fn create_directory_link(target: &Path, link: &Path) -> bool {
    #[cfg(unix)]
    let result = std::os::unix::fs::symlink(target, link);
    #[cfg(windows)]
    let result = std::os::windows::fs::symlink_dir(target, link);
    accept_link(result, link)
}

/// Creates a file link, returning whether the host allowed it.
#[must_use]
fn create_file_link(target: &Path, link: &Path) -> bool {
    #[cfg(unix)]
    let result = std::os::unix::fs::symlink(target, link);
    #[cfg(windows)]
    let result = std::os::windows::fs::symlink_file(target, link);
    accept_link(result, link)
}

fn accept_link(result: std::io::Result<()>, link: &Path) -> bool {
    if let Err(error) = result {
        assert!(
            std::env::var_os("SKILLMOUNT_REQUIRE_LINKS").is_none(),
            "required link fixture could not be created at {}: {error}",
            link.display()
        );
        return false;
    }
    true
}

fn assert_single_silent_last_tested_observation(fixture: &Fixture, stderr: &str) {
    let version_record =
        fs::read_to_string(&fixture.version_record).expect("version observation record");
    assert_eq!(
        version_record.lines().count(),
        1,
        "the version banner is observed exactly once"
    );
    assert_eq!(
        fs::canonicalize(PathBuf::from(recorded_os(&version_record, "cwd")))
            .expect("canonical version observation CWD"),
        fixture.root,
        "the observation uses the wrapper invocation CWD, not the child launch CWD"
    );
    assert!(
        !stderr.contains("version compatibility is unverified"),
        "the last-tested banner must not warn: {stderr}"
    );
}

fn assert_rejected_before_any_mutation(fixture: &Fixture, output: &Output, fragment: &str) {
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert_eq!(output.status.code(), Some(64), "{stderr}");
    assert!(stderr.contains(fragment), "{stderr}");
    assert!(
        !exists(&fixture.omp_scope()),
        "no OMP project directory is created"
    );
    assert!(!fixture.state.exists(), "no state directory is created");
    assert!(!fixture.record.exists(), "no child is launched");
}

fn assert_success(output: &Output) {
    assert_eq!(
        output.status.code(),
        Some(0),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
}

fn exists(path: &Path) -> bool {
    fs::symlink_metadata(path).is_ok()
}

fn recorded_os(record: &str, name: &str) -> OsString {
    let prefix = format!("{name}=");
    let encoded = record
        .lines()
        .find_map(|line| line.strip_prefix(&prefix))
        .unwrap_or_else(|| panic!("fake OMP record has no {name} entry"));
    native_from_hex(encoded)
}

fn recorded_os_values(record: &str, name: &str) -> Vec<OsString> {
    let prefix = format!("{name}=");
    record
        .lines()
        .filter_map(|line| line.strip_prefix(&prefix))
        .map(native_from_hex)
        .collect()
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

/// Every spelling of the planted entry point, relative to the root whose manifest declares it.
///
/// OMP resolves a manifest-declared extension path with `readFile` and imports whatever comes
/// back, with no extension filter at all (`discovery/helpers.ts:670-682`), so each of these is a
/// declaration OMP itself would honour rather than a shape invented for this test.
const SENTINEL_ENTRY_POINTS: [&str; 3] = ["./entry.js", "./entry.cmd", "./entry.ps1"];

/// The static-derivability rule is only real if a planted entry point would leave evidence.
///
/// `AGENTS.md` and ADR 0034 Decision 4 forbid an adapter from importing or executing third-party
/// Agent extension, plugin, or hook code. A passing discovery run proves nothing about that by
/// itself, because an implementation that asked a declared entry point what it contributes would
/// report the same inventory. So this fixture plants one execution-recording script at every point
/// where OMP 17.2.9 takes a path from attacker-authored state - the `omp.extensions` array of an
/// extension-package manifest (`discovery/helpers.ts:661-684`), the declared paths of
/// `.claude-plugin/plugin.json` (`discovery/claude-plugins.ts:34,195-203`), and a plugin hook
/// directory reached through a project `installed_plugins.json`
/// (`discovery/claude-plugins.ts:326-327`) - and then requires the evidence directory to still be
/// empty after two complete discovery passes through the real `asm` process.
#[test]
fn no_declared_extension_plugin_or_hook_entry_point_is_ever_executed() {
    let fixture = Fixture::new("static-derivability");
    fixture.skill(&fixture.left, "alpha", "mounted winner");
    let evidence = fixture.root.join("executed-third-party-code");
    fs::create_dir(&evidence).expect("evidence directory");

    let [user_package, project_package, plugin] = plant_declared_entry_points(&fixture, &evidence);

    let dry_run = fixture
        .command()
        .arg("--dry-run")
        .arg("--verbose")
        .arg("--")
        .arg("prompt")
        .output()
        .expect("asm should describe an OMP session over the planted extensions");
    assert_success(&dry_run);
    let planned = String::from_utf8_lossy(&dry_run.stdout);

    let mut command = Command::new(ASM);
    command
        .arg("inspect")
        .arg("--agent")
        .arg("omp")
        .arg("--skills-dir")
        .arg(&fixture.left);
    fixture.configure_environment(&mut command);
    // `inspect` accepts no `--project-root`, so it resolves every root from its invocation
    // directory, which `configure_environment` otherwise points at the fixture root.
    let inspection = command
        .current_dir(&fixture.project)
        .output()
        .expect("asm should inspect the OMP discovery model over the planted extensions");
    assert_success(&inspection);
    let inspected = String::from_utf8_lossy(&inspection.stdout);

    // Without this the sentinel assertion below would also pass for a fixture nothing ever reached.
    // Each probe Skill is visible only because discovery parsed the very manifest that declares the
    // entry point beside it, and both read-only paths report the declared root they came from.
    for (probe, root) in [
        ("probe-user-package", user_package.join("skills")),
        ("probe-project-extension", project_package.join("skills")),
        ("probe-marketplace-plugin", plugin.join("skills")),
    ] {
        assert!(
            planned.contains(probe),
            "a declaring manifest was never resolved, so the fixture proves nothing: {planned}"
        );
        let root = reported_path(&root, &fixture.project);
        for report in [&planned, &inspected] {
            assert!(
                report.contains(&root),
                "every read-only path must reach the declared root: {report}"
            );
        }
    }

    // The declared entry point itself is the sharpest evidence that this fixture is not vacuous:
    // both reports name the exact path the manifest declares, classified as a non-directory entry.
    // The adapter therefore resolved it and looked at it, which is what deriving instead of
    // executing looks like from the outside.
    let declared = reported_path(&plugin.join("entry.js"), &fixture.project);
    for report in [&planned, &inspected] {
        assert!(
            report.contains(&declared),
            "a declared entry point must be reported, not run: {report}"
        );
    }

    let mut executed = fs::read_dir(&evidence)
        .expect("evidence directory")
        .map(|entry| {
            entry
                .expect("evidence entry")
                .file_name()
                .to_string_lossy()
                .into_owned()
        })
        .collect::<Vec<_>>();
    executed.sort();
    assert!(
        executed.is_empty(),
        "an adapter must never import or execute third-party extension, plugin, or hook code, but \
         these declared entry points ran: {executed:?}"
    );
    assert!(!fixture.record.exists(), "no session child is launched");
    assert!(
        !exists(&fixture.destination()),
        "a dry run creates no mount destination"
    );
}

/// Plants one execution-recording entry point at every OMP declaration site, and returns the three
/// package roots that declare them: the user extension package, the project-declared extension
/// root, and the marketplace plugin reached through the project registry.
fn plant_declared_entry_points(fixture: &Fixture, evidence: &Path) -> [PathBuf; 3] {
    // An enabled user extension package. `packages_at` already parses this exact manifest to decide
    // enablement, so the declaration sits on a file the adapter demonstrably reads.
    let user_package = fixture.home.join(".omp/plugins/node_modules/user-package");
    plant_extension_package(&user_package, evidence, "user-package");
    fixture.skill(
        &user_package.join("skills"),
        "probe-user-package",
        "package skill",
    );
    write_declaration(
        &fixture.home.join(".omp/plugins/package.json"),
        "{\"dependencies\":{\"user-package\":\"1.0.0\"}}",
    );

    // A project-declared extension root, the shortest attacker-controlled route to a declared entry
    // point: `.omp/settings.json` lives inside the repository.
    let project_package = fixture.project.join("extensions/project-extension");
    plant_extension_package(&project_package, evidence, "project-extension");
    fixture.skill(
        &project_package.join("skills"),
        "probe-project-extension",
        "package skill",
    );
    write_declaration(
        &fixture.project.join(".omp/settings.json"),
        "{\"extensions\":[\"extensions/project-extension\"]}",
    );

    // A marketplace plugin reached through the project registry, with a hook inside it. `version`
    // has to be a JSON number or OMP discards the whole registry (`discovery/helpers.ts:798-804`),
    // which would silently make this arm of the fixture unreachable.
    let plugin = fixture
        .project
        .join(".omp/plugins/cache/plugins/shop___marketplace-plugin___1.0.0");
    plant_sentinel_scripts(&plugin, evidence, "marketplace-manifest");
    plant_sentinel_scripts(
        &plugin.join("hooks/session-start"),
        evidence,
        "marketplace-hook",
    );
    fixture.skill(
        &plugin.join("skills"),
        "probe-marketplace-plugin",
        "plugin skill",
    );
    write_declaration(
        &plugin.join(".claude-plugin/plugin.json"),
        &format!(
            "{{\"name\":\"marketplace-plugin\",\"version\":\"1.0.0\",\
             \"skills\":[\"./skills\",{}],\
             \"hooks\":\"./hooks/session-start/entry.js\"}}",
            declared_entry_points()
        ),
    );
    write_declaration(
        &fixture.project.join(".omp/plugins/installed_plugins.json"),
        &format!(
            "{{\"version\":1,\"plugins\":{{\"marketplace-plugin@shop\":\
             [{{\"installPath\":{:?},\"enabled\":true}}]}}}}",
            plugin.to_string_lossy()
        ),
    );

    [user_package, project_package, plugin]
}

/// Renders one path the way a read-only report renders it.
///
/// Both the session report and `inspect` shorten a path that lies inside the project root, so an
/// expectation written as a full path would only ever match the roots outside it.
fn reported_path(path: &Path, project_root: &Path) -> String {
    path.strip_prefix(project_root)
        .unwrap_or(path)
        .to_string_lossy()
        .into_owned()
}

/// Renders [`SENTINEL_ENTRY_POINTS`] as the interior of a JSON string array.
fn declared_entry_points() -> String {
    SENTINEL_ENTRY_POINTS
        .iter()
        .map(|entry| format!("{entry:?}"))
        .collect::<Vec<_>>()
        .join(",")
}

/// Installs one extension package whose `omp` manifest declares every planted entry point.
fn plant_extension_package(root: &Path, evidence: &Path, label: &str) {
    plant_sentinel_scripts(root, evidence, label);
    write_declaration(
        &root.join("package.json"),
        &format!(
            "{{\"name\":\"{label}\",\"version\":\"1.0.0\",\"main\":\"./entry.js\",\
             \"omp\":{{\"extensions\":[{}]}}}}",
            declared_entry_points()
        ),
    );
}

/// Writes every planted spelling of one entry point, each recording its own execution.
///
/// `entry.js` is the spelling OMP imports. Its first line is a shebang, which the kernel honours on
/// a direct spawn and which a JavaScript runtime strips as hashbang grammar, and its second line is
/// at once an `sh` command list and a JavaScript string expression followed by a line comment. One
/// declared path therefore records evidence whether it is spawned directly, handed to a shell, or
/// handed to `node`. `entry.cmd` and `entry.ps1` cover the Windows interpreters, which have no
/// notion of a shebang, so only the Unix spelling needs the executable bit.
fn plant_sentinel_scripts(root: &Path, evidence: &Path, label: &str) {
    let entry = root.join("entry.js");
    write_declaration(
        &entry,
        &format!(
            "#!/bin/sh\n\":\" //; : > '{}'; exit 0\nrequire('node:fs').writeFileSync('{}', '');\n",
            evidence.join(format!("{label}-spawned")).display(),
            evidence.join(format!("{label}-imported")).display()
        ),
    );
    write_declaration(
        &root.join("entry.cmd"),
        &format!(
            "@echo off\r\necho ran> \"{}\"\r\n",
            evidence.join(format!("{label}-cmd")).display()
        ),
    );
    write_declaration(
        &root.join("entry.ps1"),
        &format!(
            "New-Item -Force -ItemType File -Path '{}' | Out-Null\r\n",
            evidence.join(format!("{label}-ps1")).display()
        ),
    );
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;

        fs::set_permissions(&entry, fs::Permissions::from_mode(0o755))
            .expect("executable sentinel entry point");
    }
}

/// Writes one declarative fixture file, creating the directories that hold it.
fn write_declaration(path: &Path, contents: &str) {
    fs::create_dir_all(path.parent().expect("declaration parent")).expect("declaration directory");
    fs::write(path, contents).expect("declaration fixture");
}
