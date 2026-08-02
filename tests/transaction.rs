//! Crash recovery and concurrency, proved with real processes.
//!
//! Every test here kills or stalls a real `asm` process at a real boundary and then runs a real
//! second invocation against whatever the first one left on disk. That is the only evidence that
//! matters for this behaviour: a unit test that hand-writes a `staged` journal proves recovery can
//! read that journal, not that the apply sequence ever produces it, and not that the filesystem
//! matches what the journal claims.
//!
//! Failure injection is compiled only into a debug build, which is what `cargo test` produces, so
//! these tests exercise the same code path a shipped binary runs minus the checkpoint itself.

use std::fs;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Output};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

const ASM: &str = env!("CARGO_BIN_EXE_asm");

/// Every boundary the transaction layer announces, in the order a session reaches them.
///
/// Kept as literals rather than imported from the crate on purpose: the names are a contract
/// between the library and this suite, and a rename that silently updated both sides would turn a
/// crash test into a no-crash test without anyone noticing.
const BOUNDARIES: [&str; 11] = [
    "journal-planned",
    "journal-applying",
    "action-intent",
    "temporary-created",
    "action-staged",
    "final-placed",
    "action-applied",
    "journal-active",
    "journal-cleaning",
    "entry-removed",
    "directory-removed",
];

/// A project, a Skill source, and a private state root.
struct Fixture {
    root: PathBuf,
    project: PathBuf,
    sources: PathBuf,
    state: PathBuf,
}

impl Fixture {
    fn new(label: &str) -> Self {
        let nonce = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .expect("clock after epoch")
            .as_nanos();
        let root = std::env::temp_dir().join(format!(
            "skillmount-txn-{label}-{}-{nonce}",
            std::process::id()
        ));
        let fixture = Self {
            project: root.join("project"),
            sources: root.join("sources"),
            state: root.join("state"),
            root,
        };
        for path in [&fixture.project, &fixture.sources] {
            fs::create_dir_all(path).expect("fixture directory");
        }
        fixture
    }

    fn skill(&self, name: &str) -> &Self {
        let path = self.sources.join(name);
        fs::create_dir_all(&path).expect("skill directory");
        fs::write(
            path.join("SKILL.md"),
            format!("---\nname: {name}\ndescription: {name} description\n---\n"),
        )
        .expect("SKILL.md");
        self
    }

    /// Builds a session command with every redirection this suite depends on.
    ///
    /// The project root and the state root are both redirected. Without the first, a session would
    /// resolve the project from the harness's working directory and mount into this repository;
    /// without the second, it would write journals and locks into the developer's real
    /// application-support directory and contend with concurrent test runs.
    fn command(&self, agent: &str, extra: &[&str]) -> Command {
        let mut command = Command::new(ASM);
        command
            .arg(agent)
            .arg("--skills-dir")
            .arg(&self.sources)
            .arg("--project-root")
            .arg(&self.project)
            .arg("--cwd")
            .arg(&self.project)
            .args(extra)
            .env("SKILLMOUNT_STATE_DIR", &self.state)
            // Contention must be reported rather than waited out, so a serialization test finishes
            // in milliseconds instead of the production timeout.
            .env("SKILLMOUNT_LOCK_WAIT_MS", "200")
            .current_dir(&self.project);
        command
    }

    fn run(&self, agent: &str, extra: &[&str]) -> Output {
        self.command(agent, extra).output().expect("asm should run")
    }

    /// Runs a session that aborts at `boundary`.
    fn run_stopping_at(&self, agent: &str, boundary: &str, extra: &[&str]) -> Output {
        self.command(agent, extra)
            .env("SKILLMOUNT_STOP_AT", boundary)
            .output()
            .expect("asm should run")
    }

    fn transactions(&self) -> PathBuf {
        self.state.join("transactions")
    }

    /// Returns every journal file currently present.
    fn journals(&self) -> Vec<PathBuf> {
        let mut found = fs::read_dir(self.transactions()).map_or_else(
            |_| Vec::new(),
            |entries| {
                entries
                    .filter_map(Result::ok)
                    .map(|entry| entry.path())
                    .filter(|path| {
                        path.extension()
                            .is_some_and(|extension| extension == "journal")
                    })
                    .collect()
            },
        );
        found.sort();
        found
    }

