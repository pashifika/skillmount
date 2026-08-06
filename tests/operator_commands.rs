//! Executable-seam coverage for operator diagnostics and recovery commands.

use std::collections::BTreeMap;
use std::fs;
use std::path::{Path, PathBuf};
use std::process::{Command, Output, Stdio};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

const ASM: &str = env!("CARGO_BIN_EXE_asm");

struct Fixture {
    root: PathBuf,
    project: PathBuf,
    home: PathBuf,
    state: PathBuf,
    sources: PathBuf,
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
        let sources = root.join("sources");
        for path in [&project, &home, &sources, &root.join("codex-home")] {
            fs::create_dir_all(path).expect("operator fixture directory");
        }
        Self {
            root,
            project,
            home,
            state,
            sources,
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
            .env("SKILLMOUNT_TEST_OMP_VERSION", "omp/17.2.9")
            // OMP resolves its roots from the environment, so the developer's real profile,
            // configuration overlay, and XDG bases must never reach a fixture.
            .env_remove("OMP_PROFILE")
            .env_remove("PI_PROFILE")
            .env_remove("PI_CONFIG_FILES")
            .env_remove("PI_CONFIG_DIR")
            .env_remove("PI_CODING_AGENT_DIR")
            .env_remove("XDG_DATA_HOME")
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
            .arg(ASM)
            .arg("--omp-bin")
            .arg(ASM);
        command
    }

    fn skill(&self, name: &str) {
        let directory = self.sources.join(name);
        fs::create_dir_all(&directory).expect("operator Skill fixture");
        fs::write(
            directory.join("SKILL.md"),
            format!("---\nname: {name}\ndescription: {name} fixture\n---\n"),
        )
        .expect("operator Skill metadata");
    }

    fn session_command(&self, extra: &[&str]) -> Command {
        let mut command = self.command();
        command
            .arg("codex")
            .arg("--skills-dir")
            .arg(&self.sources)
            .arg("--project-root")
            .arg(&self.project)
            .arg("--cwd")
            .arg(&self.project)
            .arg("--agent-bin")
            .arg(ASM)
            .args(extra)
            .arg("--")
            .arg("exec")
            .arg("fixture");
        command
    }

    fn run_stopping_at(&self, boundary: &str, extra: &[&str]) -> Output {
        self.session_command(extra)
            .env("SKILLMOUNT_STOP_AT", boundary)
            .output()
            .expect("checkpoint session should run")
    }

