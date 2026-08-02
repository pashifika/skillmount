//! Proves that every read-only command leaves the filesystem and process state untouched.
//!
//! The unit suite asserts this at the planning boundary. This suite asserts it for the shipped
//! executable, including the error paths, because that is what an operator actually runs.

use std::collections::BTreeMap;
use std::fs;
use std::path::{Path, PathBuf};
use std::process::{Command, Output};
use std::time::{SystemTime, UNIX_EPOCH};

const ASM: &str = env!("CARGO_BIN_EXE_asm");

/// A fixture holding a project, a Skill source, and a private home directory.
///
/// The home directory is redirected so a Claude staging plan resolves inside the fixture. That
/// makes "the session root was not created" an assertion this suite can actually make, instead of
/// a claim about the developer's real home.
struct Fixture {
    root: PathBuf,
    project: PathBuf,
    sources: PathBuf,
    home: PathBuf,
    launch_sentinel: PathBuf,
    agent_bin: PathBuf,
}

impl Fixture {
    fn new(label: &str) -> Self {
        let nonce = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .expect("clock after epoch")
            .as_nanos();
        let root = std::env::temp_dir().join(format!(
            "skillmount-readonly-{label}-{}-{nonce}",
            std::process::id()
        ));
        let project = root.join("project");
        let sources = root.join("sources");
        let home = root.join("home");
        for path in [&project, &sources, &home] {
            fs::create_dir_all(path).expect("fixture directory");
        }
        let launch_sentinel = root.join("agent-was-launched");
        let agent_bin = write_fake_agent(&root, &launch_sentinel);
        Self {
            root,
            project,
            sources,
            home,
            launch_sentinel,
            agent_bin,
        }
    }

    fn skill(&self, name: &str) -> PathBuf {
        let path = self.sources.join(name);
        fs::create_dir_all(&path).expect("skill directory");
        fs::write(
            path.join("SKILL.md"),
            format!("---\nname: {name}\ndescription: {name} description\n---\n"),
        )
        .expect("SKILL.md");
        path
    }

    fn run(&self, arguments: &[&str]) -> Output {
        let mut command = Command::new(ASM);
        command
            .current_dir(&self.project)
            .args(arguments)
            .env("HOME", &self.home)
            .env("USERPROFILE", &self.home)
            .env("LOCALAPPDATA", self.home.join("AppData/Local"));
        command.output().expect("asm should run")
    }

    /// Roots that must be byte-identical before and after a read-only command.
    fn watched(&self) -> Vec<PathBuf> {
        vec![
            self.project.clone(),
            self.sources.clone(),
            self.home.clone(),
        ]
    }

    /// Roots holding data that belongs to the operator rather than to `SkillMount`.
    ///
    /// A mutating session legitimately creates `SkillMount`'s own state directories — journals and
    /// lock files — before it can decide whether it may proceed. Those are not the operator's data,
    /// so a test about a failing session watches only the project and the Skill sources.
    fn user_data(&self) -> Vec<PathBuf> {
        vec![self.project.clone(), self.sources.clone()]
    }

    fn assert_unchanged(&self, arguments: &[&str]) -> Output {
        self.assert_roots_unchanged(&self.watched(), arguments)
    }

    fn assert_roots_unchanged(&self, roots: &[PathBuf], arguments: &[&str]) -> Output {
        let before = roots
            .iter()
            .map(|root| (root.clone(), snapshot(root)))
            .collect::<Vec<_>>();

        let output = self.run(arguments);

        for (root, expected) in before {
            assert_eq!(
                snapshot(&root),
                expected,
                "`asm {}` modified {}",
                arguments.join(" "),
                root.display()
            );
        }
        assert!(
            !self.launch_sentinel.exists(),
            "`asm {}` launched a child process",
            arguments.join(" ")
        );
        output
    }
}

impl Drop for Fixture {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.root);
    }
}

