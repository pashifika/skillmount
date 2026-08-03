//! End-to-end Codex session acceptance through the real `asm` process.

#![cfg(feature = "test-fixtures")]

use std::ffi::OsStr;
use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::{SystemTime, UNIX_EPOCH};

use skillmount::domain::LinkMode;
use skillmount::link::{LinkRequest, PlacementOutcome, platform_backend};

const ASM: &str = env!("CARGO_BIN_EXE_asm");
const FAKE_CODEX: &str = env!("CARGO_BIN_EXE_skillmount-fake-agent");

struct Fixture {
    root: PathBuf,
    project: PathBuf,
    sources: PathBuf,
    state: PathBuf,
    record: PathBuf,
}

impl Fixture {
    fn new(label: &str) -> Self {
        let nonce = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .expect("clock after epoch")
            .as_nanos();
        let root = std::env::temp_dir().join(format!(
            "skillmount-codex-session-{label}-{}-{nonce}",
            std::process::id()
        ));
        let fixture = Self {
            project: root.join("project"),
            sources: root.join("sources"),
            state: root.join("state"),
            record: root.join("fake-codex.record"),
            root,
        };
        fs::create_dir_all(&fixture.project).expect("project fixture");
        fs::create_dir_all(&fixture.sources).expect("source fixture");
        fixture
    }

    fn skill(&self, name: &str) -> PathBuf {
        let skill = self.sources.join(name);
        fs::create_dir_all(&skill).expect("Skill fixture");
        fs::write(
            skill.join("SKILL.md"),
            format!("---\nname: {name}\ndescription: {name} fixture\n---\n"),
        )
        .expect("Skill metadata");
        skill
    }

    fn command(&self) -> Command {
        self.command_with_agent(Some(Path::new(FAKE_CODEX)))
    }

    fn command_with_agent(&self, agent: Option<&Path>) -> Command {
        let mut command = Command::new(ASM);
        command
            .arg("codex")
            .arg("--skills-dir")
            .arg(&self.sources)
            .arg("--project-root")
            .arg(&self.project)
            .arg("--cwd")
            .arg(&self.project);
        if let Some(agent) = agent {
            command.arg("--agent-bin").arg(agent);
        }
        command
            .arg("--")
            .arg("--literal")
            .arg("value with spaces")
            .env("SKILLMOUNT_STATE_DIR", &self.state)
            .env("SKILLMOUNT_FAKE_RECORD", &self.record)
            .env("SKILLMOUNT_FAKE_BEHAVIOR", "exit")
            .current_dir(&self.project);
        command
    }

    fn install_current_discovery_link(&self) {
        let agents = self.project.join(".agents");
        let store = self.project.join(".codex/skills");
        fs::create_dir_all(&agents).expect("current .agents helper");
        fs::create_dir_all(&store).expect("current .codex Skill store");

        let backend = platform_backend();
        let staged_path = agents.join(".skills.skillmount-fixture");
        let staged = backend
            .create_directory_link(&LinkRequest {
                source: backend
                    .canonical_directory(&store)
                    .expect("canonical current Skill store"),
                staged_path,
                mode: LinkMode::Auto,
            })
            .expect("fixture discovery link");
        let outcome = backend
            .place_no_replace(&staged, &agents.join("skills"))
            .expect("place fixture discovery link");
        assert!(matches!(outcome, PlacementOutcome::Placed(_)));
    }
}

#[cfg(unix)]
const PATH_AGENT_NAME: &str = "codex";

#[cfg(windows)]
const PATH_AGENT_NAME: &str = "codex.exe";

impl Drop for Fixture {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.root);
    }
}

fn exists(path: &Path) -> bool {
    fs::symlink_metadata(path).is_ok()
}

#[cfg(unix)]
fn native_hex(value: &OsStr) -> String {
    use std::os::unix::ffi::OsStrExt;

    hex(value.as_bytes())
}

#[cfg(windows)]
fn native_hex(value: &OsStr) -> String {
    use std::os::windows::ffi::OsStrExt;

    hex(&value
        .encode_wide()
        .flat_map(u16::to_le_bytes)
        .collect::<Vec<_>>())
}