    fn path_agent_directory(&self) -> PathBuf {
        let directory = self.root.join("agent-path");
        fs::create_dir(&directory).expect("agent PATH fixture");
        install_agent_alias(&directory, "codex");
        install_agent_alias(&directory, "claude");
        install_agent_alias(&directory, "omp");
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
    assert!(rendered.contains("[PASS] omp executable"));
    assert!(rendered.contains("omp/17.2.9"));
    assert!(rendered.contains("[UNVERIFIED] codex live compatibility"));
    assert!(rendered.contains("[UNVERIFIED] claude live compatibility"));
    assert!(rendered.contains("[UNVERIFIED] omp live compatibility"));
    assert!(rendered.contains("[PASS] project .omp/skills"));
    assert!(rendered.contains("[PASS] omp discovery"));
    assert!(rendered.contains("docs/compatibility.md"));
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

/// `doctor` must diagnose OMP without letting it decide another Agent's finding.
///
/// An unsettled OMP global configuration is an OMP-only hard failure, so it must produce exactly
/// one failure while every Codex, Claude, layout, link, lock, and transaction finding still runs.
#[test]
fn an_unsettled_omp_configuration_fails_only_the_omp_finding() {
    let fixture = Fixture::new("doctor-omp-unsettled");
    let project_before = snapshot(&fixture.project);
    let agent_dir = fixture.home.join(".omp/agent");
    fs::create_dir_all(&agent_dir).expect("OMP agent directory");
    fs::write(agent_dir.join("settings.json"), "{}").expect("legacy OMP settings");

    let output = fixture.doctor();

    assert_eq!(output.status.code(), Some(65));
    let rendered = String::from_utf8_lossy(&output.stdout);
    assert!(rendered.contains("[FAIL] omp executable"), "{rendered}");
    assert!(rendered.contains("has not yet migrated"), "{rendered}");
    assert!(rendered.contains("[PASS] codex executable"), "{rendered}");
    assert!(rendered.contains("[PASS] claude executable"), "{rendered}");
    assert!(rendered.contains("[PASS] codex discovery"), "{rendered}");
    assert!(rendered.contains("[PASS] claude discovery"), "{rendered}");
    assert!(rendered.contains("1 failure"), "{rendered}");
    assert_eq!(snapshot(&fixture.project), project_before);
    assert!(!fixture.state.exists());
}

/// An `agent.db` with no `config.yml` is OMP's ordinary steady state, not unmigrated settings.
///
/// Every OMP start creates `agent.db` for sessions and usage, and 17.2.9 writes settings only to
/// `config.yml`: `AgentStorage.getSettings` returns null for an empty `settings` table, so
/// `#migrateFromLegacy` never writes the YAML file. Treating the database as evidence of pending
/// migration refused every install that had simply never customized a global setting, and no OMP
/// run could clear the refusal.
#[test]
fn an_omp_database_without_a_config_file_is_not_an_unsettled_configuration() {
    let fixture = Fixture::new("doctor-omp-database-only");
    let agent_dir = fixture.home.join(".omp/agent");
    fs::create_dir_all(&agent_dir).expect("OMP agent directory");
    fs::write(agent_dir.join("agent.db"), b"SQLite format 3\0").expect("OMP session database");

    let output = fixture.doctor();

    let rendered = String::from_utf8_lossy(&output.stdout);
    assert!(
        !rendered.contains("has not yet migrated"),
        "an OMP database alone must not refuse the session: {rendered}"
    );
    assert!(rendered.contains("[PASS] omp discovery"), "{rendered}");
    assert_eq!(output.status.code(), Some(0));
}

#[test]
fn a_drifted_omp_banner_is_unverified_without_failing_doctor() {
    let fixture = Fixture::new("doctor-omp-drift");

    let output = fixture
        .doctor_command()
        .env("SKILLMOUNT_TEST_OMP_VERSION", "omp/99.0.0")
        .output()
        .expect("drifted-version doctor should run");

    assert!(output.status.success());
    let rendered = String::from_utf8_lossy(&output.stdout);
    assert!(
        rendered.contains("[UNVERIFIED] omp executable"),
        "{rendered}"
    );
    assert!(rendered.contains("omp/99.0.0"), "{rendered}");
    assert!(rendered.contains("omp/17.2.9"), "{rendered}");
    assert!(rendered.contains("[PASS] codex executable"), "{rendered}");
    assert!(rendered.contains("0 failure"), "{rendered}");
}

#[cfg(unix)]
#[test]
fn a_broken_omp_project_link_reports_its_chain_without_mutation() {
    let fixture = Fixture::new("doctor-omp-broken-link");
    fs::create_dir_all(fixture.project.join(".omp")).expect("OMP project scope");
    std::os::unix::fs::symlink(
        fixture.project.join("no-such-omp-store"),
        fixture.project.join(".omp/skills"),
    )
    .expect("broken OMP discovery link");
    let project_before = snapshot(&fixture.project);

    let output = fixture.doctor();

    assert_eq!(output.status.code(), Some(65));
    let rendered = String::from_utf8_lossy(&output.stdout);
    assert!(
        rendered.contains("[FAIL] project .omp/skills"),
        "{rendered}"
    );
    assert!(rendered.contains("exact chain:"), "{rendered}");
    assert!(rendered.contains("no changes were made"), "{rendered}");
    assert_eq!(snapshot(&fixture.project), project_before);
    assert!(!fixture.state.exists());
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
    assert!(rendered.contains("[PASS] omp executable"));
}

#[test]
fn untested_agent_version_is_unverified_without_failing_doctor() {
    let fixture = Fixture::new("doctor-version-unverified");

    let output = fixture
        .doctor_command()
        .env("SKILLMOUNT_TEST_CODEX_VERSION", "codex-cli 999.0.0")
        .output()
        .expect("untested-version doctor should run");

    assert!(output.status.success());
    assert!(output.stderr.is_empty());
    let rendered = String::from_utf8_lossy(&output.stdout);
    assert!(rendered.contains("[UNVERIFIED] codex executable"));
    assert!(rendered.contains("codex-cli 999.0.0"));
    assert!(rendered.contains("codex-cli 0.146.0"));
    assert!(rendered.contains("docs/compatibility.md"));
    assert!(rendered.contains("[PASS] claude executable"));
    assert!(rendered.contains("0 failure"));
    assert!(!fixture.state.exists());
}

#[test]
fn unavailable_agent_version_is_unverified_without_suppressing_other_checks() {
    let fixture = Fixture::new("doctor-version-unavailable");

    let output = fixture
        .doctor_command()
        .env("SKILLMOUNT_TEST_CODEX_VERSION", "x".repeat(1025))
        .output()
        .expect("unavailable-version doctor should run");

    assert!(output.status.success());
    assert!(output.stderr.is_empty());
    let rendered = String::from_utf8_lossy(&output.stdout);
    assert!(rendered.contains("[UNVERIFIED] codex executable"));
    assert!(rendered.contains("1024-byte observation bound"));
    assert!(rendered.contains("codex-cli 0.146.0"));
    assert!(rendered.contains("[PASS] claude executable"));
    assert!(rendered.contains("[PASS] codex discovery"));
    assert!(rendered.contains("0 failure"));
    assert!(!fixture.state.exists());
}

#[test]
fn enforced_launch_configuration_fails_doctor_without_suppressing_other_checks() {
    let fixture = Fixture::new("doctor-enforced-configuration");
    let project_before = snapshot(&fixture.project);

    let output = fixture
        .doctor_command()
        .env("SKILLMOUNT_TEST_CODEX_MANAGED_CONFIG", "present")
        .env("CLAUDE_CODE_SAFE_MODE", "1")
        .output()
        .expect("enforced-configuration doctor should run");

    assert_eq!(output.status.code(), Some(65));
    assert!(output.stderr.is_empty());
    let rendered = String::from_utf8_lossy(&output.stdout);
    assert!(rendered.contains("[FAIL] codex executable"), "{rendered}");
    assert!(
        rendered.contains("legacy managed configuration"),
        "{rendered}"
    );
    assert!(rendered.contains("[FAIL] claude executable"), "{rendered}");
    assert!(rendered.contains("CLAUDE_CODE_SAFE_MODE"), "{rendered}");
    assert!(rendered.contains("[PASS] codex discovery"), "{rendered}");
    assert!(rendered.contains("[PASS] claude discovery"), "{rendered}");
    assert!(rendered.contains("2 failure"), "{rendered}");
    assert_eq!(snapshot(&fixture.project), project_before);
    assert!(
        !fixture.state.exists(),
        "doctor must not create transaction state"
    );
}

/// `doctor` inspects each requested Agent independently.
///
/// An unusable `CODEX_HOME` is fatal for Codex and irrelevant to Claude, so it must produce exactly
/// one failure while every Claude, layout, link, lock, and transaction finding still runs.
#[test]
fn one_agents_unusable_configuration_does_not_decide_the_other_agent() {
    let fixture = Fixture::new("doctor-agent-isolation");
    let project_before = snapshot(&fixture.project);

    let output = fixture
        .doctor_command()
        .env("CODEX_HOME", fixture.root.join("no-such-codex-home"))
        .output()
        .expect("isolated-configuration doctor should run");

    assert_eq!(output.status.code(), Some(65));
    let rendered = String::from_utf8_lossy(&output.stdout);
    assert!(rendered.contains("[FAIL] codex executable"), "{rendered}");
    assert!(rendered.contains("CODEX_HOME"), "{rendered}");
    assert!(rendered.contains("[PASS] claude executable"), "{rendered}");
    assert!(rendered.contains("[PASS] claude discovery"), "{rendered}");
    assert!(
        rendered.contains("[PASS] project .claude/skills"),
        "{rendered}"
    );
    assert!(rendered.contains("1 failure"), "{rendered}");
    assert_eq!(snapshot(&fixture.project), project_before);
    assert!(
        !fixture.state.exists(),
        "doctor must not create transaction state"
    );
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

#[test]
fn doctor_classifies_free_transaction_states_without_mutation() {
    struct Case<'a> {
        label: &'a str,
        boundary: Option<&'a str>,
        extra: &'a [&'a str],
        expected_severity: &'a str,
        expected_action: &'a str,
    }
    let cases = [
        Case {
            label: "planned",
            boundary: Some("journal-planned"),
            extra: &[],
            expected_severity: "[WARN] transaction state",
            expected_action: "transaction is incomplete",
        },
        Case {
            label: "supervising",
            boundary: Some("journal-supervising"),
            extra: &[],
            expected_severity: "[UNVERIFIED] transaction state",
            expected_action: "child process domain may still use these mounts",
        },
        Case {
            label: "completed",
            boundary: Some("journal-completed"),
            extra: &[],
            expected_severity: "[WARN] transaction state",
            expected_action: "terminal completed journal remains",
        },
        Case {
            label: "kept",
            boundary: None,
            extra: &["--keep-mounts"],
            expected_severity: "[WARN] transaction state",
            expected_action: "mounts were intentionally kept",
        },
    ];

    for case in cases {
        let Case {
            label,
            boundary,
            extra,
            expected_severity,
            expected_action,
        } = case;
        let fixture = Fixture::new(&format!("doctor-{label}-journal"));
        fixture.skill("alpha");
        let session = boundary.map_or_else(
            || {
                fixture
                    .session_command(extra)
                    .output()
                    .expect("kept session should run")
            },
            |boundary| fixture.run_stopping_at(boundary, extra),
        );
        assert!(
            !session.status.success(),
            "fixture child or checkpoint must stop"
        );
        let before = snapshot(&fixture.root);

        let output = fixture.doctor();

        assert!(
            output.status.success(),
            "{label}: {}",
            String::from_utf8_lossy(&output.stdout)
        );
        let rendered = String::from_utf8_lossy(&output.stdout);
        assert!(rendered.contains(expected_severity), "{label}: {rendered}");
        assert!(
            rendered.contains(&format!(" is {label}:")),
            "{label}: {rendered}"
        );
        assert!(rendered.contains(expected_action), "{label}: {rendered}");
        assert_eq!(
            snapshot(&fixture.root),
            before,
            "doctor mutated the {label} fixture"
        );
    }
}

#[test]
fn doctor_reports_a_lock_held_active_transaction_without_mutation() {
    let fixture = Fixture::new("doctor-active-journal");
    fixture.skill("alpha");
    let hold_log = fixture.root.join("hold.log");
    let release = fixture.root.join("release");
    let stderr = fs::File::create(&hold_log).expect("hold log");
    let mut holder = fixture
        .session_command(&[])
        .env("SKILLMOUNT_HOLD_AT", "journal-active")
        .env("SKILLMOUNT_HOLD_MS", "15000")
        .env("SKILLMOUNT_HOLD_UNTIL", &release)
        .stdout(Stdio::null())
        .stderr(Stdio::from(stderr))
        .spawn()
        .expect("active fixture session");
    wait_for(|| {
        fs::read_to_string(&hold_log).is_ok_and(|text| {
            text.split_inclusive('\n').any(|line| {
                line.ends_with('\n') && line.contains("failure injection holding at journal-active")
            })
        })
    });
    let before = snapshot(&fixture.root);

    let output = fixture.doctor();

    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stdout)
    );
    let rendered = String::from_utf8_lossy(&output.stdout);
    assert!(rendered.contains("[WARN] transaction state"), "{rendered}");
    assert!(
        rendered.contains(" is active and its OS advisory lock is held"),
        "{rendered}"
    );
    assert!(
        rendered.contains("session is active and was left alone"),
        "{rendered}"
    );
    assert_eq!(
        snapshot(&fixture.root),
        before,
        "doctor mutated the active fixture"
    );

    fs::write(&release, b"release\n").expect("release active fixture");
    let status = holder.wait().expect("active fixture should finish");
    assert_eq!(
        status.code(),
        Some(64),
        "{}",
        fs::read_to_string(&hold_log).unwrap_or_default()
    );
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