    /// Returns every entry beneath the project, so residue is visible in an assertion message.
    fn project_tree(&self) -> Vec<String> {
        let mut entries = Vec::new();
        collect(&self.project, &self.project, &mut entries);
        entries.sort();
        entries
    }
}

impl Drop for Fixture {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.root);
    }
}

fn collect(root: &Path, current: &Path, entries: &mut Vec<String>) {
    let Ok(metadata) = fs::symlink_metadata(current) else {
        return;
    };
    if let Ok(relative) = current.strip_prefix(root) {
        if !relative.as_os_str().is_empty() {
            entries.push(relative.display().to_string());
        }
    }
    if metadata.is_dir() && !metadata.file_type().is_symlink() {
        let Ok(children) = fs::read_dir(current) else {
            return;
        };
        for child in children.flatten() {
            collect(root, &child.path(), entries);
        }
    }
}

/// Returns whether a path exists without following it.
fn exists(path: &Path) -> bool {
    fs::symlink_metadata(path).is_ok()
}

#[test]
fn every_named_boundary_actually_stops_a_session() {
    for boundary in BOUNDARIES {
        let fixture = Fixture::new(&format!("boundary-{boundary}"));
        fixture.skill("alpha");

        let output = fixture.run_stopping_at("codex", boundary, &[]);

        assert!(
            !output.status.success(),
            "{boundary} did not stop the session"
        );
        assert!(
            String::from_utf8_lossy(&output.stderr)
                .contains(&format!("stopping at {boundary} occurrence")),
            "{boundary} was never reached, so nothing about it is under test: {}",
            String::from_utf8_lossy(&output.stderr)
        );
    }
}

#[test]
fn a_second_invocation_recovers_every_boundary_and_leaves_the_project_clean() {
    for boundary in BOUNDARIES {
        let fixture = Fixture::new(&format!("recover-{boundary}"));
        fixture.skill("alpha");

        let killed = fixture.run_stopping_at("codex", boundary, &[]);
        assert!(!killed.status.success(), "{boundary} must stop the session");

        // A real second invocation, against whatever the first one left behind.
        let recovered = fixture.run("codex", &[]);

        assert_eq!(
            recovered.status.code(),
            Some(70),
            "the recovering session must reach the launch boundary at {boundary}: {}",
            String::from_utf8_lossy(&recovered.stderr)
        );
        assert!(
            exists(&fixture.sources.join("alpha/SKILL.md")),
            "no boundary may ever cost a Skill source"
        );

        // One boundary genuinely cannot be reconciled automatically. `temporary-created` stops
        // between creating the temporary entry and recording its identity, so the entry exists and
        // nothing proves which transaction made it. Retaining it is the specified outcome — residue
        // over deleting an unowned entry — and both the entry and the journal describing it stay,
        // reported, so an operator can finish the job.
        let residue = fixture.project_tree();
        if boundary == "temporary-created" {
            assert!(
                residue.iter().any(|entry| entry.contains(".skillmount-")),
                "the unprovable staged entry must still be present at {boundary}: {residue:?}"
            );
            assert!(
                String::from_utf8_lossy(&recovered.stderr).contains("retained"),
                "retained residue must be reported: {}",
                String::from_utf8_lossy(&recovered.stderr)
            );
            assert_eq!(
                fixture.journals().len(),
                1,
                "the journal describing unreconciled residue is kept: {:?}",
                fixture.journals()
            );
        } else {
            assert!(
                residue.is_empty(),
                "a session stopped at {boundary} must leave the project clean once recovered: \
                 {residue:?}"
            );
            assert!(
                fixture.journals().is_empty(),
                "recovery plus a completed session must leave no journal at {boundary}: {:?}",
                fixture.journals()
            );
        }
    }
}