/// Writes an executable that records the fact it ran, so a launch cannot pass unnoticed.
fn write_fake_agent(root: &Path, sentinel: &Path) -> PathBuf {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;

        let path = root.join("fake-agent");
        fs::write(&path, format!("#!/bin/sh\ntouch {}\n", sentinel.display())).expect("fake agent");
        fs::set_permissions(&path, fs::Permissions::from_mode(0o755)).expect("fake agent mode");
        path
    }
    #[cfg(windows)]
    {
        let path = root.join("fake-agent.cmd");
        fs::write(
            &path,
            format!("@echo off\r\ntype nul > \"{}\"\r\n", sentinel.display()),
        )
        .expect("fake agent");
        path
    }
}

/// Records a tree without following links, so a replaced link shows up as a difference.
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
            fs::read_link(current).map_or_else(|_| "?".into(), |t| t.display().to_string())
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

#[test]
fn inspect_reports_both_agents_without_touching_anything() {
    let fixture = Fixture::new("inspect");
    fixture.skill("alpha");
    let sources = fixture.sources.to_string_lossy().into_owned();

    let output = fixture.assert_unchanged(&["inspect", "--skills-dir", &sources]);

    assert!(output.status.success());
    let rendered = String::from_utf8_lossy(&output.stdout);
    assert!(rendered.contains("Agent:          codex"));
    assert!(rendered.contains("Agent:          claude"));
    assert!(rendered.contains("Overlay: 1 Skill(s)"));
}

#[test]
fn inspect_can_be_filtered_to_one_agent() {
    let fixture = Fixture::new("inspect-filter");
    fixture.skill("alpha");
    let sources = fixture.sources.to_string_lossy().into_owned();

    let output =
        fixture.assert_unchanged(&["inspect", "--skills-dir", &sources, "--agent", "claude"]);

    assert!(output.status.success());
    let rendered = String::from_utf8_lossy(&output.stdout);
    assert!(rendered.contains("Agent:          claude"));
    assert!(!rendered.contains("Agent:          codex"));
}

#[test]
fn a_codex_dry_run_plans_the_whole_layout_without_creating_it() {
    let fixture = Fixture::new("dry-run-codex");
    fixture.skill("alpha");
    let sources = fixture.sources.to_string_lossy().into_owned();

    let output = fixture.assert_unchanged(&["codex", "--skills-dir", &sources, "--dry-run"]);

    assert!(output.status.success());
    let rendered = String::from_utf8_lossy(&output.stdout);
    assert!(rendered.contains("MKDIR  .codex"));
    assert!(
        rendered.contains("LINK   .agents/skills") || rendered.contains("LINK   .agents\\skills")
    );
    assert!(!fixture.project.join(".codex").exists());
    assert!(!fixture.project.join(".agents").exists());
}

#[test]
fn a_claude_dry_run_never_creates_a_session_root() {
    let fixture = Fixture::new("dry-run-claude");
    fixture.skill("alpha");
    let sources = fixture.sources.to_string_lossy().into_owned();

    let output = fixture.assert_unchanged(&["claude", "--skills-dir", &sources, "--dry-run"]);

    assert!(output.status.success());
    let rendered = String::from_utf8_lossy(&output.stdout);
    assert!(rendered.contains("<session-id>"), "{rendered}");
    assert!(rendered.contains("argv[1] = --add-dir"));
    assert!(
        !fixture.home.join("Library/Caches/skillmount").exists()
            && !fixture.home.join("AppData/Local/skillmount").exists(),
        "no session or transaction storage may be created"
    );
}

#[test]
fn a_destination_conflict_fails_without_changing_the_project() {
    let fixture = Fixture::new("conflict");
    fixture.skill("alpha");
    fs::create_dir_all(fixture.project.join(".codex/skills/alpha")).expect("conflicting entry");
    let sources = fixture.sources.to_string_lossy().into_owned();

    let output = fixture.assert_unchanged(&["codex", "--skills-dir", &sources, "--dry-run"]);

    assert_eq!(
        output.status.code(),
        Some(73),
        "a destination conflict is a filesystem-state failure"
    );
    assert!(String::from_utf8_lossy(&output.stderr).contains("conflicts with"));
}

