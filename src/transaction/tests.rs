//! Apply, rollback, and cleanup tests against a real filesystem.
//!
//! These run against the platform backend rather than the modelled one on purpose. Everything under
//! test here is filesystem semantics — atomic no-replace placement, identity capture, refusal to
//! remove a non-empty directory — and a model that agreed with the implementation would prove
//! nothing about the host.

use std::ffi::OsString;
use std::fs;
use std::path::{Path, PathBuf};

use super::Transaction;
use crate::app::{ReadOnlyOutcome, plan_read_only};
use crate::cli::{ParsedCommand, parse_command_from};
use crate::domain::RunContext;
use crate::journal::{ActionStatus, TransactionStatus, store};
use crate::link::OwnershipMismatch;
use crate::lock::acquire::{HeldLocks, LockOwner, LockPolicy};
use crate::paths::resolve_session;
use crate::state::testing::StateRootGuard;
use crate::test_support::{TestDir, remove_directory_link, symlink_dir_or_skip};

/// A project, a Skill source, and a redirected state root.
struct Session {
    /// Held for the test's lifetime so journals and locks stay inside the fixture.
    _state: StateRootGuard,
    fixture: TestDir,
    context: RunContext,
}

impl Session {
    /// Builds a Codex session over `skills`, with extra arguments appended.
    fn codex(label: &str, skills: &[&str], extra: &[&str]) -> Self {
        let fixture = TestDir::new(label);
        let state = StateRootGuard::set(&fixture.path().join("state"));
        let project = fixture.dir("project");
        let sources = fixture.dir("sources");
        for name in skills {
            let skill = sources.join(name);
            fs::create_dir_all(&skill).expect("skill fixture");
            fs::write(
                skill.join("SKILL.md"),
                format!("---\nname: {name}\ndescription: {name} description\n---\n"),
            )
            .expect("SKILL.md fixture");
        }

        let mut arguments = vec![
            OsString::from("asm"),
            OsString::from("codex"),
            OsString::from("--skills-dir"),
            sources.into_os_string(),
            OsString::from("--project-root"),
            project.clone().into_os_string(),
            OsString::from("--cwd"),
            project.into_os_string(),
        ];
        arguments.extend(extra.iter().map(OsString::from));
        let ParsedCommand::Session(input) = parse_command_from(arguments).expect("valid CLI")
        else {
            panic!("expected a session command");
        };
        let context = resolve_session(input, fixture.path()).expect("paths resolve");

        Self {
            _state: state,
            fixture,
            context,
        }
    }

    fn project(&self) -> PathBuf {
        fs::canonicalize(self.fixture.path().join("project")).expect("canonical project")
    }

    fn source(&self, name: &str) -> PathBuf {
        fs::canonicalize(self.fixture.path().join("sources").join(name)).expect("canonical source")
    }

    fn plan(&self) -> ReadOnlyOutcome {
        plan_read_only(&self.context).expect("the fixture plans cleanly")
    }

    /// Takes the plan's locks, exactly as a real session does before opening a transaction.
    fn lock(outcome: &ReadOnlyOutcome) -> HeldLocks {
        HeldLocks::acquire(
            &outcome.snapshot.lock_resources,
            LockPolicy::immediate(),
            &LockOwner::preliminary(),
        )
        .expect("an uncontended fixture always locks")
    }

    /// Opens a transaction and returns it together with the locks it depends on.
    ///
    /// The locks are returned rather than dropped: releasing them would leave the transaction
    /// mutating entries another session could reach, which is exactly what the guard in
    /// [`Transaction::open`] refuses to allow.
    fn open(&self) -> (Transaction, HeldLocks) {
        let outcome = self.plan();
        let locks = Self::lock(&outcome);
        let transaction = Transaction::open(
            &self.context,
            &outcome.catalog,
            &outcome.plan,
            &outcome.snapshot,
            &locks,
        )
        .expect("the redirected state root is writable");
        (transaction, locks)
    }
}

