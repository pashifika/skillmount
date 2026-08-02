//! Lock identity, key derivation, and advisory acquisition tests.

use std::path::Path;

use super::acquire::{HeldLocks, LockOwner, LockPolicy};
use super::{LockResource, LockResourceKind, key};
use crate::mount::resolve::classify;
use crate::state::testing::StateRootGuard;
use crate::test_support::{TestDir, symlink_dir_or_skip};

#[test]
fn a_logical_key_survives_the_resource_being_created() {
    let fixture = TestDir::new("lock-stable-key");
    let anchor = std::fs::canonicalize(fixture.path()).unwrap();
    let store = anchor.join(".codex/skills");

    let before = LockResource::describe(LockResourceKind::BackingStore, &anchor, &store)
        .expect("the store is beneath its anchor");
    std::fs::create_dir_all(&store).expect("store fixture");
    let after = LockResource::describe(LockResourceKind::BackingStore, &anchor, &store)
        .expect("the store is beneath its anchor");

    assert_eq!(
        before.identity.logical_path(),
        after.identity.logical_path(),
        "a first creator and a later observer must contend on one key"
    );
    assert_eq!(before.identity.anchor, after.identity.anchor);
    assert!(
        before.identity.physical.is_none(),
        "a resource that does not exist has no physical identity"
    );
    assert!(
        after.identity.physical.is_some(),
        "an existing resource adds a physical identity"
    );
    assert_eq!(
        before.lock_keys()[0],
        after.lock_keys()[0],
        "the hashed key, not only the path it came from, must be identical"
    );
    assert_eq!(before.lock_keys().len(), 1);
    assert_eq!(
        after.lock_keys().len(),
        2,
        "an existing resource adds its physical key on top of the logical one"
    );
}

#[test]
fn the_anchor_is_never_recomputed_from_directories_the_plan_creates() {
    let fixture = TestDir::new("lock-anchor-fixed");
    let anchor = std::fs::canonicalize(fixture.path()).unwrap();
    let store = anchor.join(".codex/skills");

    let before = LockResource::describe(LockResourceKind::BackingStore, &anchor, &store)
        .expect("beneath anchor");
    std::fs::create_dir_all(&store).expect("store fixture");
    let unanchored = LockResource::describe_unanchored(LockResourceKind::BackingStore, &store);

    assert_eq!(before.identity.suffix, Path::new(".codex/skills"));
    assert_eq!(
        unanchored.identity.suffix,
        Path::new(""),
        "an unanchored key moves its anchor down once the path exists, which is exactly why \
         a shared resource must pass an explicit anchor"
    );
}

#[test]
fn two_routes_to_one_store_report_the_same_physical_identity() {
    let fixture = TestDir::new("lock-aliases");
    // Paths are built from the canonical root because a temporary directory can sit behind a
    // symbolic link, as `/tmp` does on macOS, which would put the fixture outside its anchor.
    let anchor = std::fs::canonicalize(fixture.path()).unwrap();
    let store = anchor.join("store");
    std::fs::create_dir_all(&store).expect("store fixture");
    let alias = anchor.join("alias");
    if !symlink_dir_or_skip(&store, &alias) {
        return;
    }

    let direct = LockResource::describe_entry(
        LockResourceKind::BackingStore,
        &anchor,
        &classify(&store).unwrap(),
    )
    .expect("beneath anchor");
    let through_link = LockResource::describe_entry(
        LockResourceKind::BackingStore,
        &anchor,
        &classify(&alias).unwrap(),
    )
    .expect("beneath anchor");

    assert_ne!(
        direct.identity.logical_path(),
        through_link.identity.logical_path()
    );
    assert_eq!(
        direct.identity.physical, through_link.identity.physical,
        "aliases of one store must serialize against each other"
    );
    assert_eq!(
        direct.lock_keys()[1],
        through_link.lock_keys()[1],
        "the shared physical key is what actually serializes them"
    );
}

