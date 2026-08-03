//! Executable-seam coverage for operator diagnostics and recovery commands.

use std::collections::BTreeMap;
use std::fs;
use std::path::{Path, PathBuf};
use std::process::{Command, Output};
use std::time::{SystemTime, UNIX_EPOCH};

const ASM: &str = env!("CARGO_BIN_EXE_asm");

struct Fixture {
    root: PathBuf,
    project: PathBuf,
    home: PathBuf,
    state: PathBuf,
}

impl Fixture {
    fn new(label: &str) -> Self {
        let nonce = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .expect("clock should be after the epoch")
            .as_nanos();
        let root = std::env::temp_dir().join(format!(
            "skillmount-operator-{label}-{}-{nonce}",
            std::process::id()
        ));
        let project = root.join("project");
        let home = root.join("home");
        let state = root.join("state");
        for path in [&project, &home, &root.join("codex-home")] {
            fs::create_dir_all(path).expect("operator fixture directory");
        }
        Self {
            root,
            project,
            home,
            state,
        }
    }

    fn command(&self) -> Command {
        let mut command = Command::new(ASM);
        command
            .current_dir(&self.project)
            .env("HOME", &self.home)
            .env("USERPROFILE", &self.home)
            .env("SKILLMOUNT_TEST_CODEX_USER_HOME", &self.home)
            .env("SKILLMOUNT_TEST_CODEX_MANAGED_CONFIG", "absent")
            .env("LOCALAPPDATA", self.home.join("AppData/Local"))
            .env("CODEX_HOME", self.root.join("codex-home"))
            .env("SKILLMOUNT_TEST_CODEX_VERSION", "codex-cli 0.146.0")
            .env("SKILLMOUNT_TEST_CLAUDE_VERSION", "2.1.220 (Claude Code)")
            .env("CLAUDE_CONFIG_DIR", self.home.join(".claude"))
            .env(
                "SKILLMOUNT_CLAUDE_MANAGED_SKILLS_DIR",
                self.root.join("claude-managed/skills"),
            )
            .env_remove("CLAUDE_CODE_SAFE_MODE")
            .env_remove("CLAUDE_CODE_SIMPLE")
            .env(
                "SKILLMOUNT_CODEX_ADMIN_SKILLS_DIR",
                self.root.join("admin-skills"),
            )
            .env("SKILLMOUNT_STATE_DIR", &self.state);
        command
    }

    fn doctor(&self) -> Output {
        self.doctor_command()
            .output()
            .expect("asm doctor should run")
    }

    fn doctor_command(&self) -> Command {
        let mut command = self.command();
        command
            .arg("doctor")
            .arg("--project-root")
            .arg(&self.project)
            .arg("--codex-bin")
            .arg(ASM)
            .arg("--claude-bin")
            .arg(ASM);
        command
    }

    fn path_agent_directory(&self) -> PathBuf {
        let directory = self.root.join("agent-path");
        fs::create_dir(&directory).expect("agent PATH fixture");
        install_agent_alias(&directory, "codex");
        install_agent_alias(&directory, "claude");
        directory
    }
}

impl Drop for Fixture {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.root);
    }
}

