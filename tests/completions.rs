//! Shipped-binary contract tests for static shell completion generation.

use std::fs;
#[cfg(unix)]
use std::os::unix::process::CommandExt;
use std::path::{Path, PathBuf};
use std::process::{Command, Output};
use std::sync::atomic::{AtomicU64, Ordering};

const ASM: &str = env!("CARGO_BIN_EXE_asm");
const SKILLMOUNT: &str = env!("CARGO_BIN_EXE_skillmount");
const PRODUCTS: [(&str, &str); 2] = [(ASM, "asm"), (SKILLMOUNT, "skillmount")];
const SHELLS: [&str; 4] = ["bash", "zsh", "fish", "powershell"];
static NEXT_FIXTURE: AtomicU64 = AtomicU64::new(0);

struct Fixture {
    root: PathBuf,
}

impl Fixture {
    fn new(label: &str) -> Self {
        let sequence = NEXT_FIXTURE.fetch_add(1, Ordering::Relaxed);
        let root = std::env::temp_dir().join(format!(
            "skillmount-completions-{label}-{}-{sequence}",
            std::process::id()
        ));
        fs::create_dir(&root).expect("completion fixture should be created");
        Self { root }
    }
}

impl Drop for Fixture {
    fn drop(&mut self) {
        fs::remove_dir_all(&self.root).expect("completion fixture should be removed");
    }
}

fn run(binary: &str, arguments: &[&str], current_dir: &Path) -> Output {
    Command::new(binary)
        .args(arguments)
        .current_dir(current_dir)
        .output()
        .expect("completion command should run")
}

#[cfg(unix)]
fn command_with_argv0(binary: &str, argv0: &Path) -> Command {
    let mut command = Command::new(binary);
    command.arg0(argv0);
    command
}

#[cfg(windows)]
fn command_with_argv0(binary: &str, argv0: &Path) -> Command {
    fs::copy(binary, argv0).expect("renamed executable fixture should be copied");
    Command::new(argv0)
}

fn successful_script(binary: &str, shell: &str, current_dir: &Path) -> Vec<u8> {
    let output = run(binary, &["completions", shell], current_dir);
    assert!(
        output.status.success(),
        "{binary} {shell}: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(output.stderr.is_empty(), "{binary} {shell}");
    assert!(!output.stdout.is_empty(), "{binary} {shell}");
    output.stdout
}

fn registration_marker(shell: &str, product: &str) -> String {
    match shell {
        "bash" => format!("complete -F _{product}"),
        "zsh" => format!("#compdef {product}"),
        "fish" => format!("complete -c {product}"),
        "powershell" => format!("Register-ArgumentCompleter -Native -CommandName '{product}'"),
        _ => panic!("test shell should be supported"),
    }
}

#[test]
fn shipped_binaries_generate_every_supported_shell() {
    let fixture = Fixture::new("matrix");

    for (binary, _) in PRODUCTS {
        for shell in SHELLS {
            successful_script(binary, shell, &fixture.root);
        }
    }
}

#[test]
fn generated_scripts_bind_only_the_invoked_product_name() {
    let fixture = Fixture::new("identity");

    for (binary, product) in PRODUCTS {
        let other = if product == "asm" {
            "skillmount"
        } else {
            "asm"
        };
        for shell in SHELLS {
            let script = successful_script(binary, shell, &fixture.root);
            let script = String::from_utf8(script).expect("generated script should be UTF-8");
            assert!(
                script.contains(&registration_marker(shell, product)),
                "missing {shell} registration for {product}"
            );
            assert!(
                !script.contains(other),
                "{shell} leaked {other} into {product}"
            );
            assert!(!script.contains("<asm|skillmount>"), "{shell} {product}");
        }
    }
}

#[test]
fn completion_generation_is_byte_deterministic() {
    let fixture = Fixture::new("determinism");

    for (binary, product) in PRODUCTS {
        for shell in SHELLS {
            let first = successful_script(binary, shell, &fixture.root);
            let second = successful_script(binary, shell, &fixture.root);
            assert_eq!(first, second, "{product} {shell}");
        }
    }
}

#[test]
fn completion_generation_needs_no_project_or_skill_source() {
    let fixture = Fixture::new("no-project");

    for (binary, product) in PRODUCTS {
        let output = run(binary, &["completions", "zsh"], &fixture.root);
        assert!(
            output.status.success(),
            "{product}: {}",
            String::from_utf8_lossy(&output.stderr)
        );
        assert!(!output.stdout.is_empty(), "{product}");
        assert!(output.stderr.is_empty(), "{product}");
        assert_eq!(
            fs::read_dir(&fixture.root)
                .expect("fixture should remain readable")
                .count(),
            0,
            "{product} created state outside a project"
        );
    }
}

#[test]
fn completion_usage_failures_have_stable_categories() {
    let fixture = Fixture::new("usage");

    for (binary, product) in PRODUCTS {
        for arguments in [
            &["completions"][..],
            &["completions", "elvish"][..],
            &["completions", "nushell"][..],
        ] {
            let output = run(binary, arguments, &fixture.root);
            assert_eq!(output.status.code(), Some(64), "{product} {arguments:?}");
            assert!(output.stdout.is_empty(), "{product} {arguments:?}");
            assert!(!output.stderr.is_empty(), "{product} {arguments:?}");
        }
    }

    let renamed = fixture.root.join(if cfg!(windows) {
        "renamed-skillmount.exe"
    } else {
        "renamed-skillmount"
    });
    let output = command_with_argv0(ASM, &renamed)
        .args(["completions", "bash"])
        .current_dir(&fixture.root)
        .output()
        .expect("renamed executable should run");
    assert_eq!(output.status.code(), Some(64));
    assert!(output.stdout.is_empty());
    let stderr = String::from_utf8(output.stderr).expect("diagnostic should be UTF-8");
    assert!(stderr.contains("`asm`"), "{stderr}");
    assert!(stderr.contains("`skillmount`"), "{stderr}");
}

#[test]
fn generated_scripts_expose_the_shared_command_graph() {
    let fixture = Fixture::new("graph");

    for shell in SHELLS {
        let script = successful_script(ASM, shell, &fixture.root);
        let script = String::from_utf8(script).expect("generated script should be UTF-8");
        for expected in [
            "codex",
            "claude",
            "omp",
            "inspect",
            "doctor",
            "cleanup",
            "completions",
        ] {
            assert!(script.contains(expected), "{shell} omitted {expected}");
        }
        let option_markers = if shell == "fish" {
            [
                "-l skills-dir",
                "-l project-root",
                "-l agent-bin",
                "-l omp-bin",
            ]
        } else {
            ["--skills-dir", "--project-root", "--agent-bin", "--omp-bin"]
        };
        for expected in option_markers {
            assert!(script.contains(expected), "{shell} omitted {expected}");
        }
    }
}