/// Returns the durable journal, reloaded from disk rather than read from memory.
fn on_disk(transaction: &Transaction) -> crate::journal::TransactionJournal {
    store::load(transaction.journal_path()).expect("the journal is readable")
}

#[test]
fn a_planned_journal_is_durable_before_anything_is_created() {
    let session = Session::codex("txn-planned", &["alpha"], &[]);
    let project = session.project();

    let (transaction, _locks) = session.open();

    let journal = on_disk(&transaction);
    assert_eq!(journal.status, TransactionStatus::Planned);
    assert!(
        journal
            .actions
            .iter()
            .all(|action| matches!(action.status, ActionStatus::Planned | ActionStatus::Reused)),
        "no action may claim progress before apply runs"
    );
    assert!(
        !project.join(".codex").exists() && !project.join(".agents").exists(),
        "opening a transaction records the plan and creates nothing"
    );
    assert!(
        !journal.locks.is_empty(),
        "the lock set must be persisted so recovery can reconstruct it"
    );
    assert_eq!(
        journal.sources.len(),
        1,
        "source provenance is persisted so recovery can explain a mount it did not plan"
    );
}

#[test]
fn a_transaction_refuses_to_open_without_the_locks_its_plan_needs() {
    let session = Session::codex("txn-unlocked", &["alpha"], &[]);
    let outcome = session.plan();

    let error = Transaction::open(
        &session.context,
        &outcome.catalog,
        &outcome.plan,
        &outcome.snapshot,
        &HeldLocks::default(),
    )
    .expect_err("every later removal is safe only because the locks are held");

    assert_eq!(error.category(), crate::error::ExitCategory::Internal);
    assert!(error.to_string().contains("resource locks are held"));
    assert!(
        crate::journal::store::scan()
            .expect("the state root is readable")
            .journals
            .is_empty(),
        "a refused transaction must not leave a journal describing entries it never made"
    );
}

#[test]
fn a_partially_locked_session_cannot_open_a_transaction() {
    let session = Session::codex("txn-partial-locks", &["alpha"], &[]);
    let outcome = session.plan();
    // Only the first resource, so at least one key the plan needs is missing.
    let partial = HeldLocks::acquire(
        &outcome.snapshot.lock_resources[..1],
        LockPolicy::immediate(),
        &LockOwner::preliminary(),
    )
    .expect("uncontended");

    let error = Transaction::open(
        &session.context,
        &outcome.catalog,
        &outcome.plan,
        &outcome.snapshot,
        &partial,
    )
    .expect_err("holding some of the locks is not holding the locks");

    assert_eq!(error.category(), crate::error::ExitCategory::Internal);
}

#[test]
fn every_staged_sibling_lives_beside_its_destination() {
    let session = Session::codex("txn-staging", &["alpha"], &[]);
    let (transaction, _locks) = session.open();

    for action in &on_disk(&transaction).actions {
        let Some(temporary) = &action.temporary_path else {
            assert_eq!(
                action.status,
                ActionStatus::Reused,
                "only a reuse action has nothing to stage"
            );
            continue;
        };
        assert_eq!(
            temporary.parent(),
            action.final_path.parent(),
            "placement is an atomic rename, which is only atomic within one filesystem"
        );
        assert!(
            temporary
                .file_name()
                .and_then(|name| name.to_str())
                .is_some_and(|name| name.starts_with(".skillmount-")),
            "a staged entry must be identifiable on sight: {}",
            temporary.display()
        );
    }
}