#[test]
fn healthy_doctor_reports_versions_and_leaves_project_and_state_untouched() {
    let fixture = Fixture::new("doctor-healthy");
    let project_before = snapshot(&fixture.project);

    let output = fixture.doctor();

    assert!(
        output.status.success(),
        "doctor should accept the supported fixture: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(output.stderr.is_empty(), "doctor findings belong on stdout");
    let rendered = String::from_utf8_lossy(&output.stdout);
    assert!(rendered.contains("SkillMount doctor"));
    assert!(rendered.contains("[PASS] codex executable"));
    assert!(rendered.contains("codex-cli 0.146.0"));
    assert!(rendered.contains("[PASS] claude executable"));
    assert!(rendered.contains("2.1.220 (Claude Code)"));
    #[cfg(not(windows))]
    {
        assert!(rendered.contains("[PASS] symlink capability"));
        assert!(rendered.contains("[UNVERIFIED] junction capability"));
    }
    #[cfg(windows)]
    {
        assert!(
            rendered.contains("[PASS] symlink capability")
                || rendered.contains("[WARN] symlink capability")
        );
        assert!(rendered.contains("[PASS] junction capability"));
    }
    assert!(rendered.contains("0 failure"));
    assert_eq!(snapshot(&fixture.project), project_before);
    assert!(
        !fixture.state.exists(),
        "a read-only doctor pass must not create SkillMount state"
    );
}

#[test]
fn doctor_resolves_both_agent_executables_from_path() {
    let fixture = Fixture::new("doctor-path");
    let agent_path = fixture.path_agent_directory();

    let output = fixture
        .command()
        .arg("doctor")
        .arg("--project-root")
        .arg(&fixture.project)
        .env("PATH", agent_path)
        .output()
        .expect("PATH doctor should run");

    assert!(
        output.status.success(),
        "PATH agents should be accepted: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    let rendered = String::from_utf8_lossy(&output.stdout);
    assert!(rendered.contains("[PASS] codex executable"));
    assert!(rendered.contains("[PASS] claude executable"));
}

#[test]
fn unsupported_agent_version_is_a_failing_finding_with_stable_status() {
    let fixture = Fixture::new("doctor-version-failure");

    let output = fixture
        .doctor_command()
        .env("SKILLMOUNT_TEST_CODEX_VERSION", "codex-cli 999.0.0")
        .output()
        .expect("version-failure doctor should run");

    assert_eq!(output.status.code(), Some(65));
    assert!(output.stderr.is_empty());
    let rendered = String::from_utf8_lossy(&output.stdout);
    assert!(rendered.contains("[FAIL] codex executable"));
    assert!(rendered.contains("codex-cli 999.0.0"));
    assert!(rendered.contains("1 failure"));
}

#[test]
fn duplicate_visible_skill_is_a_warning_without_failing_doctor() {
    let fixture = Fixture::new("doctor-warning");
    for root in [".agents/skills", ".codex/skills"] {
        let skill = fixture.project.join(root).join("duplicate");
        fs::create_dir_all(&skill).expect("duplicate discovery fixture");
        fs::write(
            skill.join("SKILL.md"),
            "---\nname: duplicate\ndescription: duplicate fixture\n---\n",
        )
        .expect("duplicate Skill metadata");
    }

    let output = fixture.doctor();

    assert!(
        output.status.success(),
        "warnings alone do not fail doctor: {}",
        String::from_utf8_lossy(&output.stdout)
    );
    let rendered = String::from_utf8_lossy(&output.stdout);
    assert!(rendered.contains("[WARN] codex discovery"));
    assert!(rendered.contains("logical Skill duplicate"));
    assert!(rendered.contains("0 failure"));
}

#[cfg(unix)]
#[test]
fn broken_project_discovery_link_reports_the_exact_chain_without_mutation() {
    use std::os::unix::fs::symlink;

    let fixture = Fixture::new("doctor-broken-link");
    fs::create_dir(fixture.project.join(".agents")).expect(".agents fixture");
    symlink("missing-target", fixture.project.join(".agents/skills"))
        .expect("broken discovery symlink");
    let before = snapshot(&fixture.project);

    let output = fixture.doctor();

    assert_eq!(output.status.code(), Some(65));
    let rendered = String::from_utf8_lossy(&output.stdout);
    assert!(rendered.contains("[FAIL] project .agents/skills"));
    assert!(rendered.contains("exact chain:"));
    assert!(rendered.contains("missing-target"));
    assert!(rendered.contains("no changes were made"));
    assert_eq!(snapshot(&fixture.project), before);
    assert!(!fixture.state.exists());
}

#[cfg(unix)]
#[test]
fn cyclic_project_discovery_link_is_a_failing_finding() {
    use std::os::unix::fs::symlink;

    let fixture = Fixture::new("doctor-cycle");
    fs::create_dir(fixture.project.join(".agents")).expect(".agents fixture");
    symlink("cycle-b", fixture.project.join(".agents/skills")).expect("cycle first hop");
    symlink("skills", fixture.project.join(".agents/cycle-b")).expect("cycle second hop");

    let output = fixture.doctor();

    assert_eq!(output.status.code(), Some(65));
    let rendered = String::from_utf8_lossy(&output.stdout);
    assert!(rendered.contains("[FAIL] project .agents/skills: link cycle"));
    assert!(rendered.contains("cycle-b"));
}

#[test]
fn an_unavailable_probe_root_is_reported_without_touching_the_project() {
    let fixture = Fixture::new("doctor-probe-failure");
    let unavailable_temp = fixture.root.join("missing-parent/temp");
    let before = snapshot(&fixture.project);

    let output = fixture
        .doctor_command()
        .env("TMPDIR", &unavailable_temp)
        .env("TMP", &unavailable_temp)
        .env("TEMP", &unavailable_temp)
        .output()
        .expect("probe-failure doctor should run");

    assert_eq!(output.status.code(), Some(65));
    let rendered = String::from_utf8_lossy(&output.stdout);
    assert!(rendered.contains("[FAIL] symlink capability"));
    assert!(rendered.contains("isolated probe directory"));
    assert!(rendered.contains("the project was not touched"));
    assert_eq!(snapshot(&fixture.project), before);
}

#[test]
fn corrupt_transaction_state_is_a_failing_read_only_doctor_finding() {
    let fixture = Fixture::new("doctor-corrupt-journal");
    let transactions = fixture.state.join("transactions");
    fs::create_dir_all(&transactions).expect("transaction fixture");
    let corrupt = transactions.join("ffff-future.journal");
    fs::write(&corrupt, "skillmount-journal 99 unix deadbeef\n").expect("corrupt journal");
    let project_before = snapshot(&fixture.project);

    let output = fixture.doctor();

    assert_eq!(output.status.code(), Some(65));
    let rendered = String::from_utf8_lossy(&output.stdout);
    assert!(rendered.contains("[FAIL] transaction state"));
    assert!(rendered.contains("unreadable or corrupt"));
    assert_eq!(snapshot(&fixture.project), project_before);
    assert_eq!(
        fs::read_to_string(&corrupt).unwrap(),
        "skillmount-journal 99 unix deadbeef\n"
    );
    assert!(!fixture.state.join("locks").exists());
}

#[cfg(unix)]
#[test]
fn doctor_renders_a_non_unicode_agent_path_reversibly() {
    use std::ffi::OsString;
    use std::os::unix::ffi::OsStringExt;

    let fixture = Fixture::new("doctor-non-unicode");
    let unavailable_agent = fixture.root.join(OsString::from_vec(vec![
        b'c', b'o', b'd', b'e', b'x', b'-', 0xff,
    ]));

    let output = fixture
        .command()
        .arg("doctor")
        .arg("--project-root")
        .arg(&fixture.project)
        .arg("--codex-bin")
        .arg(unavailable_agent)
        .arg("--claude-bin")
        .arg(ASM)
        .output()
        .expect("non-Unicode doctor should run");

    assert_eq!(output.status.code(), Some(65));
    let rendered = String::from_utf8_lossy(&output.stdout);
    assert!(rendered.contains("[FAIL] codex executable"));
    assert!(rendered.contains("escaped:"));
    assert!(rendered.contains("\\xFF"));
}

fn install_agent_alias(directory: &Path, name: &str) {
    #[cfg(unix)]
    std::os::unix::fs::symlink(ASM, directory.join(name)).expect("agent executable symlink");

    #[cfg(windows)]
    {
        fs::copy(ASM, directory.join(format!("{name}.exe"))).expect("agent executable copy");
    }
}

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
            fs::read_link(current)
                .map_or_else(|_| "?".into(), |target| target.display().to_string())
        )
    } else if file_type.is_dir() {
        "dir".to_owned()
    } else {
        format!("file {}", metadata.len())
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
