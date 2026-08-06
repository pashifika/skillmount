//! End-to-end OMP session acceptance through the real `asm` process.

#![cfg(feature = "test-fixtures")]

use std::ffi::OsString;
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