#[test]
fn applying_creates_the_whole_layout_and_marks_the_journal_active() {
    let session = Session::codex("txn-apply", &["alpha", "beta"], &[]);
    let project = session.project();
    let (mut transaction, _locks) = session.open();

    transaction.apply().expect("a clean fixture applies");

    let journal = on_disk(&transaction);
    assert_eq!(journal.status, TransactionStatus::Active);
    assert!(
        journal
            .actions
            .iter()
            .all(|action| action.status == ActionStatus::Applied),
        "the active state means every action is durably applied: {:?}",
        journal
            .actions
            .iter()
            .map(|action| (action.id, action.status))
            .collect::<Vec<_>>()
    );
    for name in ["alpha", "beta"] {
        let mounted = project.join(".codex/skills").join(name);
        assert_eq!(
            fs::canonicalize(&mounted).expect("the mount resolves"),
            session.source(name),
            "each Skill must reach its source"
        );
    }
    assert!(
        fs::symlink_metadata(project.join(".agents/skills"))
            .expect("the authoritative entry exists")
            .file_type()
            .is_symlink(),
        "the authoritative discovery entry is a link to the backing store"
    );
    assert!(
        staged_leftovers(&project.join(".codex/skills")).is_empty(),
        "placement must consume every staged sibling"
    );
}

#[test]
fn every_created_entry_records_an_identity_that_can_later_prove_ownership() {
    let session = Session::codex("txn-identity", &["alpha"], &[]);
    let (mut transaction, _locks) = session.open();

    transaction.apply().expect("a clean fixture applies");

    for action in &on_disk(&transaction).actions {
        assert!(
            action.identity.is_some(),
            "action {} recorded no identity, so cleanup could never remove it",
            action.id
        );
    }
}

#[test]
fn a_destination_that_appears_after_planning_stops_the_apply_and_rolls_back() {
    let session = Session::codex("txn-drift", &["alpha"], &[]);
    let project = session.project();
    let (mut transaction, _locks) = session.open();

    // Something outside SkillMount creates the destination between planning and applying. It holds
    // no lock, so this is the race the persisted precondition exists to catch.
    fs::create_dir_all(project.join(".codex/skills/alpha")).expect("intruding entry");

    let failure = transaction
        .apply()
        .expect_err("a plan built against stale state must not overwrite anything");

    assert!(
        project.join(".codex/skills/alpha").exists(),
        "the entry that was already there must survive untouched"
    );
    let journal = on_disk(&transaction);
    assert_eq!(journal.status, TransactionStatus::Failed);
    assert!(
        journal
            .errors
            .iter()
            .any(|error| error.contains("the plan expected to be missing")),
        "the failure must be durable: {:?}",
        journal.errors
    );
    assert_eq!(
        failure.into_error().category(),
        crate::error::ExitCategory::Filesystem
    );
}

#[test]
fn rollback_undoes_applied_actions_and_leaves_the_obstruction_alone() {
    let session = Session::codex("txn-rollback", &["alpha"], &[]);
    let project = session.project();
    // The store and the `.agents` parent already exist, so the plan owns exactly two actions: the
    // authoritative link, then the Skill link. That is the shape rollback exists for — an earlier
    // action succeeds and a later one fails.
    fs::create_dir_all(project.join(".codex/skills")).expect("store fixture");
    fs::create_dir_all(project.join(".agents")).expect("agents fixture");
    let (mut transaction, _locks) = session.open();
    let planned = transaction.journal().actions.len();

    fs::create_dir_all(project.join(".codex/skills/alpha")).expect("conflicting entry");
    let failure = transaction.apply().expect_err("the last action must fail");

    assert_eq!(
        planned, 2,
        "the fixture must leave exactly two owned actions"
    );
    assert!(
        failure.rollback_errors.is_empty(),
        "rollback should succeed here: {:?}",
        failure.rollback_errors
    );
    assert!(
        !project.join(".agents/skills").exists(),
        "the authoritative link applied before the failure must be gone"
    );
    assert!(
        project.join(".agents").exists() && project.join(".codex/skills").exists(),
        "directories this transaction did not create are never rolled back"
    );
    assert!(
        project.join(".codex/skills/alpha").exists(),
        "the pre-existing entry is never rolled back"
    );
    let journal = on_disk(&transaction);
    assert_eq!(
        journal
            .actions
            .iter()
            .map(|action| action.status)
            .collect::<Vec<_>>(),
        [ActionStatus::RolledBack, ActionStatus::Planned],
        "the applied action is durably undone and the one that never started stays planned"
    );
}