#[test]
fn a_skip_policy_preserves_the_existing_entry_and_reports_it() {
    let fixture = Fixture::new("skip");
    fixture.skill("alpha");
    fs::create_dir_all(fixture.project.join(".codex/skills/alpha")).expect("existing entry");
    let sources = fixture.sources.to_string_lossy().into_owned();

    let output = fixture.assert_unchanged(&[
        "codex",
        "--skills-dir",
        &sources,
        "--dry-run",
        "--conflict",
        "skip",
    ]);

    assert!(output.status.success());
    assert!(String::from_utf8_lossy(&output.stdout).contains("KEEP"));
}

#[test]
fn input_and_catalog_failures_keep_their_stable_codes_and_change_nothing() {
    let fixture = Fixture::new("failures");
    let missing = fixture.root.join("absent");
    let missing_output =
        fixture.assert_unchanged(&["inspect", "--skills-dir", &missing.to_string_lossy()]);
    assert_eq!(missing_output.status.code(), Some(66));

    let invalid = fixture.root.join("invalid");
    fs::create_dir_all(&invalid).expect("invalid fixture");
    fs::write(invalid.join("SKILL.md"), "no frontmatter\n").expect("invalid SKILL.md");
    let invalid_output =
        fixture.assert_unchanged(&["inspect", "--skills-dir", &invalid.to_string_lossy()]);
    assert_eq!(invalid_output.status.code(), Some(65));
}

#[test]
fn a_skill_disabling_claude_argument_is_rejected_before_planning() {
    let fixture = Fixture::new("claude-rejected-args");
    fixture.skill("alpha");
    let sources = fixture.sources.to_string_lossy().into_owned();

    let output = fixture.assert_unchanged(&[
        "claude",
        "--skills-dir",
        &sources,
        "--dry-run",
        "--",
        "--bare",
    ]);

    assert_eq!(output.status.code(), Some(64));
    assert!(String::from_utf8_lossy(&output.stderr).contains("--bare"));
}

#[test]
fn a_session_that_fails_before_the_mutation_boundary_changes_nothing() {
    let fixture = Fixture::new("mutation-boundary");
    fixture.skill("alpha");
    // A destination the plan cannot resolve, so the run fails while it is still read-only. A
    // session that gets past planning is no longer a read-only path and belongs to
    // `tests/transaction.rs`; what this suite still owns is the guarantee that a failure on the
    // way there costs nothing.
    fs::create_dir_all(fixture.project.join(".codex/skills/alpha")).expect("conflicting entry");
    let sources = fixture.sources.to_string_lossy().into_owned();
    let agent_bin = fixture.agent_bin.to_string_lossy().into_owned();

    let output = fixture.assert_roots_unchanged(
        &fixture.user_data(),
        &["codex", "--skills-dir", &sources, "--agent-bin", &agent_bin],
    );

    assert_eq!(output.status.code(), Some(73));
    assert!(String::from_utf8_lossy(&output.stderr).contains("conflicts with"));
}