#[test]
fn recovery_removes_a_staged_entry_that_was_never_placed() {
    let fixture = Fixture::new("staged-only");
    fixture.skill("alpha");

    // The second `action-staged` occurrence is the store directory: action 1 is `.codex`, action 2
    // is `.codex/skills`. Stopping there leaves a staged sibling whose identity is durable.
    fixture.run_stopping_at("codex", "action-staged@2", &[]);
    let staged_before = fixture
        .project_tree()
        .into_iter()
        .filter(|entry| entry.contains(".skillmount-"))
        .collect::<Vec<_>>();

    let recovered = fixture.run("codex", &[]);

    assert_eq!(
        staged_before.len(),
        1,
        "the fixture must actually leave a staged entry: {staged_before:?}"
    );
    assert_eq!(recovered.status.code(), Some(70));
    assert!(
        !fixture
            .project_tree()
            .iter()
            .any(|entry| entry.contains(".skillmount-")),
        "a staged entry with a durable identity must be removable: {:?}",
        fixture.project_tree()
    );
}

#[test]
fn recovery_removes_an_entry_placed_before_its_applied_record() {
    let fixture = Fixture::new("placed-not-applied");
    fixture.skill("alpha");

    // Stopping after the last placement leaves the journal saying `staged` while the entry already
    // occupies its final path. Recovery has to inspect both paths and remove the matching one.
    fixture.run_stopping_at("codex", "final-placed@5", &[]);
    let mounted = fixture.project.join(".codex/skills/alpha");
    assert!(
        exists(&mounted),
        "the fixture must leave the mount in place: {:?}",
        fixture.project_tree()
    );

    let recovered = fixture.run("codex", &[]);

    assert_eq!(recovered.status.code(), Some(70));
    assert!(
        fixture.project_tree().is_empty(),
        "recovery must reconcile the placed-but-unrecorded entry: {:?}",
        fixture.project_tree()
    );
}

#[test]
fn recovery_never_removes_an_entry_a_user_replaced_after_the_crash() {
    let fixture = Fixture::new("replaced-after-crash");
    fixture.skill("alpha");
    fixture.run_stopping_at("codex", "journal-active", &[]);

    // The operator replaces the mount with work of their own before the next session runs.
    let mounted = fixture.project.join(".codex/skills/alpha");
    if cfg!(windows) {
        fs::remove_dir(&mounted)
    } else {
        fs::remove_file(&mounted)
    }
    .expect("the crashed session left a link here");
    fs::create_dir_all(mounted.join("their-own-work")).expect("replacement");

    let recovered = fixture.run("codex", &[]);

    assert!(
        exists(&mounted.join("their-own-work")),
        "recovery must never delete something it cannot prove it created"
    );
    assert!(
        String::from_utf8_lossy(&recovered.stderr).contains("retained"),
        "the mismatch must be reported: {}",
        String::from_utf8_lossy(&recovered.stderr)
    );
}

#[test]
fn no_recover_fails_closed_and_changes_nothing() {
    let fixture = Fixture::new("no-recover");
    fixture.skill("alpha");
    fixture.run_stopping_at("codex", "journal-active", &[]);
    let before = fixture.project_tree();
    let journals_before = fixture.journals();

    let refused = fixture.run("codex", &["--no-recover"]);

    assert_eq!(
        refused.status.code(),
        Some(75),
        "unreconciled state under --no-recover is a temporary failure: {}",
        String::from_utf8_lossy(&refused.stderr)
    );
    let stderr = String::from_utf8_lossy(&refused.stderr);
    assert!(stderr.contains("--no-recover"), "{stderr}");
    assert!(stderr.contains("nothing was changed"), "{stderr}");
    assert_eq!(fixture.project_tree(), before);
    assert_eq!(fixture.journals(), journals_before);
}

#[test]
fn no_recover_succeeds_when_there_is_nothing_to_reconcile() {
    let fixture = Fixture::new("no-recover-clean");
    fixture.skill("alpha");

    let output = fixture.run("codex", &["--no-recover"]);

    assert_eq!(
        output.status.code(),
        Some(70),
        "a clean state must not be refused: {}",
        String::from_utf8_lossy(&output.stderr)
    );
}