#[cfg(unix)]
#[test]
fn doctor_escapes_line_and_terminal_controls_in_link_targets() {
    use std::ffi::OsString;
    use std::os::unix::ffi::OsStringExt;
    use std::os::unix::fs::symlink;

    let fixture = Fixture::new("doctor-control-target");
    fs::create_dir(fixture.project.join(".agents")).expect(".agents fixture");
    let target = OsString::from_vec(b"missing\n[PASS] forged\x1b]52;clipboard\x07".to_vec());
    symlink(&target, fixture.project.join(".agents/skills"))
        .expect("control-character discovery symlink");

    let output = fixture.doctor();

    assert_eq!(output.status.code(), Some(65));
    let rendered = String::from_utf8_lossy(&output.stdout);
    assert!(rendered.contains("\\u{A}"), "{rendered}");
    assert!(rendered.contains("\\u{1B}"), "{rendered}");
    assert!(rendered.contains("\\u{7}"), "{rendered}");
    assert!(!rendered.contains("\n[PASS] forged"), "{rendered}");
    assert!(!rendered.contains('\u{1B}'), "{rendered}");
}

#[test]
fn top_level_doctor_errors_escape_controls_in_user_supplied_paths() {
    let fixture = Fixture::new("doctor-control-project-error");
    let missing = fixture
        .root
        .join("missing\n[PASS] forged\u{1B}]52;clipboard\u{7}");

    let output = fixture
        .command()
        .arg("doctor")
        .arg("--project-root")
        .arg(missing)
        .arg("--codex-bin")
        .arg(ASM)
        .arg("--claude-bin")
        .arg(ASM)
        .output()
        .expect("doctor with an unavailable controlled path should run");

    assert_eq!(output.status.code(), Some(66));
    let rendered = String::from_utf8_lossy(&output.stderr);
    assert!(rendered.contains("\\u{A}"), "{rendered}");
    assert!(rendered.contains("\\u{1B}"), "{rendered}");
    assert!(rendered.contains("\\u{7}"), "{rendered}");
    assert!(!rendered.contains("\n[PASS] forged"), "{rendered}");
    assert!(!rendered.contains('\u{1B}'), "{rendered}");
}

fn install_agent_alias(directory: &Path, name: &str) {
    #[cfg(unix)]
    std::os::unix::fs::symlink(ASM, directory.join(name)).expect("agent executable symlink");

    #[cfg(windows)]
    {
        fs::copy(ASM, directory.join(format!("{name}.exe"))).expect("agent executable copy");
    }
}

fn wait_for(condition: impl Fn() -> bool) {
    let deadline = Instant::now() + Duration::from_secs(5);
    while Instant::now() < deadline {
        if condition() {
            return;
        }
        std::thread::sleep(Duration::from_millis(10));
    }
    panic!("the spawned session did not reach its checkpoint");
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