#[test]
fn a_worktree_reaching_a_shared_store_shares_only_the_physical_key() {
    let fixture = TestDir::new("lock-worktrees");
    let root = std::fs::canonicalize(fixture.path()).unwrap();
    let store = root.join("shared/skills");
    std::fs::create_dir_all(&store).expect("shared store fixture");

    // Two project roots, each reaching the one store through its own `.codex/skills` link.
    let mut resources = Vec::new();
    for worktree in ["work-a", "work-b"] {
        let project = root.join(worktree);
        std::fs::create_dir_all(project.join(".codex")).expect("worktree fixture");
        let entry = project.join(".codex/skills");
        if !symlink_dir_or_skip(&store, &entry) {
            return;
        }
        resources.push(
            LockResource::describe_entry(
                LockResourceKind::BackingStore,
                &project,
                &classify(&entry).unwrap(),
            )
            .expect("beneath its own project root"),
        );
    }

    let [first, second] = <[LockResource; 2]>::try_from(resources).expect("two worktrees");
    assert_ne!(
        first.lock_keys()[0],
        second.lock_keys()[0],
        "different worktrees address different logical paths"
    );
    assert_eq!(
        first.lock_keys()[1],
        second.lock_keys()[1],
        "distinct logical paths that resolve to one store must still serialize"
    );
}

#[test]
fn key_derivation_cannot_be_confused_by_moving_a_separator() {
    let fixture = TestDir::new("lock-key-ambiguity");
    let root = std::fs::canonicalize(fixture.path()).unwrap();
    let split_low = LockResource::describe(
        LockResourceKind::BackingStore,
        &root,
        &root.join("ab").join("c"),
    )
    .unwrap();
    let split_high = LockResource::describe(
        LockResourceKind::BackingStore,
        &root.join("ab"),
        &root.join("ab").join("c"),
    );

    // The second describe is against a different anchor, which is the whole point: the same final
    // path split at a different place must not hash to the same key.
    assert_ne!(
        split_low.lock_keys()[0],
        split_high.unwrap().lock_keys()[0],
        "length-prefixed hashing must keep the anchor/suffix split significant"
    );
}

#[test]
fn the_two_resource_kinds_never_share_a_logical_key() {
    let fixture = TestDir::new("lock-kind-tag");
    let root = std::fs::canonicalize(fixture.path()).unwrap();
    let path = root.join("skills");

    let entry = LockResource::describe(LockResourceKind::DiscoveryEntry, &root, &path).unwrap();
    let store = LockResource::describe(LockResourceKind::BackingStore, &root, &path).unwrap();

    assert_ne!(entry.lock_keys()[0], store.lock_keys()[0]);
}

#[test]
fn one_existing_directory_reached_as_two_kinds_shares_a_physical_key() {
    let fixture = TestDir::new("lock-kindless-physical");
    let root = std::fs::canonicalize(fixture.path()).unwrap();
    let path = root.join("skills");
    std::fs::create_dir_all(&path).expect("store fixture");

    let entry = LockResource::describe(LockResourceKind::DiscoveryEntry, &root, &path).unwrap();
    let store = LockResource::describe(LockResourceKind::BackingStore, &root, &path).unwrap();

    assert_eq!(
        entry.lock_keys()[1],
        store.lock_keys()[1],
        "the physical key omits the kind on purpose: it is the same directory either way"
    );
}

#[test]
fn a_resource_outside_its_anchor_is_an_internal_error() {
    let fixture = TestDir::new("lock-outside-anchor");
    let anchor = std::fs::canonicalize(fixture.path()).unwrap();

    let error = LockResource::describe(
        LockResourceKind::BackingStore,
        &anchor,
        Path::new("/elsewhere/skills"),
    )
    .expect_err("a resource outside the anchor is a planning invariant violation");

    assert_eq!(error.category(), crate::error::ExitCategory::Internal);
}

