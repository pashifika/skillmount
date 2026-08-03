//! Cross-process smoke tests for the two installed executable names.

use std::fs;
use std::path::{Path, PathBuf};
use std::process::{Command, Output};
use std::time::{SystemTime, UNIX_EPOCH};

const ASM: &str = env!("CARGO_BIN_EXE_asm");
const SKILLMOUNT: &str = env!("CARGO_BIN_EXE_skillmount");
const RELEASE_CODEX_ENV: &str = "SKILLMOUNT_CLI_SMOKE_CODEX_BIN";

fn session_agent() -> PathBuf {
    if let Some(executable) = std::env::var_os(RELEASE_CODEX_ENV) {
        return PathBuf::from(executable);
    }

    #[cfg(debug_assertions)]
    return PathBuf::from(ASM);

    #[cfg(not(debug_assertions))]
    panic!("{RELEASE_CODEX_ENV} must name the explicit Codex fixture for a release smoke run");
}

fn run(executable: &str, arguments: &[&str]) -> Output {
    let home =
        std::env::temp_dir().join(format!("skillmount-cli-smoke-home-{}", std::process::id()));
    fs::create_dir_all(home.join("codex-home")).expect("smoke-test Codex home");
    Command::new(executable)
        .args(arguments)
        .env("HOME", &home)
        .env("USERPROFILE", &home)
        .env("SKILLMOUNT_TEST_CODEX_USER_HOME", &home)
        .env("SKILLMOUNT_TEST_CODEX_MANAGED_CONFIG", "absent")
        .env("LOCALAPPDATA", home.join("AppData/Local"))
        .env("CODEX_HOME", home.join("codex-home"))
        .env(
            "SKILLMOUNT_CODEX_ADMIN_SKILLS_DIR",
            home.join("admin-skills"),
        )
        .output()
        .expect("smoke-test executable should run")
}

/// Runs a mutating session against a throwaway project and a throwaway state root.
///
/// Both redirections are mandatory. Without `--project-root` the session would resolve the project
/// from the test harness's own working directory and mount into this repository, and without
/// `SKILLMOUNT_STATE_DIR` it would write journals and locks into the developer's real
/// application-support directory.
fn run_session(executable: &str, project: &Path, state: &Path, arguments: &[&str]) -> Output {
    let home = state.join("home");
    fs::create_dir_all(home.join("codex-home")).expect("session Codex home");
    Command::new(executable)
        .args(arguments)
        .arg("--project-root")
        .arg(project)
        .arg("--cwd")
        .arg(project)
        .arg("--agent-bin")
        .arg(session_agent())
        .arg("--")
        .arg("exec")
        .arg("fixture")
        .env("HOME", &home)
        .env("USERPROFILE", &home)
        .env("SKILLMOUNT_TEST_CODEX_USER_HOME", &home)
        .env("SKILLMOUNT_TEST_CODEX_MANAGED_CONFIG", "absent")
        .env("LOCALAPPDATA", home.join("AppData/Local"))
        .env("CODEX_HOME", home.join("codex-home"))
        .env("SKILLMOUNT_TEST_CODEX_VERSION", "codex-cli 0.146.0")
        .env(
            "SKILLMOUNT_FAKE_RECORD",
            state.join("cli-smoke-codex.record"),
        )
        .env("SKILLMOUNT_FAKE_BEHAVIOR", "exit")
        .env(
            "SKILLMOUNT_CODEX_ADMIN_SKILLS_DIR",
            home.join("admin-skills"),
        )
        .env("SKILLMOUNT_STATE_DIR", state)
        .current_dir(project)
        .output()
        .expect("smoke-test executable should run")
}

fn assert_successful_parity(arguments: &[&str]) -> Output {
    let asm = run(ASM, arguments);
    let skillmount = run(SKILLMOUNT, arguments);

    assert!(asm.status.success());
    assert_eq!(asm.status, skillmount.status);
    assert_eq!(asm.stdout, skillmount.stdout);
    assert_eq!(asm.stderr, skillmount.stderr);

    asm
}

#[test]
fn both_executable_names_share_help_output() {
    for arguments in [&[][..], &["--help"][..], &["-h"][..]] {
        let output = assert_successful_parity(arguments);
        let stdout = String::from_utf8_lossy(&output.stdout);
        assert!(stdout.contains("SkillMount"));
        assert!(stdout.contains("Usage: <asm|skillmount> <COMMAND>"));
    }
}

#[test]
fn both_executable_names_share_version_output() {
    for arguments in [["--version"], ["-V"]] {
        let output = assert_successful_parity(&arguments);
        assert_eq!(
            String::from_utf8_lossy(&output.stdout),
            format!("SkillMount {}\n", env!("CARGO_PKG_VERSION"))
        );
    }
}

#[test]
fn both_executable_names_reject_invalid_invocations_with_usage_status() {
    for arguments in [
        &["mount", "codex"][..],
        &["--unknown"][..],
        &["codex"][..],
        &["codex", "--skills-dir", "skills", "--unknown"][..],
    ] {
        let asm = run(ASM, arguments);
        let skillmount = run(SKILLMOUNT, arguments);

        assert_eq!(asm.status.code(), Some(64));
        assert_eq!(asm.status, skillmount.status);
        assert_eq!(asm.stdout, skillmount.stdout);
        assert_eq!(asm.stderr, skillmount.stderr);
        assert!(asm.stdout.is_empty());
        assert!(String::from_utf8_lossy(&asm.stderr).contains("error:"));
    }
}

