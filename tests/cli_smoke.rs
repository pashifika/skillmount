//! Cross-process smoke tests for the two installed executable names.

use std::fs;
use std::path::PathBuf;
use std::process::{Command, Output};
use std::time::{SystemTime, UNIX_EPOCH};

const ASM: &str = env!("CARGO_BIN_EXE_asm");
const SKILLMOUNT: &str = env!("CARGO_BIN_EXE_skillmount");

fn run(executable: &str, arguments: &[&str]) -> Output {
    Command::new(executable)
        .args(arguments)
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
    let missing = fixture.join("missing");
    let missing_output = run(ASM, &["codex", "--skills-dir", &missing.to_string_lossy()]);
    assert_eq!(missing_output.status.code(), Some(66));
    assert!(String::from_utf8_lossy(&missing_output.stderr).contains(&*missing.to_string_lossy()));

    let invalid = fixture.join("invalid");
    fs::create_dir(&invalid).expect("invalid Skill fixture");
    fs::write(invalid.join("SKILL.md"), "not frontmatter\n").expect("invalid SKILL.md");
    let invalid_output = run(ASM, &["codex", "--skills-dir", &invalid.to_string_lossy()]);
    assert_eq!(invalid_output.status.code(), Some(65));
    assert!(String::from_utf8_lossy(&invalid_output.stderr).contains("invalid selected Skill"));

    fs::remove_dir_all(fixture).expect("fixture cleanup");
}

#[test]
fn inspect_resolves_without_crossing_the_unimplemented_mount_boundary() {
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
    ];
    let dry_run = run(ASM, &dry_run_arguments);
    assert!(dry_run.status.success(), "a dry run completes normally");
    let planned = String::from_utf8_lossy(&dry_run.stdout);
    assert!(planned.contains("LINK"), "the plan is rendered: {planned}");
    assert!(
        !fixture.join(".codex").exists() && !fixture.join(".agents").exists(),
        "a dry run creates nothing"
    );

    let session_arguments = ["codex", "--skills-dir", &skill.to_string_lossy()];
    let session = run(ASM, &session_arguments);
    let fallback_session = run(SKILLMOUNT, &session_arguments);
    assert_eq!(session.status.code(), Some(70));
    assert_eq!(session.status, fallback_session.status);
    assert_eq!(session.stdout, fallback_session.stdout);
    assert_eq!(session.stderr, fallback_session.stderr);
    assert!(
        String::from_utf8_lossy(&session.stderr).contains("reserved for later changes"),
        "a normal session still stops at the mutation boundary"
    );

    fs::remove_dir_all(fixture).expect("fixture cleanup");
}