fn hex(bytes: &[u8]) -> String {
    use std::fmt::Write as _;

    bytes.iter().fold(String::new(), |mut encoded, byte| {
        write!(encoded, "{byte:02x}").expect("String writes cannot fail");
        encoded
    })
}

#[test]
fn selected_skills_stay_mounted_while_fake_codex_runs_then_cleanup_succeeds() {
    let fixture = Fixture::new("happy-path");
    fixture.skill("alpha");

    let mounted = fixture.project.join(".codex/skills/alpha");
    let expected_paths = std::env::join_paths([&mounted]).expect("fixture path list");
    let output = fixture
        .command()
        .env("SKILLMOUNT_FAKE_EXPECT_PATHS", expected_paths)
        .output()
        .expect("asm should run");

    assert_eq!(
        output.status.code(),
        Some(0),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(
        String::from_utf8_lossy(&output.stderr).contains("discovery does not grant sandbox access"),
        "external bundled-resource access must be explained"
    );
    let record = fs::read_to_string(&fixture.record).expect("fake Codex launch record");
    let launch_cwd = fs::canonicalize(&fixture.project).expect("canonical project fixture");
    assert!(record.contains(&format!("cwd={}\n", native_hex(launch_cwd.as_os_str()))));
    assert!(record.contains(&format!("arg={}\n", native_hex(OsStr::new("--literal")))));
    assert!(record.contains(&format!(
        "arg={}\n",
        native_hex(OsStr::new("value with spaces"))
    )));
    assert!(!record.contains(&format!("arg={}\n", native_hex(OsStr::new("-C")))));
    assert!(!record.contains(&format!("arg={}\n", native_hex(OsStr::new("--add-dir")))));
    assert!(record.contains(&format!("visible={}\n", native_hex(mounted.as_os_str()))));
    assert!(fixture.sources.join("alpha/SKILL.md").is_file());
    assert!(!exists(&fixture.project.join(".agents")));
    assert!(!exists(&fixture.project.join(".codex")));
}

#[test]
fn current_discovery_link_and_project_owned_skill_are_preserved() {
    let fixture = Fixture::new("current-layout");
    fixture.skill("alpha");
    fixture.install_current_discovery_link();

    let project_skill = fixture.project.join(".codex/skills/rasen");
    fs::create_dir(&project_skill).expect("project-owned Skill");
    let project_skill_body = "---\nname: rasen\ndescription: project fixture\n---\n";
    fs::write(project_skill.join("SKILL.md"), project_skill_body).expect("project Skill metadata");

    let backend = platform_backend();
    let discovery = fixture.project.join(".agents/skills");
    let discovery_before = backend
        .inspect_no_follow(&discovery)
        .expect("inspect fixture discovery link");
    let mounted = fixture.project.join(".codex/skills/alpha");
    let expected_paths =
        std::env::join_paths([&project_skill, &mounted]).expect("fixture path list");

    let output = fixture
        .command()
        .env("SKILLMOUNT_FAKE_EXPECT_PATHS", expected_paths)
        .output()
        .expect("asm should run");

    assert_eq!(
        output.status.code(),
        Some(0),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let discovery_after = backend
        .inspect_no_follow(&discovery)
        .expect("reinspect fixture discovery link");
    assert_eq!(discovery_after, discovery_before);
    assert_eq!(
        fs::read_to_string(project_skill.join("SKILL.md")).expect("project Skill survives"),
        project_skill_body
    );
    assert!(!exists(&mounted));
    assert!(fixture.project.join(".codex/skills").is_dir());
}

#[test]
fn a_missing_explicit_codex_fails_with_66_before_mutation() {
    let fixture = Fixture::new("missing-agent");
    fixture.skill("alpha");
    let missing = fixture.root.join("missing-codex");

    let output = fixture
        .command_with_agent(Some(&missing))
        .output()
        .expect("asm should report the missing executable");

    assert_eq!(output.status.code(), Some(66));
    assert!(!exists(&fixture.project.join(".agents")));
    assert!(!exists(&fixture.project.join(".codex")));
    assert!(!exists(&fixture.state));
    assert!(!exists(&fixture.record));
}

#[test]
fn codex_is_resolved_from_path_before_mounting() {
    let fixture = Fixture::new("path-agent");
    fixture.skill("alpha");
    let bin = fixture.root.join("bin");
    fs::create_dir(&bin).expect("PATH fixture");
    let path_agent = bin.join(PATH_AGENT_NAME);
    fs::copy(FAKE_CODEX, &path_agent).expect("copy fake Codex onto PATH");
    let search_path = std::env::join_paths([&bin]).expect("PATH fixture encoding");

    let output = fixture
        .command_with_agent(None)
        .env("PATH", search_path)
        .output()
        .expect("asm should launch the PATH-resolved fake Codex");

    assert_eq!(
        output.status.code(),
        Some(0),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(fixture.record.is_file());
    assert!(!exists(&fixture.project.join(".agents")));
    assert!(!exists(&fixture.project.join(".codex")));
}

#[test]
fn a_missing_path_codex_fails_with_66_before_mutation() {
    let fixture = Fixture::new("missing-path-agent");
    fixture.skill("alpha");
    let empty_bin = fixture.root.join("empty-bin");
    fs::create_dir(&empty_bin).expect("empty PATH directory");
    let search_path = std::env::join_paths([&empty_bin]).expect("PATH fixture encoding");

    let output = fixture
        .command_with_agent(None)
        .env("PATH", search_path)
        .output()
        .expect("asm should report missing PATH Codex");

    assert_eq!(output.status.code(), Some(66));
    assert!(!exists(&fixture.project.join(".agents")));
    assert!(!exists(&fixture.project.join(".codex")));
    assert!(!exists(&fixture.state));
    assert!(!exists(&fixture.record));
}

#[test]
fn child_nonzero_status_is_preserved_after_successful_cleanup() {
    let fixture = Fixture::new("child-nonzero");
    fixture.skill("alpha");

    let output = fixture
        .command()
        .env("SKILLMOUNT_FAKE_EXIT", "2")
        .output()
        .expect("asm should preserve the fake Codex status");

    assert_eq!(output.status.code(), Some(2));
    assert!(!exists(&fixture.project.join(".agents")));
    assert!(!exists(&fixture.project.join(".codex")));
    assert!(
        !String::from_utf8_lossy(&output.stderr).contains("session cleanup failed"),
        "successful cleanup must not add a secondary failure"
    );
}

#[test]
fn cleanup_failure_replaces_child_success_and_preserves_user_content() {
    let fixture = Fixture::new("cleanup-failure");
    fixture.skill("alpha");
    let user_file = fixture.project.join(".codex/skills/user-note.txt");

    let output = fixture
        .command()
        .env("SKILLMOUNT_FAKE_CREATE_FILE", &user_file)
        .output()
        .expect("asm should report cleanup residue");

    assert_eq!(output.status.code(), Some(73));
    assert_eq!(
        fs::read_to_string(&user_file).expect("user file must survive cleanup"),
        "created by fake agent\n"
    );
    assert!(!exists(&fixture.project.join(".codex/skills/alpha")));
    assert!(!exists(&fixture.project.join(".agents")));
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(stderr.contains("session cleanup failed"), "{stderr}");
    assert!(stderr.contains("journal retained at"), "{stderr}");
}

#[test]
fn child_failure_remains_primary_when_cleanup_also_fails() {
    let fixture = Fixture::new("child-and-cleanup-failure");
    fixture.skill("alpha");
    let user_file = fixture.project.join(".codex/skills/user-note.txt");

    let output = fixture
        .command()
        .env("SKILLMOUNT_FAKE_CREATE_FILE", &user_file)
        .env("SKILLMOUNT_FAKE_EXIT", "2")
        .output()
        .expect("asm should preserve child precedence");

    assert_eq!(output.status.code(), Some(2));
    assert!(user_file.is_file());
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        stderr.contains("warning: session cleanup failed"),
        "{stderr}"
    );
    assert!(
        !stderr.contains("error: session cleanup failed"),
        "{stderr}"
    );
}