fn temporary_fixture(label: &str) -> PathBuf {
    let nonce = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .expect("clock should be after epoch")
        .as_nanos();
    let path = std::env::temp_dir().join(format!(
        "skillmount-cli-{label}-{}-{nonce}",
        std::process::id()
    ));
    fs::create_dir_all(&path).expect("fixture should be created");
    path
}

#[test]
fn wrapper_maps_missing_and_invalid_catalog_inputs_to_stable_codes() {
    let fixture = temporary_fixture("errors");
    let project = fixture.join("project");
    let state = fixture.join("state");
    fs::create_dir(&project).expect("throwaway project fixture");
    let missing = fixture.join("missing");
    let missing_output = run_session(
        ASM,
        &project,
        &state,
        &["codex", "--skills-dir", &missing.to_string_lossy()],
    );
    assert_eq!(missing_output.status.code(), Some(66));
    assert!(String::from_utf8_lossy(&missing_output.stderr).contains(&*missing.to_string_lossy()));
    assert!(
        !project.join(".agents").exists() && !project.join(".codex").exists(),
        "a missing catalog must not mutate the throwaway project"
    );
    assert_eq!(
        journal_count(&state),
        0,
        "catalog validation never opens a transaction journal"
    );

    let invalid = fixture.join("invalid");
    fs::create_dir(&invalid).expect("invalid Skill fixture");
    fs::write(invalid.join("SKILL.md"), "not frontmatter\n").expect("invalid SKILL.md");
    let invalid_output = run_session(
        ASM,
        &project,
        &state,
        &["codex", "--skills-dir", &invalid.to_string_lossy()],
    );
    assert_eq!(invalid_output.status.code(), Some(65));
    assert!(String::from_utf8_lossy(&invalid_output.stderr).contains("invalid selected Skill"));
    assert!(
        !project.join(".agents").exists() && !project.join(".codex").exists(),
        "an invalid catalog must not mutate the throwaway project"
    );
    assert_eq!(journal_count(&state), 0);

    fs::remove_dir_all(fixture).expect("fixture cleanup");
}

#[test]
fn inspect_and_codex_session_preserve_binary_parity() {
    let fixture = temporary_fixture("inspect");
    let skill = fixture.join("demo");
    fs::create_dir(&skill).expect("Skill fixture");
    fs::write(
        skill.join("SKILL.md"),
        "---\nname: demo\ndescription: demo description\n---\nbody\n",
    )
    .expect("valid SKILL.md");

    let inspect = run(ASM, &["inspect", "--skills-dir", &skill.to_string_lossy()]);
    assert!(inspect.status.success());
    let inspected = String::from_utf8_lossy(&inspect.stdout);
    assert!(inspected.contains("Overlay: 1 Skill(s), 0 source override(s)"));
    assert!(
        inspected.contains("Agent:          codex") && inspected.contains("Agent:          claude"),
        "the default inspection covers every agent: {inspected}"
    );
    assert!(inspected.contains("Effective argv:"));

    let dry_run_arguments = [
        "codex",
        "--skills-dir",
        &skill.to_string_lossy(),
        "--dry-run",
        "--",
        "exec",
        "fixture",
    ];
    let dry_run = run(ASM, &dry_run_arguments);
    assert!(dry_run.status.success(), "a dry run completes normally");
    let planned = String::from_utf8_lossy(&dry_run.stdout);
    assert!(planned.contains("LINK"), "the plan is rendered: {planned}");
    assert!(
        !fixture.join(".codex").exists() && !fixture.join(".agents").exists(),
        "a dry run creates nothing"
    );

    let project = fixture.join("project");
    fs::create_dir(&project).expect("project fixture");
    let state = fixture.join("state");
    let session_arguments = ["codex", "--skills-dir", &skill.to_string_lossy()];
    let session = run_session(ASM, &project, &state, &session_arguments);
    let fallback_session = run_session(SKILLMOUNT, &project, &state, &session_arguments);

    if std::env::var_os(RELEASE_CODEX_ENV).is_some() {
        assert!(
            session.status.success(),
            "the release smoke fixture is a supported Codex executable: {}",
            String::from_utf8_lossy(&session.stderr)
        );
        assert!(
            state.join("cli-smoke-codex.record").is_file(),
            "the release smoke fixture records the child launch"
        );
    } else {
        assert_eq!(session.status.code(), Some(64));
        assert!(
            String::from_utf8_lossy(&session.stderr)
                .contains("discovery does not grant sandbox access"),
            "the debug session reports permission separation: {}",
            String::from_utf8_lossy(&session.stderr)
        );
    }
    assert_eq!(session.status, fallback_session.status);
    assert_eq!(session.stdout, fallback_session.stdout);
    assert_eq!(session.stderr, fallback_session.stderr);
    assert!(
        !project.join(".agents").exists() && !project.join(".codex").exists(),
        "a completed session releases everything it applied"
    );
    assert!(
        journal_count(&state) == 0,
        "a completed transaction leaves no journal behind"
    );

    fs::remove_dir_all(fixture).expect("fixture cleanup");
}

/// Counts journals left in a redirected state root.
fn journal_count(state: &Path) -> usize {
    fs::read_dir(state.join("transactions")).map_or(0, |entries| {
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
