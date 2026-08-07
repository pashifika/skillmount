//! Bounded `--version` observation coverage over a real Agent process.
//!
//! [ADR 0036](../docs/adr/0036-confine-agent-version-observation-to-doctor.md) removes the version
//! observation from every mutating session, so `doctor` is the only surface that still executes an
//! Agent to read a banner. The containment properties ADR 0033 introduced are reachable only
//! through a real `--version` child: `tests/operator_commands.rs` drives every doctor version
//! finding through the `SKILLMOUNT_TEST_*_VERSION` debug override, which returns before spawn. This
//! suite therefore owns the spawn, bound, and process-domain evidence for that observation.

#![cfg(feature = "test-fixtures")]

use std::fs;
use std::path::{Path, PathBuf};
use std::process::{Command, Output};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

const ASM: &str = env!("CARGO_BIN_EXE_asm");
const FAKE_AGENT: &str = env!("CARGO_BIN_EXE_skillmount-fake-agent");

struct Fixture {
    root: PathBuf,
    project: PathBuf,
    home: PathBuf,
    state: PathBuf,
    version_record: PathBuf,
}

impl Fixture {
    fn new(label: &str) -> Self {
        let nonce = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .expect("clock should be after the epoch")
            .as_nanos();
        let root = std::env::temp_dir().join(format!(
            "skillmount-doctor-version-{label}-{}-{nonce}",
            std::process::id()
        ));
        let fixture = Self {
            project: root.join("project"),
            home: root.join("home"),
            state: root.join("state"),
            version_record: root.join("version.record"),
            root,
        };
        for path in [
            &fixture.project,
            &fixture.home,
            &fixture.root.join("codex-home"),
        ] {
            fs::create_dir_all(path).expect("doctor version fixture directory");
        }
        fixture
    }

    /// Runs `doctor` with Codex bound to the fake agent and the other two Agents short-circuited.
    ///
    /// Only Codex reaches the real observer, so the shared version record holds exactly the
    /// observations this suite is asserting about.
    fn doctor_command(&self) -> Command {
        let mut command = Command::new(ASM);
        command
            .current_dir(&self.root)
            .env("HOME", &self.home)
            .env("USERPROFILE", &self.home)
            .env("SKILLMOUNT_TEST_CODEX_USER_HOME", &self.home)
            .env("SKILLMOUNT_TEST_CODEX_MANAGED_CONFIG", "absent")
            .env("LOCALAPPDATA", self.home.join("AppData/Local"))
            .env("CODEX_HOME", self.root.join("codex-home"))
            .env("CLAUDE_CONFIG_DIR", self.home.join(".claude"))
            .env(
                "SKILLMOUNT_CLAUDE_MANAGED_SKILLS_DIR",
                self.root.join("claude-managed/skills"),
            )
            .env(
                "SKILLMOUNT_CODEX_ADMIN_SKILLS_DIR",
                self.root.join("admin-skills"),
            )
            .env_remove("CLAUDE_CODE_SAFE_MODE")
            .env_remove("CLAUDE_CODE_SIMPLE")
            // Claude and OMP keep the deterministic override so this suite observes exactly one
            // real process, while Codex must reach the spawn path the override bypasses.
            .env("SKILLMOUNT_TEST_CLAUDE_VERSION", "2.1.220 (Claude Code)")
            .env("SKILLMOUNT_TEST_OMP_VERSION", "omp/17.2.9")
            .env_remove("SKILLMOUNT_TEST_CODEX_VERSION")
            // OMP resolves its roots from the environment, so the developer's real profile,
            // configuration overlay, and XDG bases must never reach a fixture.
            .env_remove("OMP_PROFILE")
            .env_remove("PI_PROFILE")
            .env_remove("PI_CONFIG_FILES")
            .env_remove("PI_CONFIG_DIR")
            .env_remove("PI_CODING_AGENT_DIR")
            .env_remove("XDG_DATA_HOME")
            .env("SKILLMOUNT_STATE_DIR", &self.state)
            .env("SKILLMOUNT_FAKE_VERSION_RECORD", &self.version_record)
            .arg("doctor")
            .arg("--project-root")
            .arg(&self.project)
            .arg("--codex-bin")
            .arg(FAKE_AGENT)
            .arg("--claude-bin")
            .arg(ASM)
            .arg("--omp-bin")
            .arg(ASM);
        command
    }

    fn observations(&self) -> usize {
        fs::read_to_string(&self.version_record).map_or(0, |record| record.lines().count())
    }
}

impl Drop for Fixture {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.root);
    }
}