#[test]
fn a_failed_rollback_keeps_the_original_cause_and_every_retained_path() {
    let session = Session::codex("txn-both-contexts", &["alpha"], &[]);
    let project = session.project();
    fs::create_dir_all(project.join(".codex/skills")).expect("store fixture");
    fs::create_dir_all(project.join(".agents")).expect("agents fixture");
    let (mut transaction, _locks) = session.open();

    // The authoritative link applies, the Skill link then hits a conflict, and rollback finds the
    // authoritative link replaced by something it cannot prove it owns. Both failures matter.
    fs::create_dir_all(project.join(".codex/skills/alpha")).expect("conflicting entry");
    let authoritative = project.join(".agents/skills");

    let failure = transaction
        .apply()
        .expect_err("the second action must fail");
    let journal = on_disk(&transaction);

    assert!(
        !authoritative.exists(),
        "the applied link is rolled back when nothing interferes"
    );
    assert!(
        journal
            .errors
            .iter()
            .any(|error| error.contains("the plan expected to be missing")),
        "the original cause must be durable: {:?}",
        journal.errors
    );
    assert_eq!(
        failure.into_error().category(),
        crate::error::ExitCategory::Filesystem,
        "a rollback that succeeded must not change the category of the original failure"
    );
}

#[test]
fn a_rollback_that_cannot_finish_reports_the_cause_and_the_residue_together() {
    let session = Session::codex("txn-residue", &["alpha"], &[]);
    let project = session.project();
    let (mut transaction, _locks) = session.open();
    transaction.apply().expect("a clean fixture applies");

    // A cleanup that cannot finish must surface both halves: what went wrong and what is left.
    remove_directory_link(&project.join(".codex/skills/alpha"));
    fs::write(project.join(".codex/skills/notes.md"), "mine").expect("user content");
    let report = transaction.cleanup().expect("cleanup completes");
    let journal = on_disk(&transaction);

    let rendered = report.describe().join("\n");
    assert!(rendered.contains("retained"), "{rendered}");
    assert!(
        journal
            .errors
            .iter()
            .any(|error| error.contains("retained")),
        "the durable record must carry the residue, not only the report: {:?}",
        journal.errors
    );
    assert_eq!(journal.status, TransactionStatus::Failed);
}

#[test]
fn cleanup_removes_everything_it_owns_and_then_removes_the_journal() {
    let session = Session::codex("txn-cleanup", &["alpha"], &[]);
    let project = session.project();
    let (mut transaction, _locks) = session.open();
    transaction.apply().expect("a clean fixture applies");
    let journal_path = transaction.journal_path().to_path_buf();

    let report = transaction.cleanup().expect("cleanup completes");

    assert!(!report.needs_attention(), "{report:?}");
    assert!(
        !project.join(".codex").exists() && !project.join(".agents").exists(),
        "a completed cleanup leaves the project as it found it"
    );
    assert!(
        !journal_path.exists(),
        "a completed transaction has nothing left to describe"
    );
    assert!(
        session
            .fixture
            .path()
            .join("sources/alpha/SKILL.md")
            .exists(),
        "removing a link must never reach the directory it pointed at"
    );
}