#[test]
fn a_lock_filename_is_a_bounded_portable_digest() {
    let fixture = TestDir::new("lock-filename");
    let root = std::fs::canonicalize(fixture.path()).unwrap();
    let resource =
        LockResource::describe(LockResourceKind::BackingStore, &root, &root.join("a/b/c")).unwrap();

    let name = resource.lock_keys()[0].file_name();

    assert_eq!(name.len(), 64 + ".lock".len());
    assert!(
        name.bytes().all(|byte| byte.is_ascii_hexdigit()
            || byte == b'.'
            || byte == b'l'
            || byte == b'o'
            || byte == b'c'
            || byte == b'k'),
        "a lock filename must be legal on both platforms: {name}"
    );
}

/// Builds two resources that produce four distinct keys, discovered in opposite orders.
fn opposing_resources(root: &Path) -> (Vec<LockResource>, Vec<LockResource>) {
    let first = LockResource::describe(LockResourceKind::BackingStore, root, &root.join("one"))
        .expect("beneath root");
    let second = LockResource::describe(LockResourceKind::DiscoveryEntry, root, &root.join("two"))
        .expect("beneath root");
    (vec![first.clone(), second.clone()], vec![second, first])
}

#[test]
fn sessions_that_discover_resources_in_opposite_orders_lock_in_one_order() {
    let fixture = TestDir::new("lock-order");
    let root = std::fs::canonicalize(fixture.path()).unwrap();
    let (forward, reverse) = opposing_resources(&root);

    let forward_keys = super::acquire::sorted_keys_for_test(&forward)
        .into_iter()
        .map(|(key, _)| key)
        .collect::<Vec<_>>();
    let reverse_keys = super::acquire::sorted_keys_for_test(&reverse)
        .into_iter()
        .map(|(key, _)| key)
        .collect::<Vec<_>>();

    assert_eq!(
        forward_keys, reverse_keys,
        "a shared acquisition order is what prevents a lock-order deadlock"
    );
    assert!(forward_keys.windows(2).all(|pair| pair[0] < pair[1]));
}

#[test]
fn one_resource_reached_twice_takes_a_single_lock() {
    let fixture = TestDir::new("lock-dedupe");
    let _guard = StateRootGuard::set(fixture.path());
    let root = std::fs::canonicalize(fixture.path()).unwrap();
    let store = root.join("skills");
    std::fs::create_dir_all(&store).expect("store fixture");
    // The Codex layout: the discovery entry and the backing store are one directory, so their
    // physical keys coincide. Requesting both must not deadlock against itself.
    let resources = vec![
        LockResource::describe(LockResourceKind::DiscoveryEntry, &root, &store).unwrap(),
        LockResource::describe(LockResourceKind::BackingStore, &root, &store).unwrap(),
    ];

    let held = HeldLocks::acquire(
        &resources,
        LockPolicy::immediate(),
        &LockOwner::preliminary(),
    )
    .expect("a self-overlapping set must still be acquirable");

    assert_eq!(
        held.keys().count(),
        3,
        "two distinct logical keys plus the one physical key they share"
    );
    assert!(held.holds_all(&resources));
}

#[test]
fn a_first_creator_and_a_later_observer_cannot_apply_concurrently() {
    let fixture = TestDir::new("lock-first-creator");
    let _guard = StateRootGuard::set(fixture.path());
    let root = std::fs::canonicalize(fixture.path()).unwrap();
    let store = root.join(".codex/skills");

    // One session plans the store before it exists and takes its lock.
    let planned = LockResource::describe(LockResourceKind::BackingStore, &root, &store).unwrap();
    let held = HeldLocks::acquire(
        std::slice::from_ref(&planned),
        LockPolicy::immediate(),
        &LockOwner::preliminary(),
    )
    .expect("an uncontended lock is available");

    // The store appears, and a second session observes it.
    std::fs::create_dir_all(&store).expect("store fixture");
    let observed = LockResource::describe(LockResourceKind::BackingStore, &root, &store).unwrap();

    assert!(
        held.holds(&observed.lock_keys()[0]),
        "the observer's logical key is the one the creator already holds"
    );
    let contention =
        HeldLocks::try_acquire_all(std::slice::from_ref(&observed), &LockOwner::preliminary())
            .unwrap();
    assert!(
        contention.is_err(),
        "the second session must not be able to take the same key"
    );
    let contention = contention.expect_err("checked above");
    assert_eq!(contention.key, observed.lock_keys()[0]);
    assert!(contention.describe().contains(&store.display().to_string()));
}