#[test]
fn verbose_output_names_the_rightmost_winner_and_every_shadowed_origin() {
    let fixture = Fixture::new("provenance");
    let base = fixture.root.join("base-skills");
    let team = fixture.root.join("team-skills");
    for root in [&base, &team] {
        let skill = root.join("alpha");
        fs::create_dir_all(&skill).expect("source");
        fs::write(
            skill.join("SKILL.md"),
            "---\nname: alpha\ndescription: alpha description\n---\n",
        )
        .expect("SKILL.md");
    }

    let output = fixture.assert_unchanged(&[
        "codex",
        "--skills-dir",
        &base.to_string_lossy(),
        "--skills-dir",
        &team.to_string_lossy(),
        "--dry-run",
        "--verbose",
    ]);

    assert!(output.status.success());
    let rendered = String::from_utf8_lossy(&output.stdout);
    assert!(
        rendered.contains("Overlay: 1 Skill(s), 1 source override(s)"),
        "{rendered}"
    );
    assert!(rendered.contains("OVERRIDE  alpha"), "{rendered}");
    assert!(
        rendered.contains("different source"),
        "the displaced origin and its reason must be listed: {rendered}"
    );
    // The selected origin is reported canonically, so the expectation is canonicalized too: a
    // temporary directory sits behind a symbolic link on macOS.
    let winner = fs::canonicalize(team.join("alpha")).expect("canonical winner");
    assert!(
        rendered.contains(&format!("-> [2] {}", winner.display())),
        "the rightmost occurrence wins: {rendered}"
    );
}

#[test]
fn an_existing_transaction_record_is_reported_and_left_alone() {
    let fixture = Fixture::new("recovery");
    fixture.skill("alpha");
    let transactions = if cfg!(windows) {
        fixture.home.join("AppData/Local/skillmount/transactions")
    } else {
        fixture
            .home
            .join("Library/Application Support/skillmount/transactions")
    };
    fs::create_dir_all(&transactions).expect("transaction directory");
    let record = transactions.join("01JEXAMPLE.journal");
    fs::write(&record, "not a journal this build wrote\n").expect("transaction record");
    let sources = fixture.sources.to_string_lossy().into_owned();

    let output = fixture.assert_unchanged(&["codex", "--skills-dir", &sources, "--dry-run"]);

    assert!(output.status.success());
    assert!(
        String::from_utf8_lossy(&output.stdout).contains("WOULD RETAIN"),
        "a journal this build cannot interpret must be reported rather than ignored: {}",
        String::from_utf8_lossy(&output.stdout)
    );
    assert_eq!(
        fs::read_to_string(&record).expect("record still readable"),
        "not a journal this build wrote\n",
        "a dry run must not recover, rewrite, or remove a transaction journal"
    );
}

#[test]
fn passthrough_values_with_shell_metacharacters_stay_separate_indexed_values() {
    let fixture = Fixture::new("metacharacters");
    fixture.skill("alpha");
    let sources = fixture.sources.to_string_lossy().into_owned();
    let awkward = ["--flag", "a b\"c'd;e", "$(id)", "x && y", "back\\slash"];

    let mut arguments = vec![
        "codex",
        "--skills-dir",
        &sources,
        "--dry-run",
        "--verbose",
        "--",
    ];
    arguments.extend(awkward);
    let output = fixture.assert_unchanged(&arguments);

    assert!(output.status.success());
    let rendered = String::from_utf8_lossy(&output.stdout);
    for (index, value) in awkward.iter().enumerate() {
        assert!(
            rendered.contains(&format!("[{index}] {value}")),
            "forwarded value {index} must appear verbatim on its own line: {rendered}"
        );
        assert!(
            rendered.contains(&format!("argv[{}] = {value}", index + 1)),
            "effective argv must index the same value: {rendered}"
        );
    }
    assert!(
        !rendered
            .lines()
            .any(|line| line.contains(awkward[0]) && line.contains(awkward[1])),
        "two forwarded values must never share a line, which is what a joined command string \
         would produce: {rendered}"
    );
}

#[test]
fn read_only_output_is_identical_across_runs() {
    let fixture = Fixture::new("deterministic");
    fixture.skill("gamma");
    fixture.skill("alpha");
    let sources = fixture.sources.to_string_lossy().into_owned();

    let first = fixture.run(&["inspect", "--skills-dir", &sources]);
    let second = fixture.run(&["inspect", "--skills-dir", &sources]);

    assert_eq!(first.stdout, second.stdout);
    assert_eq!(first.stderr, second.stderr);
}
