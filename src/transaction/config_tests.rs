//! Native config-file write-ahead and recovery tests without Agent discovery fixtures.

use super::Transaction;
use crate::domain::AgentId;
use crate::journal::{
    ActionOperation, ActionStatus, JournalAction, JournalLock, RecordedKind, TransactionId,
    TransactionJournal, TransactionStatus, staged_sibling, store,
};
use crate::link::{PlacementOutcome, platform_backend};
use crate::lock::acquire::{HeldLocks, LockOwner, LockPolicy};
use crate::lock::{LockAccess, LockResource, LockResourceKind};
use crate::mount::PathPrecondition;
use crate::state::testing::StateRootGuard;
use crate::test_support::TestDir;
use std::fs;

const CONTENTS: &str = "skills:\n  customDirectories: []\n";

/// Keeps actual locks and redirected state alive throughout one filesystem scenario.
fn with_config_transaction(test: impl FnOnce(Transaction, &HeldLocks)) {
    let fixture = TestDir::new("config-transaction");
    let _state = StateRootGuard::set(&fixture.path().join("state"));
    let root = fs::canonicalize(fixture.path()).unwrap();
    let id = TransactionId::mint();
    let final_path = root.join("omp-config.yml");
    let temporary = staged_sibling(&id, 1, &final_path);
    let resources = vec![
        LockResource::describe_shared(LockResourceKind::DiscoveryEntry, LockAccess::Mutate, &root)
            .unwrap(),
        LockResource::describe_shared(LockResourceKind::BackingStore, LockAccess::Mutate, &root)
            .unwrap(),
        LockResource::describe_shared(
            LockResourceKind::DiscoveryEntry,
            LockAccess::Mutate,
            &temporary,
        )
        .unwrap(),
    ];
    let locks = HeldLocks::acquire(
        &resources,
        LockPolicy::immediate(),
        &LockOwner::preliminary(),
    )
    .unwrap();
    let journal = TransactionJournal {
        transaction_id: id,
        agent: AgentId::Omp,
        owner_pid: std::process::id(),
        status: TransactionStatus::Planned,
        project_root: root.clone(),
        launch_cwd: root.clone(),
        discovery_entry: root.clone(),
        backing_store: root,
        keep_mounts: false,
        sources: vec![],
        locks: resources.iter().map(JournalLock::from).collect(),
        actions: vec![JournalAction {
            id: 1,
            operation: ActionOperation::CreateConfigFile,
            expected_precondition: PathPrecondition::Missing,
            temporary_path: Some(temporary),
            final_path,
            source_canonical: None,
            link_target: None,
            config_contents: Some(CONTENTS.to_owned()),
            kind: RecordedKind::File,
            status: ActionStatus::Planned,
            identity: None,
        }],
        errors: vec![],
    };
    let path = store::journal_path(&journal.transaction_id).unwrap();
    store::persist(&journal, &path).unwrap();
    test(Transaction::adopt(journal, path, &locks).unwrap(), &locks);
}

#[test]
fn config_transaction_apply_and_cleanup_remove_only_owned_bytes() {
    with_config_transaction(|mut transaction, _| {
        transaction.apply().unwrap();
        transaction.verify_config_files().unwrap();
        let path = transaction.journal.actions[0].final_path.clone();
        assert_eq!(fs::read_to_string(&path).unwrap(), CONTENTS);
        assert!(transaction.journal.actions[0].identity.is_some());
        assert!(!transaction.cleanup().unwrap().needs_attention());
        assert!(!path.exists());
        assert!(!transaction.journal_path().exists());
    });
}

#[test]
fn config_transaction_changed_bytes_reject_spawn_and_retain_journal() {
    with_config_transaction(|mut transaction, _| {
        transaction.apply().unwrap();
        let path = transaction.journal.actions[0].final_path.clone();
        fs::write(&path, "user changes").unwrap();
        assert!(transaction.verify_config_files().is_err());
        assert!(transaction.cleanup().unwrap().needs_attention());
        assert!(transaction.journal_path().is_file());
        assert_eq!(fs::read_to_string(path).unwrap(), "user changes");
    });
}

#[test]
fn config_transaction_recovers_staged_identity_before_and_after_placement() {
    for placed in [false, true] {
        with_config_transaction(|mut transaction, locks| {
            let action = &mut transaction.journal.actions[0];
            let staged = platform_backend()
                .create_config_file(action.temporary_path.as_ref().unwrap(), CONTENTS)
                .unwrap();
            action.status = ActionStatus::Staged;
            action.identity.clone_from(&staged.identity);
            if placed {
                assert!(matches!(
                    platform_backend()
                        .place_config_file_no_replace(&staged, &action.final_path)
                        .unwrap(),
                    PlacementOutcome::Placed(_)
                ));
            }
            transaction.journal.status = TransactionStatus::Applying;
            transaction.persist().unwrap();
            let path = transaction.path.clone();
            let journal = store::load(&path).unwrap();
            let final_path = journal.actions[0].final_path.clone();
            let mut recovered = Transaction::adopt(journal, path, locks).unwrap();
            assert!(!recovered.cleanup().unwrap().needs_attention());
            assert!(!staged.path.exists());
            assert!(!final_path.exists());
        });
    }
}

#[test]
fn config_transaction_intent_without_identity_retains_ambiguous_file() {
    with_config_transaction(|mut transaction, _| {
        let action = &mut transaction.journal.actions[0];
        let staged = platform_backend()
            .create_config_file(action.temporary_path.as_ref().unwrap(), CONTENTS)
            .unwrap();
        action.status = ActionStatus::Intent;
        transaction.journal.status = TransactionStatus::Applying;
        transaction.persist().unwrap();
        assert!(transaction.cleanup().unwrap().needs_attention());
        assert_eq!(fs::read_to_string(staged.path).unwrap(), CONTENTS);
        assert!(transaction.journal_path().exists());
    });
}
