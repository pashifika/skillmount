//! Cross-process smoke tests for the two installed executable names.

use std::process::{Command, Output};

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
        assert!(stdout.contains("Usage: <asm|skillmount> [OPTIONS]"));
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
fn both_executable_names_reject_unsupported_invocations() {
    for arguments in [
        &["mount", "codex"][..],
        &["--unknown"][..],
        &["--help", "extra"][..],
    ] {
        let asm = run(ASM, arguments);
        let skillmount = run(SKILLMOUNT, arguments);

        assert_eq!(asm.status.code(), Some(2));
        assert_eq!(asm.status, skillmount.status);
        assert_eq!(asm.stdout, skillmount.stdout);
        assert_eq!(asm.stderr, skillmount.stderr);
        assert!(asm.stdout.is_empty());
        assert!(String::from_utf8_lossy(&asm.stderr).contains("not implemented yet"));
    }
}