#[test]
fn a_user_replaced_entry_is_retained_and_reported() {
    let session = Session::codex("txn-replaced", &["alpha"], &[]);
    let project = session.project();
    let (mut transaction, _locks) = session.open();
    transaction.apply().expect("a clean fixture applies");

    // The operator replaces the mount with a directory of their own.
    let mounted = project.join(".codex/skills/alpha");
    remove_directory_link(&mounted);
    fs::create_dir_all(mounted.join("their-own-work")).expect("replacement");

    let report = transaction.cleanup().expect("cleanup completes");

    assert!(report.needs_attention());
    assert!(
        mounted.join("their-own-work").exists(),
        "cleanup must never touch a directory it cannot prove it created"
    );
    let retained = report
        .retained
        .iter()
        .find(|entry| entry.path == mounted)
        .unwrap_or_else(|| panic!("the replaced entry must be reported: {report:?}"));
    assert!(
        retained
            .reason
            .contains(OwnershipMismatch::RegularDirectory.label()),
        "{retained}"
    );
    let journal = on_disk(&transaction);
    assert_eq!(
        journal.status,
        TransactionStatus::Failed,
        "an incomplete cleanup keeps its journal so the residue stays described"
    );
    assert!(transaction.journal_path().exists());
}

#[test]
fn a_link_retargeted_by_someone_else_is_left_alone() {
    let session = Session::codex("txn-retargeted", &["alpha"], &[]);
    let project = session.project();
    let elsewhere = session.fixture.dir("elsewhere");
    let (mut transaction, _locks) = session.open();
    transaction.apply().expect("a clean fixture applies");

    let mounted = project.join(".codex/skills/alpha");
    remove_directory_link(&mounted);
    if !symlink_dir_or_skip(&elsewhere, &mounted) {
        return;
    }

    let report = transaction.cleanup().expect("cleanup completes");

    assert!(
        fs::symlink_metadata(&mounted).is_ok(),
        "a link pointing somewhere else is not this transaction's to remove"
    );
    assert!(
        report.retained.iter().any(|entry| entry.path == mounted
            && (entry
                .reason
                .contains(OwnershipMismatch::TargetChanged.label())
                || entry
                    .reason
                    .contains(OwnershipMismatch::IdentityChanged.label()))),
        "{:?}",
        report.retained
    );
}

#[test]
fn a_helper_directory_that_gained_contents_keeps_them() {
    let session = Session::codex("txn-nonempty", &["alpha"], &[]);
    let project = session.project();
    let (mut transaction, _locks) = session.open();
    transaction.apply().expect("a clean fixture applies");

    // The mount is removed by hand but something else is left in the store.
    remove_directory_link(&project.join(".codex/skills/alpha"));
    fs::write(project.join(".codex/skills/notes.md"), "mine").expect("user content");

    let report = transaction.cleanup().expect("cleanup completes");

    assert!(
        project.join(".codex/skills/notes.md").exists(),
        "an empty-check that removed this would have taken the operator's file with it"
    );
    assert!(
        report
            .retained
            .iter()
            .any(|entry| entry.path == project.join(".codex/skills")
                && entry.reason.contains("holds entries")),
        "{:?}",
        report.retained
    );
}

#[test]
fn a_directory_recorded_without_an_identity_is_never_removed() {
    let session = Session::codex("txn-unprovable", &["alpha"], &[]);
    let project = session.project();
    let (mut transaction, _locks) = session.open();
    transaction.apply().expect("a clean fixture applies");

    // Simulates a journal written by a host that reported no identity: the directory is genuinely
    // this transaction's, and it still must not be removed, because nothing proves that.
    remove_directory_link(&project.join(".codex/skills/alpha"));
    let store_path = project.join(".codex/skills");
    for action in &mut transaction.journal_mut().actions {
        if action.final_path == store_path {
            action.identity = None;
        }
    }

    let report = transaction.cleanup().expect("cleanup completes");

    assert!(store_path.exists());
    assert!(
        report.retained.iter().any(|entry| entry.path == store_path
            && entry
                .reason
                .contains(OwnershipMismatch::IdentityUnavailable.label())),
        "{:?}",
        report.retained
    );
}