#[test]
fn a_lock_that_is_dropped_becomes_available_again() {
    let fixture = TestDir::new("lock-release");
    let _guard = StateRootGuard::set(fixture.path());
    let root = std::fs::canonicalize(fixture.path()).unwrap();
    let resource =
        LockResource::describe(LockResourceKind::BackingStore, &root, &root.join("s")).unwrap();

    let held = HeldLocks::acquire(
        std::slice::from_ref(&resource),
        LockPolicy::immediate(),
        &LockOwner::preliminary(),
    )
    .expect("uncontended");
    let lock_file = crate::state::lock_base()
        .unwrap()
        .join(resource.lock_keys()[0].file_name());
    assert!(lock_file.exists());
    drop(held);

    assert!(
        lock_file.exists(),
        "the file outliving the lock is exactly why its existence must not be read as liveness"
    );
    assert!(
        HeldLocks::try_acquire_all(&[resource], &LockOwner::preliminary())
            .unwrap()
            .is_ok(),
        "a released lock must be immediately available even though its file remains"
    );
}

#[test]
fn a_partial_acquisition_releases_what_it_already_took() {
    let fixture = TestDir::new("lock-partial");
    let _guard = StateRootGuard::set(fixture.path());
    let root = std::fs::canonicalize(fixture.path()).unwrap();
    let free =
        LockResource::describe(LockResourceKind::BackingStore, &root, &root.join("free")).unwrap();
    let busy =
        LockResource::describe(LockResourceKind::BackingStore, &root, &root.join("busy")).unwrap();
    let _blocker = HeldLocks::acquire(
        std::slice::from_ref(&busy),
        LockPolicy::immediate(),
        &LockOwner::preliminary(),
    )
    .expect("uncontended");

    let outcome = HeldLocks::try_acquire_all(&[free.clone(), busy], &LockOwner::preliminary())
        .expect("lock files are creatable");

    assert!(outcome.is_err());
    assert!(
        HeldLocks::try_acquire_all(&[free], &LockOwner::preliminary())
            .unwrap()
            .is_ok(),
        "the free lock must not stay held by the attempt that failed on the other one"
    );
}

#[test]
fn an_expired_wait_names_the_contended_resource_as_a_temporary_failure() {
    let fixture = TestDir::new("lock-wait");
    let _guard = StateRootGuard::set(fixture.path());
    let root = std::fs::canonicalize(fixture.path()).unwrap();
    let resource = LockResource::describe(
        LockResourceKind::BackingStore,
        &root,
        &root.join(".codex/skills"),
    )
    .unwrap();
    let _blocker = HeldLocks::acquire(
        std::slice::from_ref(&resource),
        LockPolicy::immediate(),
        &LockOwner::preliminary(),
    )
    .expect("uncontended");

    let policy = LockPolicy {
        wait: std::time::Duration::from_millis(30),
        poll: std::time::Duration::from_millis(5),
    };
    let error = HeldLocks::acquire(&[resource], policy, &LockOwner::preliminary())
        .expect_err("a lock held past the wait policy must fail");

    assert_eq!(error.category(), crate::error::ExitCategory::Temporary);
    let message = error.to_string();
    assert!(message.contains(".codex"), "{message}");
    assert!(message.contains("nothing was changed"), "{message}");
}