#[test]
fn a_kept_transaction_survives_every_later_session() {
    let fixture = Fixture::new("kept");
    fixture.skill("alpha");

    let kept = fixture.run("codex", &["--keep-mounts"]);
    assert_eq!(kept.status.code(), Some(70));
    let mounted = fixture.project.join(".codex/skills/alpha");
    assert!(exists(&mounted), "--keep-mounts retains the mounts");
    assert_eq!(fixture.journals().len(), 1, "the kept journal is retained");

    // A later session sees the mount, reuses it, and must not treat the terminal journal as stale.
    let later = fixture.run("codex", &[]);

    assert_eq!(
        later.status.code(),
        Some(70),
        "{}",
        String::from_utf8_lossy(&later.stderr)
    );
    assert!(
        exists(&mounted),
        "a terminal kept transaction is never recovered automatically"
    );
    assert_eq!(
        fixture.journals().len(),
        1,
        "only the kept journal remains: {:?}",
        fixture.journals()
    );
}

#[test]
fn a_journal_this_build_cannot_interpret_is_retained_and_reported() {
    let fixture = Fixture::new("corrupt-journal");
    fixture.skill("alpha");
    fs::create_dir_all(fixture.transactions()).expect("transaction directory");
    let corrupt = fixture.transactions().join("aaaa-bbbb.journal");
    fs::write(&corrupt, "skillmount-journal 99 unix deadbeef\n").expect("corrupt journal");

    let output = fixture.run("codex", &[]);

    assert_eq!(
        output.status.code(),
        Some(70),
        "an unrelated corrupt journal must not brick every session: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(
        String::from_utf8_lossy(&output.stderr).contains("cannot be interpreted"),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert_eq!(
        fs::read_to_string(&corrupt).expect("still readable"),
        "skillmount-journal 99 unix deadbeef\n",
        "a journal that cannot be read is never rewritten or removed"
    );

    let refused = fixture.run("codex", &["--no-recover"]);
    assert_eq!(
        refused.status.code(),
        Some(75),
        "under --no-recover an uninterpretable journal is a hard stop"
    );
}

#[test]
fn a_destination_conflict_rolls_back_and_leaves_the_obstruction() {
    let fixture = Fixture::new("late-conflict");
    fixture.skill("alpha").skill("beta");
    // Occupies the destination of a later Skill link, so earlier actions apply and one fails.
    fs::create_dir_all(fixture.project.join(".codex/skills/beta/mine")).expect("obstruction");

    let output = fixture.run("codex", &[]);

    assert_eq!(
        output.status.code(),
        Some(73),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(
        exists(&fixture.project.join(".codex/skills/beta/mine")),
        "the obstruction is never touched"
    );
    assert!(
        !exists(&fixture.project.join(".agents/skills")),
        "everything the failed transaction applied is rolled back: {:?}",
        fixture.project_tree()
    );
    assert!(
        !exists(&fixture.project.join(".codex/skills/alpha")),
        "including the mount that had already succeeded"
    );
}

#[test]
fn a_cleanup_that_cannot_finish_keeps_its_journal_and_its_evidence() {
    let fixture = Fixture::new("cleanup-blocked");
    fixture.skill("alpha");
    fixture.run_stopping_at("codex", "journal-active", &[]);

    // Something is added to the store, so the store directory can no longer be removed while the
    // mount inside it still can.
    fs::write(fixture.project.join(".codex/skills/notes.md"), "mine").expect("user content");
    let recovered = fixture.run("codex", &[]);

    assert!(
        exists(&fixture.project.join(".codex/skills/notes.md")),
        "the operator's file must survive"
    );
    let stderr = String::from_utf8_lossy(&recovered.stderr);
    assert!(stderr.contains("holds entries"), "{stderr}");
    assert!(
        !fixture.journals().is_empty(),
        "a cleanup that retained something keeps the journal describing it"
    );
}

#[test]
fn two_codex_sessions_on_one_store_serialize() {
    let fixture = Fixture::new("codex-serialized");
    fixture.skill("alpha");

    // The first session pauses while holding its locks, which is the window a second one must not
    // be able to enter.
    let mut holder: Child = fixture
        .command("codex", &[])
        .env("SKILLMOUNT_HOLD_AT", "journal-active")
        .env("SKILLMOUNT_HOLD_MS", "4000")
        .spawn()
        .expect("the first session should start");
    wait_for(|| !fixture.journals().is_empty());

    let contender = fixture.run("codex", &[]);
    let _ = holder.wait();

    assert_eq!(
        contender.status.code(),
        Some(75),
        "a second Codex session on the same store must wait or report a temporary failure: {}",
        String::from_utf8_lossy(&contender.stderr)
    );
    let stderr = String::from_utf8_lossy(&contender.stderr);
    assert!(
        stderr.contains("another SkillMount session holds"),
        "{stderr}"
    );
    assert!(stderr.contains("nothing was changed"), "{stderr}");
}

#[test]
fn two_isolated_claude_sessions_do_not_serialize() {
    let fixture = Fixture::new("claude-concurrent");
    fixture.skill("alpha");

    let mut holder: Child = fixture
        .command("claude", &[])
        .env("SKILLMOUNT_HOLD_AT", "journal-active")
        .env("SKILLMOUNT_HOLD_MS", "4000")
        .spawn()
        .expect("the first session should start");
    wait_for(|| !fixture.journals().is_empty());

    let concurrent = fixture.run("claude", &[]);
    let _ = holder.wait();

    assert_eq!(
        concurrent.status.code(),
        Some(70),
        "two Claude sessions with separate staging roots share no mutable resource: {}",
        String::from_utf8_lossy(&concurrent.stderr)
    );
    assert!(
        !String::from_utf8_lossy(&concurrent.stderr).contains("another SkillMount session holds"),
        "isolated staging must not serialize child execution"
    );
}

#[test]
fn a_claude_session_stages_under_its_own_identifier() {
    let fixture = Fixture::new("claude-staging-id");
    fixture.skill("alpha");

    let output = fixture.run("claude", &["--keep-mounts"]);

    assert_eq!(
        output.status.code(),
        Some(70),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let sessions = fs::read_dir(fixture.state.join("sessions"))
        .expect("a staging root was created")
        .filter_map(Result::ok)
        .map(|entry| entry.file_name().to_string_lossy().into_owned())
        .collect::<Vec<_>>();
    assert_eq!(sessions.len(), 1, "{sessions:?}");
    assert!(
        !sessions[0].contains('<'),
        "a mutating run must replace the planning placeholder with a real identifier: {sessions:?}"
    );
    assert!(
        exists(
            &fixture
                .state
                .join("sessions")
                .join(&sessions[0])
                .join("root/.claude/skills/alpha")
        ),
        "the Skill is staged inside that root"
    );
}

#[test]
fn a_claude_session_removes_its_whole_staging_root_at_cleanup() {
    let fixture = Fixture::new("claude-cleanup");
    fixture.skill("alpha").skill("beta");

    let output = fixture.run("claude", &[]);

    assert_eq!(
        output.status.code(),
        Some(70),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    // Claude staging creates a four-deep chain — `<id>`, `<id>/root`, `<id>/root/.claude`,
    // `<id>/root/.claude/skills` — so reverse-order removal carries more weight here than in the
    // Codex layout every other cleanup test covers. Its lock resources are unanchored too, which
    // is a different derivation from the anchored Codex ones.
    let staged = fs::read_dir(fixture.state.join("sessions"))
        .expect("the shared sessions directory survives")
        .filter_map(Result::ok)
        .map(|entry| entry.path())
        .collect::<Vec<_>>();
    assert!(
        staged.is_empty(),
        "every directory the session created must be gone: {staged:?}"
    );
    assert!(
        fixture.state.join("sessions").is_dir(),
        "the shared parent is not the session's to remove"
    );
    assert!(
        fixture.journals().is_empty(),
        "a completed transaction leaves no journal: {:?}",
        fixture.journals()
    );
    assert!(
        fixture.project_tree().is_empty(),
        "staging never touches the project: {:?}",
        fixture.project_tree()
    );
    for name in ["alpha", "beta"] {
        assert!(
            exists(&fixture.sources.join(name).join("SKILL.md")),
            "removing a staged link must never reach the source it pointed at"
        );
    }
}

/// Waits up to two seconds for `condition`, so a spawned session is observably underway.
fn wait_for(condition: impl Fn() -> bool) {
    let deadline = Instant::now() + Duration::from_secs(2);
    while Instant::now() < deadline {
        if condition() {
            return;
        }
        std::thread::sleep(Duration::from_millis(10));
    }
    panic!("the spawned session never reached an observable state");
}