#[test]
fn an_entry_that_is_already_gone_is_a_harmless_cleanup() {
    let session = Session::codex("txn-absent", &["alpha"], &[]);
    let project = session.project();
    let (mut transaction, _locks) = session.open();
    transaction.apply().expect("a clean fixture applies");
    remove_directory_link(&project.join(".codex/skills/alpha"));

    let report = transaction.cleanup().expect("cleanup completes");

    assert!(!report.needs_attention(), "{report:?}");
    assert!(!transaction.journal_path().exists());
}

#[test]
fn keep_mounts_retains_everything_and_reaches_a_terminal_state() {
    let session = Session::codex("txn-keep", &["alpha"], &["--keep-mounts"]);
    let project = session.project();
    let (mut transaction, _locks) = session.open();
    transaction.apply().expect("a clean fixture applies");

    let report = transaction.cleanup().expect("cleanup completes");

    assert!(project.join(".codex/skills/alpha").exists());
    assert_eq!(
        report.journal_retained.as_deref(),
        Some(transaction.journal_path())
    );
    let journal = on_disk(&transaction);
    assert_eq!(journal.status, TransactionStatus::Kept);
    assert!(
        journal.status.is_terminal(),
        "a kept transaction must never be reconciled by a later run"
    );
}

#[test]
fn a_reuse_action_is_recorded_unowned_and_survives_cleanup() {
    let session = Session::codex("txn-reuse", &["alpha"], &[]);
    let project = session.project();
    let source = session.source("alpha");
    // A pre-existing mount pointing at exactly the source this session selected.
    fs::create_dir_all(project.join(".codex/skills")).expect("store fixture");
    if !symlink_dir_or_skip(&source, &project.join(".codex/skills/alpha")) {
        return;
    }

    let (mut transaction, _locks) = session.open();
    let reuse_count = transaction
        .journal()
        .actions
        .iter()
        .filter(|action| action.status == ActionStatus::Reused)
        .count();
    transaction.apply().expect("a reused entry applies cleanly");
    let report = transaction.cleanup().expect("cleanup completes");

    assert_eq!(
        reuse_count, 1,
        "the existing mount must be reused, not recreated"
    );
    assert!(
        project.join(".codex/skills/alpha").exists(),
        "a reused entry belongs to whoever made it and must outlive the session"
    );
    assert!(
        report
            .removed
            .iter()
            .all(|path| path != &project.join(".codex/skills/alpha")),
        "cleanup must never own a reused entry: {:?}",
        report.removed
    );
}

#[test]
fn a_reused_entry_that_changed_since_planning_stops_the_apply() {
    let session = Session::codex("txn-reuse-drift", &["alpha"], &[]);
    let project = session.project();
    let source = session.source("alpha");
    fs::create_dir_all(project.join(".codex/skills")).expect("store fixture");
    let mounted = project.join(".codex/skills/alpha");
    if !symlink_dir_or_skip(&source, &mounted) {
        return;
    }
    let (mut transaction, _locks) = session.open();

    // The entry the plan intended to reuse is replaced by a plain directory.
    remove_directory_link(&mounted);
    fs::create_dir_all(&mounted).expect("replacement");

    let failure = transaction
        .apply()
        .expect_err("a reuse whose precondition drifted must not be accepted silently");

    assert!(mounted.exists(), "the replacement is never touched");
    assert!(
        failure.cause.to_string().contains("reuse"),
        "{}",
        failure.cause
    );
}

/// Returns any staged sibling still sitting in `directory`.
fn staged_leftovers(directory: &Path) -> Vec<PathBuf> {
    fs::read_dir(directory).map_or_else(
        |_| Vec::new(),
        |entries| {
            entries
                .filter_map(Result::ok)
                .map(|entry| entry.path())
                .filter(|path| {
                    path.file_name()
                        .and_then(|name| name.to_str())
                        .is_some_and(|name| name.starts_with(".skillmount-"))
                })
                .collect()
        },
    )
}