/// Every observation is diagnostic: it never fails doctor and never writes `SkillMount` state.
fn assert_diagnostic_only(fixture: &Fixture, output: &Output) -> String {
    let stdout = String::from_utf8_lossy(&output.stdout).into_owned();
    assert!(
        output.status.success(),
        "version evidence alone must not fail doctor: {stdout}"
    );
    assert!(
        output.stderr.is_empty(),
        "doctor findings belong on stdout: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(stdout.contains("0 failure"), "{stdout}");
    assert!(
        !fixture.state.exists(),
        "observing a version must not create SkillMount state"
    );
    stdout
}

#[test]
fn an_exact_banner_passes_after_exactly_one_real_observation() {
    let fixture = Fixture::new("exact-banner");

    let output = fixture
        .doctor_command()
        .output()
        .expect("doctor should observe the fake agent");
    let rendered = assert_diagnostic_only(&fixture, &output);

    assert!(rendered.contains("[PASS] codex executable"), "{rendered}");
    assert!(rendered.contains("codex-cli 0.146.0"), "{rendered}");
    assert_eq!(
        fixture.observations(),
        1,
        "the banner is observed exactly once"
    );
}

#[test]
fn a_drifted_banner_is_unverified_and_names_both_releases() {
    let fixture = Fixture::new("drifted-banner");

    let output = fixture
        .doctor_command()
        .env("SKILLMOUNT_FAKE_VERSION_OUTPUT", "codex-cli 999.0.0")
        .output()
        .expect("doctor should observe a drifted fake agent");
    let rendered = assert_diagnostic_only(&fixture, &output);

    assert!(
        rendered.contains("[UNVERIFIED] codex executable"),
        "{rendered}"
    );
    assert!(rendered.contains("codex-cli 999.0.0"), "{rendered}");
    assert!(rendered.contains("codex-cli 0.146.0"), "{rendered}");
    assert!(rendered.contains("docs/compatibility.md"), "{rendered}");
    assert!(rendered.contains("[PASS] claude executable"), "{rendered}");
    assert_eq!(fixture.observations(), 1);
}

#[test]
fn a_nonzero_version_exit_is_unverified_without_suppressing_other_checks() {
    let fixture = Fixture::new("nonzero-exit");

    let output = fixture
        .doctor_command()
        .env("SKILLMOUNT_FAKE_VERSION_EXIT", "9")
        .output()
        .expect("doctor should classify a failed version probe");
    let rendered = assert_diagnostic_only(&fixture, &output);

    assert!(
        rendered.contains("[UNVERIFIED] codex executable"),
        "{rendered}"
    );
    assert!(rendered.contains("--version exited with"), "{rendered}");
    assert!(rendered.contains("[PASS] codex discovery"), "{rendered}");
    assert_eq!(fixture.observations(), 1);
}

#[test]
fn oversized_interleaved_version_output_is_bounded() {
    let fixture = Fixture::new("oversized-interleaved");

    let output = fixture
        .doctor_command()
        .env("SKILLMOUNT_FAKE_VERSION_BEHAVIOR", "oversized-interleaved")
        .output()
        .expect("doctor should bound both version output streams");
    let rendered = assert_diagnostic_only(&fixture, &output);

    assert!(
        rendered.contains("[UNVERIFIED] codex executable"),
        "{rendered}"
    );
    assert!(
        rendered.contains("1024-byte observation bound"),
        "{rendered}"
    );
    assert_eq!(fixture.observations(), 1);
}

/// A descendant that inherits both captured handles must not outlive the observation bound.
#[test]
fn inherited_version_output_handles_are_terminated_at_the_lifetime_bound() {
    let fixture = Fixture::new("inherited-descriptor");
    let descendant_record = fixture.root.join("version-descendant.record");
    let started = Instant::now();

    let output = fixture
        .doctor_command()
        .env("SKILLMOUNT_FAKE_VERSION_BEHAVIOR", "inherited-descriptor")
        .env(
            "SKILLMOUNT_FAKE_VERSION_DESCENDANT_RECORD",
            &descendant_record,
        )
        .output()
        .expect("doctor should stop a version descendant that retains the output handles");
    let elapsed = started.elapsed();
    let rendered = assert_diagnostic_only(&fixture, &output);

    assert!(
        elapsed < Duration::from_secs(20),
        "capture took {elapsed:?}"
    );
    assert!(
        rendered.contains("[UNVERIFIED] codex executable"),
        "{rendered}"
    );
    assert!(
        rendered.contains("3-second process/output lifetime bound"),
        "{rendered}"
    );
    assert_eq!(fixture.observations(), 1);
    assert!(
        !descendant_is_running(&descendant_record),
        "the version descendant must be dead before doctor reports"
    );
}

fn descendant_pid(path: &Path) -> u32 {
    fs::read_to_string(path)
        .expect("fake version descendant record")
        .lines()
        .find_map(|line| line.strip_prefix("pid="))
        .expect("fake version descendant records its PID")
        .parse()
        .expect("fake version descendant PID is numeric")
}

#[cfg(unix)]
fn descendant_is_running(path: &Path) -> bool {
    let pid = nix::unistd::Pid::from_raw(
        i32::try_from(descendant_pid(path)).expect("fake version descendant PID fits a Unix PID"),
    );
    let running = nix::sys::signal::kill(pid, None).is_ok();
    if running {
        let _ = nix::sys::signal::kill(pid, nix::sys::signal::Signal::SIGKILL);
    }
    running
}

#[cfg(windows)]
fn descendant_is_running(path: &Path) -> bool {
    skillmount::process::test_support::process_is_running(descendant_pid(path))
        .expect("query version descendant liveness")
}