#[test]
fn holder_diagnostics_are_recorded_without_becoming_liveness_evidence() {
    let fixture = TestDir::new("lock-holder");
    let _guard = StateRootGuard::set(fixture.path());
    let root = std::fs::canonicalize(fixture.path()).unwrap();
    let resource =
        LockResource::describe(LockResourceKind::BackingStore, &root, &root.join("s")).unwrap();
    let owner = LockOwner {
        transaction: "abc123".to_owned(),
        pid: 4242,
    };

    let held = HeldLocks::acquire(
        std::slice::from_ref(&resource),
        LockPolicy::immediate(),
        &owner,
    )
    .expect("uncontended");
    let contents = std::fs::read_to_string(
        crate::state::lock_base()
            .unwrap()
            .join(resource.lock_keys()[0].file_name()),
    )
    .expect("the holder description is readable");
    drop(held);

    assert!(contents.contains("transaction=abc123"));
    assert!(contents.contains("pid=4242"));
    assert!(contents.contains("not evidence"));
}

#[test]
fn a_stale_holder_record_never_authorizes_taking_the_lock_or_blocks_it() {
    let fixture = TestDir::new("lock-pid-reuse");
    let _guard = StateRootGuard::set(fixture.path());
    let root = std::fs::canonicalize(fixture.path()).unwrap();
    let resource =
        LockResource::describe(LockResourceKind::BackingStore, &root, &root.join("s")).unwrap();
    let directory = crate::state::lock_base().unwrap();
    crate::state::ensure_private_directory(&directory).unwrap();
    // A crashed session's file, naming a pid the operating system has since handed to something
    // else. Nothing is holding the lock.
    std::fs::write(
        directory.join(resource.lock_keys()[0].file_name()),
        format!("transaction=dead pid={}\n", std::process::id()),
    )
    .expect("fixture write");

    let taken =
        HeldLocks::try_acquire_all(std::slice::from_ref(&resource), &LockOwner::preliminary())
            .unwrap();

    assert!(
        taken.is_ok(),
        "a leftover file must not block a session, whatever pid it names"
    );
    let held = taken.expect("checked above");
    assert!(
        HeldLocks::try_acquire_all(&[resource], &LockOwner::preliminary())
            .unwrap()
            .is_err(),
        "and once the lock is really held, the same file must not authorize a second holder"
    );
    drop(held);
}

#[test]
fn resources_without_a_shared_key_do_not_serialize() {
    let fixture = TestDir::new("lock-independent");
    let _guard = StateRootGuard::set(fixture.path());
    let root = std::fs::canonicalize(fixture.path()).unwrap();
    let first = LockResource::describe_unanchored(
        LockResourceKind::BackingStore,
        &root.join("session-a/root/.claude/skills"),
    );
    let second = LockResource::describe_unanchored(
        LockResourceKind::BackingStore,
        &root.join("session-b/root/.claude/skills"),
    );

    let _held = HeldLocks::acquire(&[first], LockPolicy::immediate(), &LockOwner::preliminary())
        .expect("uncontended");

    assert!(
        HeldLocks::try_acquire_all(&[second], &LockOwner::preliminary())
            .unwrap()
            .is_ok(),
        "two isolated staging roots must run concurrently"
    );
}

#[test]
fn a_physical_key_is_stable_for_one_identity() {
    let identity = crate::link::PlatformIdentity::from_recorded("unix:1:00000000000000ff");

    assert_eq!(key::physical(&identity), key::physical(&identity));
    assert_ne!(
        key::physical(&identity),
        key::physical(&crate::link::PlatformIdentity::from_recorded(
            "unix:1:00000000000000fe"
        ))
    );
}

#[test]
fn journal_locks_round_trip_into_the_same_keys() {
    let fixture = TestDir::new("lock-journal-roundtrip");
    let root = std::fs::canonicalize(fixture.path()).unwrap();
    let store = root.join("skills");
    std::fs::create_dir_all(&store).expect("store fixture");
    let resource = LockResource::describe(LockResourceKind::BackingStore, &root, &store).unwrap();

    let recorded = crate::journal::JournalLock::from(&resource);
    let rebuilt: LockResource = recorded.to_resource();

    assert_eq!(rebuilt.lock_keys(), resource.lock_keys());
    assert_eq!(rebuilt, resource);
}
