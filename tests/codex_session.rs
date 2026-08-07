//! End-to-end Codex session acceptance through the real `asm` process.

#![cfg(feature = "test-fixtures")]

use std::ffi::{OsStr, OsString};
use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;
#[cfg(unix)]
use std::process::Stdio;
use std::thread;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

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
    version_record: PathBuf,
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
            version_record: root.join("fake-codex-version.record"),
            root,
        };
        fs::create_dir_all(&fixture.project).expect("project fixture");
        fs::create_dir_all(&fixture.sources).expect("source fixture");
        fs::create_dir_all(fixture.root.join("codex-home")).expect("Codex home fixture");
        fixture
    }

    fn skill(&self, name: &str) -> PathBuf {
        self.skill_in(&self.sources, name, "fixture")
    }

    fn skill_in(&self, source: &Path, name: &str, marker: &str) -> PathBuf {
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
        self.command_with_options(&[])
    }

    fn command_with_options(&self, options: &[&str]) -> Command {
        self.command_with_agent_and_options(Some(Path::new(FAKE_CODEX)), options)
    }

    fn command_with_agent(&self, agent: Option<&Path>) -> Command {
        self.command_with_agent_and_options(agent, &[])
    }

    fn command_with_agent_and_options(&self, agent: Option<&Path>, options: &[&str]) -> Command {
        self.command_with_sources_agent_and_options(&[self.sources.as_path()], agent, options)
    }

    fn command_with_sources_agent_and_options(
        &self,
        sources: &[&Path],
        agent: Option<&Path>,
        options: &[&str],
    ) -> Command {
        let mut command = Command::new(ASM);
        command.arg("codex");
        for source in sources {
            command.arg("--skills-dir").arg(source);
        }
        command
            .arg("--project-root")
            .arg(&self.project)
            .arg("--cwd")
            .arg(&self.project);
        if let Some(agent) = agent {
            command.arg("--agent-bin").arg(agent);
        }
        command
            .args(options)
            .arg("--")
            .arg("exec")
            .arg("--literal")
            .arg("value with spaces");
        self.configure_environment(&mut command, &self.project);
        command
    }

    fn configure_environment(&self, command: &mut Command, process_cwd: &Path) {
        command
            .env("HOME", self.root.join("home"))
            .env("USERPROFILE", self.root.join("home"))
            .env("SKILLMOUNT_TEST_CODEX_USER_HOME", self.root.join("home"))
            .env("SKILLMOUNT_TEST_CODEX_MANAGED_CONFIG", "absent")
            .env("LOCALAPPDATA", self.root.join("home/AppData/Local"))
            .env("CODEX_HOME", self.root.join("codex-home"))
            .env(
                "SKILLMOUNT_CODEX_ADMIN_SKILLS_DIR",
                self.root.join("admin-skills"),
            )
            .env("SKILLMOUNT_STATE_DIR", &self.state)
            .env("SKILLMOUNT_FAKE_RECORD", &self.record)
            .env("SKILLMOUNT_FAKE_VERSION_RECORD", &self.version_record)
            .env("SKILLMOUNT_FAKE_RECORD_CODEX_HOME", "1")
            .env("SKILLMOUNT_FAKE_BEHAVIOR", "exit")
            .current_dir(process_cwd);
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
struct UnixProcessGuard(nix::unistd::Pid);

#[cfg(unix)]
impl UnixProcessGuard {
    fn from_record(path: &Path) -> Self {
        let record = fs::read_to_string(path).expect("fake descendant record");
        let pid = record
            .lines()
            .find_map(|line| line.strip_prefix("pid="))
            .expect("fake descendant records its PID")
            .parse::<i32>()
            .expect("fake descendant PID is numeric");
        Self(nix::unistd::Pid::from_raw(pid))
    }

    fn is_running(&self) -> bool {
        nix::sys::signal::kill(self.0, None).is_ok()
    }
}

#[cfg(unix)]
impl Drop for UnixProcessGuard {
    fn drop(&mut self) {
        let _ = nix::sys::signal::kill(self.0, nix::sys::signal::Signal::SIGKILL);
    }
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

fn record_contains_os(record: &str, name: &str, value: &OsStr) -> bool {
    let expected = format!("{name}={}", native_hex(value));
    record.lines().any(|line| line == expected)
}

fn recorded_os(record: &str, name: &str) -> OsString {
    let prefix = format!("{name}=");
    let encoded = record
        .lines()
        .find_map(|line| line.strip_prefix(&prefix))
        .unwrap_or_else(|| panic!("fake Codex record has no {name} entry"));
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
    use std::os::unix::ffi::OsStringExt;

    OsString::from_vec(decode_hex(value))
}

#[cfg(windows)]
fn native_from_hex(value: &str) -> OsString {
    use std::os::windows::ffi::OsStringExt;

    let bytes = decode_hex(value);
    assert_eq!(bytes.len() % 2, 0, "odd UTF-16 byte record");
    let wide = bytes
        .chunks_exact(2)
        .map(|pair| u16::from_le_bytes([pair[0], pair[1]]))
        .collect::<Vec<_>>();
    OsString::from_wide(&wide)
}

/// A mutating session spawns exactly one Agent process: the supervised session child.
///
/// [ADR 0036](../docs/adr/0036-confine-agent-version-observation-to-doctor.md) removed the
/// pre-state `--version` observation, so the absence of the record file is the observable proof.
fn assert_no_version_process_and_no_compatibility_warning(fixture: &Fixture, stderr: &str) {
    assert!(
        !fixture.version_record.exists(),
        "a mutating session must not run --version"
    );
    assert!(
        !stderr.contains("version compatibility is unverified"),
        "a session must emit no compatibility warning: {stderr}"
    );
    assert!(
        fixture.record.is_file(),
        "the supervised session child must still start"
    );
}

#[test]
fn selected_skills_stay_mounted_while_fake_codex_runs_then_cleanup_succeeds() {
    let fixture = Fixture::new("happy-path");
    fixture.skill("alpha");

    let mounted = fixture.project.join(".agents/skills/alpha");
    let expected_paths = std::env::join_paths([&mounted]).expect("fixture path list");
    let output = fixture
        .command()
        .env("SKILLMOUNT_FAKE_EXPECT_PATHS", expected_paths)
        .current_dir(&fixture.root)
        .output()
        .expect("asm should run");

    assert_eq!(
        output.status.code(),
        Some(0),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        stderr.contains("Mounted 1 skill from 1 source argument for Codex (0 source overrides)."),
        "{stderr}"
    );
    assert!(stderr.contains("  alpha\n"), "{stderr}");
    assert!(stderr.contains("Launching codex..."), "{stderr}");
    assert!(output.stdout.is_empty(), "child stdout stays data-only");
    assert!(
        stderr.contains("discovery does not grant sandbox access"),
        "external bundled-resource access must be explained"
    );
    let record = fs::read_to_string(&fixture.record).expect("fake Codex launch record");
    let recorded_arguments = recorded_os_values(&record, "arg");
    assert_eq!(
        recorded_arguments,
        vec![
            OsString::from("-C"),
            fs::canonicalize(&fixture.project)
                .expect("canonical project fixture")
                .into_os_string(),
            OsString::from("-c"),
            OsString::from("project_root_markers=[\".git\"]"),
            OsString::from("-c"),
            OsString::from("skills.config=[{name=\"alpha\",enabled=true}]"),
            OsString::from("exec"),
            OsString::from("--literal"),
            OsString::from("value with spaces"),
        ],
        "injected arguments must precede unchanged passthrough values"
    );
    let recorded_cwd = PathBuf::from(recorded_os(&record, "cwd"));
    assert_eq!(
        fs::canonicalize(recorded_cwd).expect("canonical fake Codex CWD"),
        fs::canonicalize(&fixture.project).expect("canonical project fixture")
    );
    assert!(record_contains_os(&record, "arg", OsStr::new("--literal")));
    assert!(record_contains_os(
        &record,
        "arg",
        OsStr::new("value with spaces")
    ));
    assert!(record_contains_os(&record, "arg", OsStr::new("-C")));
    assert!(record_contains_os(
        &record,
        "arg",
        fs::canonicalize(&fixture.project)
            .expect("canonical project fixture")
            .as_os_str()
    ));
    assert!(record_contains_os(
        &record,
        "arg",
        OsStr::new("project_root_markers=[\".git\"]")
    ));
    assert!(record_contains_os(
        &record,
        "arg",
        OsStr::new("skills.config=[{name=\"alpha\",enabled=true}]")
    ));
    assert!(!record_contains_os(&record, "arg", OsStr::new("--add-dir")));
    assert!(record_contains_os(&record, "visible", mounted.as_os_str()));
    assert!(record_contains_os(
        &record,
        "env:CODEX_HOME",
        fs::canonicalize(fixture.root.join("codex-home"))
            .expect("canonical Codex home")
            .as_os_str()
    ));
    assert!(fixture.sources.join("alpha/SKILL.md").is_file());
    assert!(!exists(&fixture.project.join(".agents")));
    assert!(!exists(&fixture.project.join(".codex")));
    assert_no_version_process_and_no_compatibility_warning(&fixture, &stderr);
}

#[test]
fn machine_readable_codex_stdout_is_not_prefixed_by_wrapper_diagnostics() {
    let fixture = Fixture::new("machine-readable-stdout");
    fixture.skill("alpha");

    let output = fixture
        .command()
        .env("SKILLMOUNT_FAKE_BEHAVIOR", "json")
        .output()
        .expect("machine-readable fake Codex session");

    assert_eq!(output.status.code(), Some(0));
    assert_eq!(output.stdout, b"{\"type\":\"fixture\"}\n");
    let parsed: serde_json::Value =
        serde_json::from_slice(&output.stdout).expect("stdout is one valid JSON value");
    assert_eq!(parsed["type"], "fixture");
    let diagnostics = String::from_utf8_lossy(&output.stderr);
    assert!(diagnostics.contains("Mounted 1 skill"), "{diagnostics}");
    assert!(diagnostics.contains("Launching codex"), "{diagnostics}");
}

#[test]
fn three_source_codex_overlay_mounts_the_rightmost_winner_and_lists_every_origin() {
    let fixture = Fixture::new("three-source-overlay");
    fixture.skill_in(&fixture.sources, "alpha", "first");
    let second = fixture.root.join("second");
    let third = fixture.root.join("third");
    fs::create_dir(&second).expect("second catalog");
    fs::create_dir(&third).expect("third catalog");
    fixture.skill_in(&second, "alpha", "second");
    let winner = fixture.skill_in(&third, "alpha", "third winner");
    let mounted = fixture.project.join(".agents/skills/alpha");
    let expected_paths = std::env::join_paths([&mounted]).expect("fixture path list");

    let output = fixture
        .command_with_sources_agent_and_options(
            &[fixture.sources.as_path(), second.as_path(), third.as_path()],
            Some(Path::new(FAKE_CODEX)),
            &["--verbose"],
        )
        .env("SKILLMOUNT_FAKE_EXPECT_PATHS", expected_paths)
        .output()
        .expect("three-source Codex session");

    assert_eq!(
        output.status.code(),
        Some(0),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        stderr.contains("Mounted 1 skill from 3 source arguments for Codex (1 source override)."),
        "{stderr}"
    );
    assert_eq!(stderr.matches("(different source)").count(), 2, "{stderr}");
    assert!(stderr.contains("[3]"), "{stderr}");
    assert!(output.stdout.is_empty());
    let record = fs::read_to_string(&fixture.record).expect("fake Codex launch record");
    assert_eq!(
        fs::canonicalize(PathBuf::from(recorded_os(&record, "visible-target")))
            .expect("canonical mounted target"),
        fs::canonicalize(winner).expect("canonical rightmost winner")
    );
    assert!(!exists(&mounted));
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
    let mounted = fixture.project.join(".agents/skills/alpha");
    let expected_paths =
        std::env::join_paths([&project_skill, &mounted]).expect("fixture path list");

    let output = fixture
        .command_with_options(&["--verbose"])
        .env("SKILLMOUNT_FAKE_EXPECT_PATHS", expected_paths)
        .output()
        .expect("asm should run");

    assert_eq!(
        output.status.code(),
        Some(0),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(stderr.contains("link[0]"), "{stderr}");
    assert!(stderr.contains("terminal"), "{stderr}");
    assert!(stderr.contains("info: cleanup removed"), "{stderr}");
    assert!(output.stdout.is_empty());
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
fn a_drifted_codex_banner_neither_warns_nor_starts_a_version_process() {
    let fixture = Fixture::new("drifted-banner");
    fixture.skill("alpha");

    let output = fixture
        .command_with_options(&["-v"])
        .env("SKILLMOUNT_FAKE_VERSION_OUTPUT", "codex-cli 0.147.0")
        .env("SKILLMOUNT_FAKE_EXIT", "23")
        .output()
        .expect("asm should launch a drifted Codex build");
    let stderr = String::from_utf8_lossy(&output.stderr);

    assert_eq!(output.status.code(), Some(23), "{stderr}");
    assert_no_version_process_and_no_compatibility_warning(&fixture, &stderr);
    // Verbose output still names the dated constant, and now says the executable was never
    // queried on any surface. The installed banner cannot appear because nothing read it.
    assert!(stderr.contains("codex-cli 0.146.0"), "{stderr}");
    assert!(stderr.contains("executable not queried"), "{stderr}");
    assert!(!stderr.contains("codex-cli 0.147.0"), "{stderr}");
    assert!(!exists(&fixture.project.join(".agents")));
    assert!(!exists(&fixture.project.join(".codex")));
}

#[test]
fn a_codex_build_without_usable_version_evidence_still_keeps_mounts() {
    let fixture = Fixture::new("unusable-version-keep");
    fixture.skill("alpha");

    // The fake agent would fail `--version` if anything asked; nothing does.
    let output = fixture
        .command_with_options(&["--keep-mounts"])
        .env("SKILLMOUNT_FAKE_VERSION_EXIT", "9")
        .output()
        .expect("asm should ignore an unusable --version");
    let stderr = String::from_utf8_lossy(&output.stderr);

    assert_eq!(output.status.code(), Some(0), "{stderr}");
    assert_no_version_process_and_no_compatibility_warning(&fixture, &stderr);
    assert!(exists(&fixture.project.join(".agents/skills/alpha")));
}

#[test]
fn a_plugin_namespace_appearing_after_apply_overrides_keep_and_cleans_mounts() {
    let fixture = Fixture::new("plugin-after-apply");
    fixture.skill("alpha");
    let manifest = fixture.sources.join(".codex-plugin/plugin.json");
    let mounted = fixture.project.join(".agents/skills/alpha");
    let release = fixture.root.join("release-plugin-check");
    let manifest_for_injector = manifest.clone();
    let release_for_injector = release.clone();
    let injector = thread::spawn(move || {
        let deadline = Instant::now() + Duration::from_secs(10);
        while !exists(&mounted) {
            assert!(
                Instant::now() < deadline,
                "the applied mount never became observable"
            );
            thread::sleep(Duration::from_millis(10));
        }
        fs::create_dir_all(
            manifest_for_injector
                .parent()
                .expect("plugin manifest parent"),
        )
        .expect("plugin fixture directory");
        fs::write(&manifest_for_injector, br#"{"name":"late-plugin"}"#)
            .expect("late plugin manifest");
        fs::write(release_for_injector, b"release\n").expect("release spawn-boundary check");
    });

    let output = fixture
        .command_with_options(&["--keep-mounts"])
        .env("SKILLMOUNT_HOLD_AT", "journal-active")
        .env("SKILLMOUNT_HOLD_MS", "10000")
        .env("SKILLMOUNT_HOLD_UNTIL", &release)
        .output()
        .expect("asm should reject namespace qualification at the spawn boundary");
    injector.join().expect("plugin injector");
    let stderr = String::from_utf8_lossy(&output.stderr);

    assert_eq!(output.status.code(), Some(73), "{stderr}");
    assert!(stderr.contains("namespace-qualify"), "{stderr}");
    assert!(manifest.is_file(), "the post-apply race fixture remains");
    assert!(!exists(&fixture.record), "the child must not start");
    assert!(!exists(&fixture.project.join(".agents")));
    assert!(!exists(&fixture.project.join(".codex")));
}

#[test]
fn supervision_journal_failure_overrides_keep_and_cleans_mounts_before_returning() {
    let fixture = Fixture::new("supervision-journal-failure");
    fixture.skill("alpha");

    let output = fixture
        .command_with_options(&["--keep-mounts"])
        .env("SKILLMOUNT_TEST_FAIL_BEGIN_SUPERVISION", "1")
        .output()
        .expect("asm should report the durable supervision failure");

    assert_eq!(output.status.code(), Some(73));
    assert!(
        String::from_utf8_lossy(&output.stderr)
            .contains("injected begin-supervision persistence failure")
    );
    assert!(!exists(&fixture.record), "the child must not start");
    assert!(!exists(&fixture.project.join(".agents")));
    assert!(!exists(&fixture.project.join(".codex")));
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
fn every_relative_wrapper_path_resolves_from_the_invocation_cwd_in_a_real_session() {
    let fixture = Fixture::new("relative-wrapper-paths");
    fixture.skill("alpha");
    let agent_name = Path::new(FAKE_CODEX)
        .file_name()
        .expect("fake agent filename");
    let local_agent = fixture.root.join(agent_name);
    fs::copy(FAKE_CODEX, &local_agent).expect("copy relative fake agent");
    let mounted = fixture.project.join(".agents/skills/alpha");
    let expected_paths = std::env::join_paths([&mounted]).expect("fixture path list");

    let mut command = Command::new(ASM);
    command
        .arg("codex")
        .arg("--skills-dir")
        .arg("sources")
        .arg("--project-root")
        .arg("project")
        .arg("--cwd")
        .arg("project")
        .arg("--agent-bin")
        .arg(agent_name)
        .arg("--")
        .arg("exec")
        .arg("relative fixture")
        .env("SKILLMOUNT_FAKE_EXPECT_PATHS", expected_paths);
    fixture.configure_environment(&mut command, &fixture.root);

    let output = command.output().expect("relative-path Codex session");

    assert_eq!(
        output.status.code(),
        Some(0),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let record = fs::read_to_string(&fixture.record).expect("fake Codex launch record");
    assert_eq!(
        fs::canonicalize(PathBuf::from(recorded_os(&record, "cwd"))).expect("canonical child CWD"),
        fs::canonicalize(&fixture.project).expect("canonical project")
    );
    assert_eq!(
        fs::canonicalize(PathBuf::from(recorded_os(&record, "visible-target")))
            .expect("canonical mounted target"),
        fs::canonicalize(fixture.sources.join("alpha")).expect("canonical source Skill")
    );
    assert!(!exists(&mounted));
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
    let user_file = fixture.project.join(".agents/skills/user-note.txt");

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
    assert!(!exists(&fixture.project.join(".agents/skills/alpha")));
    assert!(fixture.project.join(".agents/skills").is_dir());
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(stderr.contains("session cleanup failed"), "{stderr}");
    assert!(stderr.contains("journal retained at"), "{stderr}");
}

#[test]
fn child_failure_remains_primary_when_cleanup_also_fails() {
    let fixture = Fixture::new("child-and-cleanup-failure");
    fixture.skill("alpha");
    let user_file = fixture.project.join(".agents/skills/user-note.txt");

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
    assert!(stderr.contains("retained path"), "{stderr}");
    assert!(stderr.contains("journal retained at"), "{stderr}");
    assert!(stderr.contains("recovery argv[0] = asm"), "{stderr}");
    assert!(stderr.contains("recovery argv[1] = cleanup"), "{stderr}");
    assert!(
        stderr.contains("recovery argv[2] = --project-root"),
        "{stderr}"
    );
    assert!(
        !stderr.contains("asm cleanup --project-root"),
        "recovery guidance must not construct a shell command: {stderr}"
    );
}

#[cfg(unix)]
#[test]
fn a_supervising_journal_is_quarantined_while_an_orphan_descendant_remains_alive() {
    let fixture = Fixture::new("supervising-quarantine");
    fixture.skill("alpha");
    let descendant_record = fixture.root.join("fake-descendant.record");

    let first = fixture
        .command()
        .env("SKILLMOUNT_FAKE_BEHAVIOR", "orphan-descendant-ignore-all")
        .env("SKILLMOUNT_FAKE_DESCENDANT_RECORD", &descendant_record)
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .expect("the uncertain session should return within the supervision bound");
    let descendant = UnixProcessGuard::from_record(&descendant_record);
    let mounted = fixture.project.join(".agents/skills/alpha");

    assert_eq!(first.code(), Some(70));
    assert!(descendant.is_running(), "the descendant must still be live");
    assert!(
        exists(&mounted),
        "cleanup must be deferred while liveness is unknown"
    );

    let second = fixture
        .command()
        .output()
        .expect("a later session should fail closed on the supervising journal");
    let stderr = String::from_utf8_lossy(&second.stderr);

    assert_eq!(second.status.code(), Some(75), "{stderr}");
    assert!(
        stderr.contains("process-domain death was never proved"),
        "{stderr}"
    );
    assert!(
        stderr.contains("quarantined mounts were not changed and remain journal-backed"),
        "{stderr}"
    );
    assert!(stderr.contains("recovery[0] argv[1] = cleanup"), "{stderr}");
    assert!(
        exists(&mounted),
        "automatic recovery must not remove the live mount"
    );
    assert!(
        descendant.is_running(),
        "recovery must not affect the live child domain"
    );
    assert_eq!(
        fs::read_dir(fixture.state.join("transactions"))
            .expect("retained transaction directory")
            .filter_map(Result::ok)
            .filter(|entry| entry
                .path()
                .extension()
                .is_some_and(|value| value == "journal"))
            .count(),
        1,
        "the quarantined ownership evidence must remain durable"
    );
}
