//! OMP native-directory sessions preserve settings without creating Skill links.
#![cfg(feature = "test-fixtures")]

use std::fs;
use std::path::{Path, PathBuf};
use std::process::{Command, Output};
use std::time::{SystemTime, UNIX_EPOCH};

struct Fixture {
    root: PathBuf,
    project: PathBuf,
    home: PathBuf,
    left: PathBuf,
    right: PathBuf,
    state: PathBuf,
    captured: PathBuf,
}
impl Fixture {
    fn new(label: &str) -> Self {
        let nonce = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let root = std::env::temp_dir().join(format!(
            "skillmount-native-omp-{label}-{}-{nonce}",
            std::process::id()
        ));
        fs::create_dir_all(&root).unwrap();
        let root = fs::canonicalize(root).unwrap();
        let result = Self {
            project: root.join("project"),
            home: root.join("home"),
            left: root.join("left"),
            right: root.join("right"),
            state: root.join("state"),
            captured: root.join("captured.json"),
            root,
        };
        for dir in [&result.project, &result.home, &result.left, &result.right] {
            fs::create_dir_all(dir).unwrap();
        }
        fs::create_dir(result.project.join(".git")).unwrap();
        fs::create_dir(result.home.join(".git")).unwrap();
        result
    }
    fn write(&self, path: &Path, body: &str) {
        assert!(path.starts_with(&self.root));
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(path, body).unwrap();
    }
    fn command(&self) -> Command {
        let mut command = Command::new(env!("CARGO_BIN_EXE_asm"));
        command
            .args(["omp", "--skills-dir"])
            .arg(&self.left)
            .arg("--skills-dir")
            .arg(&self.right)
            .arg("--project-root")
            .arg(&self.project)
            .arg("--cwd")
            .arg(&self.project)
            .arg("--agent-bin")
            .arg(env!("CARGO_BIN_EXE_skillmount-fake-agent"))
            .env("HOME", &self.home)
            .env("USERPROFILE", &self.home)
            .env("LOCALAPPDATA", self.home.join("AppData/Local"))
            .env("SKILLMOUNT_STATE_DIR", &self.state)
            .env("SKILLMOUNT_FAKE_RECORD", self.root.join("agent.record"))
            .env("SKILLMOUNT_FAKE_OMP_CONFIG_RECORD", &self.captured)
            .env("SKILLMOUNT_FAKE_BEHAVIOR", "exit")
            .current_dir(&self.root);
        for name in [
            "OMP_PROFILE",
            "PI_PROFILE",
            "PI_CONFIG_FILES",
            "PI_CONFIG_DIR",
            "PI_CODING_AGENT_DIR",
            "XDG_DATA_HOME",
        ] {
            command.env_remove(name);
        }
        command
    }
    fn captured(&self) -> serde_json::Value {
        serde_json::from_slice(&fs::read(&self.captured).unwrap()).unwrap()
    }
}
impl Drop for Fixture {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.root);
    }
}
fn success(output: &Output) {
    assert!(
        output.status.success(),
        "stdout={} stderr={}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
}

#[test]
fn native_overlay_preserves_configured_order_and_adds_cli_roots_without_links() {
    let f = Fixture::new("session");
    let config = f.project.join(".omp/config.yml");
    let body =
        "skills:\n  customDirectories: [~/existing, relative-root]\n  ignoredSkills: [hidden]\n";
    f.write(&config, body);
    f.write(
        &f.left.join("same/SKILL.md"),
        "not SkillMount metadata: OMP owns parsing",
    );
    f.write(&f.right.join("same/SKILL.md"), "different duplicate");
    let output = f
        .command()
        .arg("--")
        .args(["--skills", "same", "-p", "hello"])
        .output()
        .unwrap();
    success(&output);
    assert_eq!(
        f.captured()["skills"]["customDirectories"],
        serde_json::json!(["~/existing", "relative-root", f.left, f.right])
    );
    assert_eq!(fs::read_to_string(config).unwrap(), body);
    assert!(!f.project.join(".omp/skills").exists());
    assert!(f.root.join("agent.record").exists());
    let sessions = f.state.join("sessions");
    if sessions.exists() {
        assert_eq!(fs::read_dir(sessions).unwrap().count(), 0);
    }
}

#[test]
fn dry_run_is_mutation_free_and_reports_native_delegation() {
    let f = Fixture::new("dry");
    let output = f
        .command()
        .args(["--dry-run", "--verbose"])
        .output()
        .unwrap();
    success(&output);
    let text = String::from_utf8_lossy(&output.stdout);
    assert!(text.contains("delegated"), "{text}");
    assert!(text.contains("--config"), "{text}");
    assert!(!f.state.exists());
    assert!(!f.captured.exists());
    assert!(!f.project.join(".omp").exists());
}

#[test]
fn settings_layers_replace_arrays_but_do_not_replace_other_user_settings() {
    let f = Fixture::new("precedence");
    f.write(
        &f.home.join(".omp/agent/config.yml"),
        "skills:\n  customDirectories: [global]\n",
    );
    f.write(
        &f.project.join(".omp/config.yml"),
        "skills:\n  customDirectories: [native]\n",
    );
    f.write(
        &f.project.join(".codex/config.toml"),
        "[skills]\ncustomDirectories = ['codex']\n",
    );
    f.write(
        &f.project.join(".opencode/opencode.jsonc"),
        r#"{"skills":{"customDirectories":["opencode"]}}"#,
    );
    success(&f.command().output().unwrap());
    assert_eq!(
        f.captured()["skills"]["customDirectories"],
        serde_json::json!(["opencode", f.left, f.right])
    );
}

#[test]
fn individual_skill_root_is_rejected_without_broadening_to_siblings() {
    let f = Fixture::new("leaf");
    f.write(&f.left.join("SKILL.md"), "leaf");
    let output = f.command().output().unwrap();
    assert!(!output.status.success());
    assert!(String::from_utf8_lossy(&output.stderr).contains("collection"));
    assert!(!f.state.exists());
}

#[test]
fn native_irrelevant_mount_and_validation_options_fail_before_state_creation() {
    for args in [
        vec!["--link-mode", "symlink"],
        vec!["--keep-mounts"],
        vec!["--conflict", "skip"],
        vec!["--validation", "none"],
    ] {
        let f = Fixture::new("options");
        let output = f.command().args(args).output().unwrap();
        assert!(!output.status.success());
        assert!(!f.state.exists());
        assert!(!f.captured.exists());
    }
}

#[test]
fn unsupported_config_inputs_fail_closed_without_mutation() {
    for (relative, body) in [
        ("opencode.jsonc", "{ // comment\n}"),
        (
            "opencode.json",
            r#"{"skills":{"customDirectories":["{env:ROOT}"]}}"#,
        ),
        (
            ".omp/config.yml",
            "skills:\n  customDirectories: not-an-array\n",
        ),
    ] {
        let f = Fixture::new("unsupported");
        f.write(&f.project.join(relative), body);
        let output = f.command().output().unwrap();
        assert!(!output.status.success());
        assert!(String::from_utf8_lossy(&output.stderr).contains("cannot preserve OMP"));
        assert!(!f.state.exists());
        assert_eq!(fs::read_to_string(f.project.join(relative)).unwrap(), body);
    }
}

#[test]
fn relocated_or_user_overlay_configuration_is_rejected() {
    for variable in ["OMP_PROFILE", "PI_PROFILE", "PI_CONFIG_FILES"] {
        let f = Fixture::new("env");
        let output = f.command().env(variable, "other").output().unwrap();
        assert!(!output.status.success());
        assert!(!f.state.exists());
    }
    let f = Fixture::new("cli-overlay");
    let output = f
        .command()
        .arg("--")
        .args(["--config", "other.yml"])
        .output()
        .unwrap();
    assert!(!output.status.success());
    assert!(!f.state.exists());
}

#[test]
fn new_non_session_commands_and_resume_modes_are_rejected() {
    for args in [
        vec!["skill", "list"],
        vec!["auth"],
        vec!["session"],
        vec!["--restore"],
        vec!["--trusted-extension", "ext.ts"],
    ] {
        let f = Fixture::new("non-session");
        let output = f.command().arg("--").args(args).output().unwrap();
        assert!(!output.status.success());
        assert!(!f.state.exists());
    }
}

#[test]
fn native_filter_settings_are_not_emulated_or_overwritten() {
    let f = Fixture::new("filters");
    f.write(
        &f.project.join(".omp/config.yml"),
        "skills:\n  enabled: false\n  includeSkills: [missing]\n  ignoredSkills: ['*']\n",
    );
    success(&f.command().output().unwrap());
    assert_eq!(
        f.captured(),
        serde_json::json!({"skills":{"customDirectories":[f.left,f.right]}})
    );
}

#[test]
fn legacy_database_requires_migration_unless_yaml_exists() {
    let f = Fixture::new("legacy-db");
    let agent_dir = f.home.join(".omp/agent");
    fs::create_dir_all(&agent_dir).unwrap();
    fs::write(agent_dir.join("agent.db"), [0xff, 0, 0xfe]).unwrap();
    let output = f.command().output().unwrap();
    assert!(!output.status.success());
    assert!(String::from_utf8_lossy(&output.stderr).contains("legacy settings"));
    assert!(!f.state.exists());
    f.write(&agent_dir.join("config.yml"), "");
    success(&f.command().output().unwrap());
}

#[test]
fn absolute_config_directory_is_rejected_before_state_access() {
    let f = Fixture::new("absolute-config-dir");
    let output = f.command().env("PI_CONFIG_DIR", &f.home).output().unwrap();
    assert!(!output.status.success());
    assert!(String::from_utf8_lossy(&output.stderr).contains("absolute PI_CONFIG_DIR"));
    assert!(!f.state.exists());
}

#[cfg(windows)]
#[test]
fn drive_relative_config_directory_is_rejected_before_state_access() {
    let f = Fixture::new("drive-relative-config-dir");
    let output = f
        .command()
        .env("PI_CONFIG_DIR", "C:config")
        .output()
        .unwrap();
    assert!(!output.status.success());
    assert!(String::from_utf8_lossy(&output.stderr).contains("drive prefix"));
    assert!(!f.state.exists());
}
