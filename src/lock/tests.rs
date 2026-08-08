//! Lock identity, key derivation, and advisory acquisition tests.

use std::path::Path;

use super::acquire::{
    AdvisoryLockState, HeldLocks, LockOwner, LockPolicy, MissingLockOutcome, observe,
};
use super::{LockAccess, LockResource, LockResourceKind, key};
use crate::mount::resolve::classify;
use crate::state::testing::StateRootGuard;
use crate::test_support::{TestDir, symlink_dir_or_skip};

#[test]
fn a_logical_key_survives_the_resource_being_created() {
    let fixture = TestDir::new("lock-stable-key");
    let anchor = std::fs::canonicalize(fixture.path()).unwrap();
    let store = anchor.join(".codex/skills");

    let before = LockResource::describe(
        LockResourceKind::BackingStore,
        crate::lock::LockAccess::Mutate,
        &anchor,
        &store,
    )
    .expect("the store is beneath its anchor");
    std::fs::create_dir_all(&store).expect("store fixture");
    let after = LockResource::describe(
        LockResourceKind::BackingStore,
        crate::lock::LockAccess::Mutate,
        &anchor,
        &store,
    )
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
fn a_shared_missing_root_keeps_its_logical_key_after_external_creation() {
    let fixture = TestDir::new("lock-shared-stable-key");
    let root = std::fs::canonicalize(fixture.path()).unwrap();
    let shared = root.join("home/.agents/skills");

    let observed = LockResource::describe_shared(
        LockResourceKind::DiscoveryEntry,
        LockAccess::Observe,
        &shared,
    )
    .expect("absolute shared root");
    std::fs::create_dir_all(&shared).expect("external shared-root creation");
    let mutated = LockResource::describe_shared(
        LockResourceKind::DiscoveryEntry,
        LockAccess::Mutate,
        &shared,
    )
    .expect("same absolute shared root");

    assert_eq!(observed.lock_keys()[0], mutated.lock_keys()[0]);
    assert!(observed.identity.physical.is_none());
    assert!(mutated.identity.physical.is_some());
}

#[test]
fn the_anchor_is_never_recomputed_from_directories_the_plan_creates() {
    let fixture = TestDir::new("lock-anchor-fixed");
    let anchor = std::fs::canonicalize(fixture.path()).unwrap();
    let store = anchor.join(".codex/skills");

    let before = LockResource::describe(
        LockResourceKind::BackingStore,
        crate::lock::LockAccess::Mutate,
        &anchor,
        &store,
    )
    .expect("beneath anchor");
    std::fs::create_dir_all(&store).expect("store fixture");
    let unanchored = LockResource::describe_unanchored(
        LockResourceKind::BackingStore,
        crate::lock::LockAccess::Mutate,
        &store,
    );

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
        crate::lock::LockAccess::Mutate,
        &anchor,
        &classify(&store).unwrap(),
    )
    .expect("beneath anchor");
    let through_link = LockResource::describe_entry(
        LockResourceKind::BackingStore,
        crate::lock::LockAccess::Mutate,
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
                crate::lock::LockAccess::Mutate,
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
        crate::lock::LockAccess::Mutate,
        &root,
        &root.join("ab").join("c"),
    )
    .unwrap();
    let split_high = LockResource::describe(
        LockResourceKind::BackingStore,
        crate::lock::LockAccess::Mutate,
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

    let entry = LockResource::describe(
        LockResourceKind::DiscoveryEntry,
        crate::lock::LockAccess::Mutate,
        &root,
        &path,
    )
    .unwrap();
    let store = LockResource::describe(
        LockResourceKind::BackingStore,
        crate::lock::LockAccess::Mutate,
        &root,
        &path,
    )
    .unwrap();

    assert_ne!(entry.lock_keys()[0], store.lock_keys()[0]);
}

#[test]
fn one_existing_directory_reached_across_access_and_resource_kinds_shares_a_physical_key() {
    let fixture = TestDir::new("lock-kindless-physical");
    let root = std::fs::canonicalize(fixture.path()).unwrap();
    let path = root.join("skills");
    std::fs::create_dir_all(&path).expect("store fixture");

    let entry = LockResource::describe(
        LockResourceKind::DiscoveryEntry,
        LockAccess::Observe,
        &root,
        &path,
    )
    .unwrap();
    let store = LockResource::describe(
        LockResourceKind::BackingStore,
        LockAccess::Mutate,
        &root,
        &path,
    )
    .unwrap();

    assert_eq!(
        entry.lock_keys()[1],
        store.lock_keys()[1],
        "physical identity crosses Agent resource kinds and observation/mutation intent"
    );
}

#[test]
fn access_mode_does_not_change_logical_or_physical_keys() {
    let fixture = TestDir::new("lock-access-stable-keys");
    let root = std::fs::canonicalize(fixture.path()).unwrap();
    let path = root.join("skills");
    std::fs::create_dir_all(&path).expect("store fixture");

    let observed = LockResource::describe(
        LockResourceKind::DiscoveryEntry,
        LockAccess::Observe,
        &root,
        &path,
    )
    .unwrap();
    let mutated = LockResource::describe(
        LockResourceKind::DiscoveryEntry,
        LockAccess::Mutate,
        &root,
        &path,
    )
    .unwrap();

    assert_eq!(observed.lock_keys(), mutated.lock_keys());
    assert_eq!(observed.identity, mutated.identity);
    assert_ne!(observed.access, mutated.access);
}

#[test]
fn duplicate_logical_requests_fold_to_the_strongest_access() {
    let fixture = TestDir::new("lock-access-fold-logical");
    let root = std::fs::canonicalize(fixture.path()).unwrap();
    let path = root.join("skills");
    let template = LockResource::describe(
        LockResourceKind::DiscoveryEntry,
        LockAccess::Observe,
        &root,
        &path,
    )
    .unwrap();
    let logical_key = template.lock_keys()[0].clone();

    for (left, right, expected) in [
        (
            LockAccess::Observe,
            LockAccess::Observe,
            LockAccess::Observe,
        ),
        (LockAccess::Observe, LockAccess::Mutate, LockAccess::Mutate),
        (LockAccess::Mutate, LockAccess::Mutate, LockAccess::Mutate),
    ] {
        let mut first = template.clone();
        first.access = left;
        let mut second = template.clone();
        second.access = right;
        let requests = super::acquire::sorted_keys_for_test(&[first, second]);
        let (_, access, resources) = requests
            .iter()
            .find(|(key, _, _)| key == &logical_key)
            .expect("logical request");

        assert_eq!(*access, expected, "{left:?} plus {right:?}");
        assert_eq!(resources, std::slice::from_ref(&path));
    }
}

#[test]
fn mixed_kind_physical_requests_retain_paths_and_fold_to_mutation() {
    let fixture = TestDir::new("lock-access-fold-physical");
    let root = std::fs::canonicalize(fixture.path()).unwrap();
    let store = root.join("store");
    std::fs::create_dir_all(&store).expect("store fixture");
    let alias = root.join("alias");
    if !symlink_dir_or_skip(&store, &alias) {
        return;
    }

    let observed = LockResource::describe_entry(
        LockResourceKind::DiscoveryEntry,
        LockAccess::Observe,
        &root,
        &classify(&alias).unwrap(),
    )
    .unwrap();
    let mutated = LockResource::describe_entry(
        LockResourceKind::BackingStore,
        LockAccess::Mutate,
        &root,
        &classify(&store).unwrap(),
    )
    .unwrap();
    let physical_key = observed.lock_keys()[1].clone();
    let requests = super::acquire::sorted_keys_for_test(&[observed, mutated]);
    let (_, access, resources) = requests
        .iter()
        .find(|(key, _, _)| key == &physical_key)
        .expect("shared physical request");

    assert_eq!(*access, LockAccess::Mutate);
    assert_eq!(resources.len(), 2);
    assert!(resources.contains(&alias));
    assert!(resources.contains(&store));
}

#[test]
fn a_resource_outside_its_anchor_is_an_internal_error() {
    let fixture = TestDir::new("lock-outside-anchor");
    let anchor = std::fs::canonicalize(fixture.path()).unwrap();

    let error = LockResource::describe(
        LockResourceKind::BackingStore,
        crate::lock::LockAccess::Mutate,
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
    let resource = LockResource::describe(
        LockResourceKind::BackingStore,
        crate::lock::LockAccess::Mutate,
        &root,
        &root.join("a/b/c"),
    )
    .unwrap();

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
    let first = LockResource::describe(
        LockResourceKind::BackingStore,
        crate::lock::LockAccess::Mutate,
        root,
        &root.join("one"),
    )
    .expect("beneath root");
    let second = LockResource::describe(
        LockResourceKind::DiscoveryEntry,
        crate::lock::LockAccess::Mutate,
        root,
        &root.join("two"),
    )
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
        .map(|(key, _, _)| key)
        .collect::<Vec<_>>();
    let reverse_keys = super::acquire::sorted_keys_for_test(&reverse)
        .into_iter()
        .map(|(key, _, _)| key)
        .collect::<Vec<_>>();

    assert_eq!(
        forward_keys, reverse_keys,
        "a shared acquisition order is what prevents a lock-order deadlock"
    );
    assert!(forward_keys.windows(2).all(|pair| pair[0] < pair[1]));
}

#[test]
fn two_phased_sessions_restart_instead_of_crossing_the_global_order() {
    let fixture = TestDir::new("lock-phased-order");
    let _guard = StateRootGuard::set(fixture.path());
    let root = std::fs::canonicalize(fixture.path()).unwrap();
    let (resources, _) = opposing_resources(&root);
    let first = resources[0].clone();
    let second = resources[1].clone();
    let (earlier, later) = if first.lock_keys()[0] < second.lock_keys()[0] {
        (first, second)
    } else {
        (second, first)
    };

    // Session A discovered only the later key. Session B discovered only the earlier one. If A
    // incrementally waited on the earlier key while B waited on the later key, the phases would
    // cross. A must instead retire its preliminary set.
    let mut session_a = HeldLocks::acquire(
        std::slice::from_ref(&later),
        LockPolicy::immediate(),
        &LockOwner::preliminary(),
    )
    .expect("the later phase starts uncontended");
    let mut session_b = HeldLocks::acquire(
        std::slice::from_ref(&earlier),
        LockPolicy::immediate(),
        &LockOwner::preliminary(),
    )
    .expect("the earlier phase starts uncontended");

    assert!(session_a.requires_reacquire(std::slice::from_ref(&earlier)));
    let inversion = session_a
        .acquire_more(
            std::slice::from_ref(&earlier),
            LockPolicy::immediate(),
            &LockOwner::preliminary(),
        )
        .expect_err("the lock layer must reject an accidental backwards acquisition");
    assert_eq!(inversion.category(), crate::error::ExitCategory::Internal);

    drop(session_a);
    session_b
        .acquire_more(
            std::slice::from_ref(&later),
            LockPolicy::immediate(),
            &LockOwner::preliminary(),
        )
        .expect("the session already holding the earlier key may finish in order");
    drop(session_b);

    let restarted = HeldLocks::acquire(
        &[later, earlier],
        LockPolicy::immediate(),
        &LockOwner::preliminary(),
    )
    .expect("the retired session reacquires the complete union in sorted order");
    assert_eq!(restarted.keys().count(), 2);
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
        LockResource::describe(
            LockResourceKind::DiscoveryEntry,
            LockAccess::Mutate,
            &root,
            &store,
        )
        .unwrap(),
        LockResource::describe(
            LockResourceKind::BackingStore,
            LockAccess::Mutate,
            &root,
            &store,
        )
        .unwrap(),
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
fn observers_share_while_mutation_excludes_every_other_access() {
    let fixture = TestDir::new("lock-shared-exclusive");
    let _guard = StateRootGuard::set(fixture.path());
    let root = std::fs::canonicalize(fixture.path()).unwrap();
    let path = root.join("shared");
    let observed = LockResource::describe(
        LockResourceKind::DiscoveryEntry,
        LockAccess::Observe,
        &root,
        &path,
    )
    .unwrap();
    let mut mutated = observed.clone();
    mutated.access = LockAccess::Mutate;

    let reader_a = HeldLocks::acquire(
        std::slice::from_ref(&observed),
        LockPolicy::immediate(),
        &LockOwner {
            transaction: "reader-a".to_owned(),
            pid: 1001,
        },
    )
    .expect("first observer");
    let reader_b = HeldLocks::acquire(
        std::slice::from_ref(&observed),
        LockPolicy::immediate(),
        &LockOwner {
            transaction: "reader-b".to_owned(),
            pid: 1002,
        },
    )
    .expect("second observer");

    assert!(reader_a.holds_all(std::slice::from_ref(&observed)));
    assert!(!reader_a.holds_all(std::slice::from_ref(&mutated)));
    let contention =
        HeldLocks::try_acquire_all(std::slice::from_ref(&mutated), &LockOwner::preliminary())
            .unwrap()
            .expect_err("mutation must contend with both observers");
    assert_eq!(contention.access, LockAccess::Mutate);

    drop(reader_a);
    assert!(
        HeldLocks::try_acquire_all(std::slice::from_ref(&mutated), &LockOwner::preliminary())
            .unwrap()
            .is_err(),
        "one remaining observer still excludes mutation"
    );
    drop(reader_b);

    let writer =
        HeldLocks::try_acquire_all(std::slice::from_ref(&mutated), &LockOwner::preliminary())
            .unwrap()
            .expect("mutation is available after all observers release");
    assert!(writer.holds_all(std::slice::from_ref(&mutated)));
    assert!(
        writer.holds_all(std::slice::from_ref(&observed)),
        "mutation access satisfies an observation request"
    );
    assert!(
        HeldLocks::try_acquire_all(&[observed], &LockOwner::preliminary())
            .unwrap()
            .is_err(),
        "mutation excludes a later observer"
    );
}

#[test]
fn observation_to_mutation_requires_a_fresh_acquisition_pass() {
    let fixture = TestDir::new("lock-access-promotion");
    let _guard = StateRootGuard::set(fixture.path());
    let root = std::fs::canonicalize(fixture.path()).unwrap();
    let path = root.join("shared");
    let observed = LockResource::describe(
        LockResourceKind::DiscoveryEntry,
        LockAccess::Observe,
        &root,
        &path,
    )
    .unwrap();
    let mut mutated = observed.clone();
    mutated.access = LockAccess::Mutate;
    let mut held = HeldLocks::acquire(
        std::slice::from_ref(&observed),
        LockPolicy::immediate(),
        &LockOwner::preliminary(),
    )
    .expect("observation lock");

    assert!(held.requires_reacquire(std::slice::from_ref(&mutated)));
    assert!(matches!(
        held.try_acquire_missing(std::slice::from_ref(&mutated), &LockOwner::preliminary())
            .unwrap(),
        MissingLockOutcome::RequiresReacquire
    ));
    let error = held
        .acquire_more(
            std::slice::from_ref(&mutated),
            LockPolicy::immediate(),
            &LockOwner::preliminary(),
        )
        .expect_err("in-place promotion must be refused");
    assert_eq!(error.category(), crate::error::ExitCategory::Internal);
}

#[test]
fn a_first_creator_and_a_later_observer_cannot_apply_concurrently() {
    let fixture = TestDir::new("lock-first-creator");
    let _guard = StateRootGuard::set(fixture.path());
    let root = std::fs::canonicalize(fixture.path()).unwrap();
    let store = root.join(".codex/skills");

    // One session plans the store before it exists and takes its lock.
    let planned = LockResource::describe(
        LockResourceKind::BackingStore,
        crate::lock::LockAccess::Mutate,
        &root,
        &store,
    )
    .unwrap();
    let held = HeldLocks::acquire(
        std::slice::from_ref(&planned),
        LockPolicy::immediate(),
        &LockOwner::preliminary(),
    )
    .expect("an uncontended lock is available");

    // The store appears, and a second session observes it.
    std::fs::create_dir_all(&store).expect("store fixture");
    let observed = LockResource::describe(
        LockResourceKind::BackingStore,
        crate::lock::LockAccess::Mutate,
        &root,
        &store,
    )
    .unwrap();

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
    let resource = LockResource::describe(
        LockResourceKind::BackingStore,
        crate::lock::LockAccess::Mutate,
        &root,
        &root.join("s"),
    )
    .unwrap();

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
    let free = LockResource::describe(
        LockResourceKind::BackingStore,
        crate::lock::LockAccess::Mutate,
        &root,
        &root.join("free"),
    )
    .unwrap();
    let busy = LockResource::describe(
        LockResourceKind::BackingStore,
        crate::lock::LockAccess::Mutate,
        &root,
        &root.join("busy"),
    )
    .unwrap();
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
fn a_missing_key_contention_keeps_the_partly_overlapping_resource_path() {
    let fixture = TestDir::new("lock-partial-diagnostic");
    let _guard = StateRootGuard::set(fixture.path());
    let root = std::fs::canonicalize(fixture.path()).unwrap();
    let store = root.join("store");
    std::fs::create_dir_all(&store).expect("store fixture");
    let alias = root.join("alias");
    if !symlink_dir_or_skip(&store, &alias) {
        return;
    }

    let full = LockResource::describe(
        LockResourceKind::BackingStore,
        crate::lock::LockAccess::Mutate,
        &root,
        &store,
    )
    .unwrap();
    let alias_resource = LockResource::describe(
        LockResourceKind::BackingStore,
        crate::lock::LockAccess::Mutate,
        &root,
        &alias,
    )
    .unwrap();
    let _physical_blocker = HeldLocks::acquire(
        std::slice::from_ref(&alias_resource),
        LockPolicy::immediate(),
        &LockOwner::preliminary(),
    )
    .expect("the alias holds the shared physical key");
    let mut logical_only = full.clone();
    logical_only.identity.physical = None;
    let current = HeldLocks::acquire(
        &[logical_only],
        LockPolicy::immediate(),
        &LockOwner::preliminary(),
    )
    .expect("the current process already holds only the logical key");

    let MissingLockOutcome::Contended(contention) = current
        .try_acquire_missing(std::slice::from_ref(&full), &LockOwner::preliminary())
        .expect("lock files are available")
    else {
        panic!("the exact missing physical key must be held by the alias");
    };

    assert_eq!(contention.resources, vec![store]);
}

#[test]
fn an_expired_wait_names_the_contended_resource_as_a_temporary_failure() {
    let fixture = TestDir::new("lock-wait");
    let _guard = StateRootGuard::set(fixture.path());
    let root = std::fs::canonicalize(fixture.path()).unwrap();
    let resource = LockResource::describe(
        LockResourceKind::BackingStore,
        crate::lock::LockAccess::Mutate,
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

fn holder_records(resource: &LockResource) -> Vec<std::path::PathBuf> {
    let directory = crate::state::lock_base()
        .unwrap()
        .join(format!("{}.owners", resource.lock_keys()[0]));
    let Ok(entries) = std::fs::read_dir(directory) else {
        return Vec::new();
    };
    let mut records = entries
        .flatten()
        .map(|entry| entry.path())
        .filter(|path| path.is_file())
        .collect::<Vec<_>>();
    records.sort();
    records
}

#[test]
fn holder_diagnostics_are_recorded_without_becoming_liveness_evidence() {
    let fixture = TestDir::new("lock-holder");
    let _guard = StateRootGuard::set(fixture.path());
    let root = std::fs::canonicalize(fixture.path()).unwrap();
    let resource = LockResource::describe(
        LockResourceKind::BackingStore,
        LockAccess::Mutate,
        &root,
        &root.join("s"),
    )
    .unwrap();
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
    let records = holder_records(&resource);
    assert_eq!(records.len(), 1);
    let description = records[0].clone();
    let contents = std::fs::read_to_string(&description)
        .expect("the holder description is readable while the lock is held");
    let lock_file = crate::state::lock_base()
        .unwrap()
        .join(resource.lock_keys()[0].file_name());
    let lock_file_length = std::fs::metadata(&lock_file).unwrap().len();
    drop(held);

    assert!(contents.contains("transaction=abc123"));
    assert!(contents.contains("pid=4242"));
    assert!(
        contents.contains("is evidence that anyone still holds it"),
        "the description must say plainly that it is not liveness: {contents}"
    );
    assert_eq!(
        lock_file_length, 0,
        "the lock file carries no content, so nothing has to be read out of a locked range"
    );
    assert!(
        !description.exists(),
        "releasing the lock clears only this holder's description"
    );
}

#[test]
fn advisory_observation_detects_shared_and_exclusive_locks_without_writing() {
    let fixture = TestDir::new("lock-observe");
    let _guard = StateRootGuard::set(fixture.path());
    let root = std::fs::canonicalize(fixture.path()).unwrap();
    let mut resource = LockResource::describe(
        LockResourceKind::BackingStore,
        LockAccess::Mutate,
        &root,
        &root.join("s"),
    )
    .unwrap();
    let lock_base = crate::state::lock_base().unwrap();
    assert!(!lock_base.exists());

    let absent = observe(std::slice::from_ref(&resource)).expect("missing locks are free");
    assert!(
        absent
            .iter()
            .all(|entry| entry.state == AdvisoryLockState::Free)
    );
    assert!(
        !lock_base.exists(),
        "observation must not create a lock directory, file, or sidecar"
    );

    for (access, transaction) in [
        (LockAccess::Observe, "shared-observer"),
        (LockAccess::Mutate, "exclusive-mutator"),
    ] {
        resource.access = access;
        let held = HeldLocks::acquire(
            std::slice::from_ref(&resource),
            LockPolicy::immediate(),
            &LockOwner {
                transaction: transaction.to_owned(),
                pid: 4242,
            },
        )
        .expect("fixture lock");
        let records_before = holder_records(&resource);
        let active = observe(std::slice::from_ref(&resource)).expect("held lock is observable");

        assert_eq!(active.len(), 1);
        assert_eq!(active[0].access, access);
        assert_eq!(active[0].key, resource.lock_keys()[0]);
        assert!(matches!(
            &active[0].state,
            AdvisoryLockState::Held { holder: Some(holder) }
                if holder.contains(&format!("transaction={transaction}"))
        ));
        assert_eq!(
            holder_records(&resource),
            records_before,
            "observation must not add or replace holder text"
        );

        drop(held);
        let released =
            observe(std::slice::from_ref(&resource)).expect("released lock is observable");
        assert!(
            released
                .iter()
                .all(|entry| entry.state == AdvisoryLockState::Free),
            "a leftover lock file is not liveness evidence"
        );
    }
}

#[test]
fn a_stale_holder_record_never_authorizes_taking_the_lock_or_blocks_it() {
    let fixture = TestDir::new("lock-pid-reuse");
    let _guard = StateRootGuard::set(fixture.path());
    let root = std::fs::canonicalize(fixture.path()).unwrap();
    let resource = LockResource::describe(
        LockResourceKind::BackingStore,
        LockAccess::Mutate,
        &root,
        &root.join("s"),
    )
    .unwrap();
    let directory = crate::state::lock_base().unwrap();
    crate::state::ensure_private_directory(&directory).unwrap();
    std::fs::write(directory.join(resource.lock_keys()[0].file_name()), b"")
        .expect("fixture lock file");
    let holder_directory = directory.join(format!("{}.owners", resource.lock_keys()[0]));
    crate::state::ensure_private_directory(&holder_directory).unwrap();
    std::fs::write(
        holder_directory.join("stale.owner"),
        format!("transaction=dead pid={}\n", std::process::id()),
    )
    .expect("fixture owner record");

    let taken =
        HeldLocks::try_acquire_all(std::slice::from_ref(&resource), &LockOwner::preliminary())
            .unwrap();

    assert!(
        taken.is_ok(),
        "a leftover record must not block a session, whatever pid it names"
    );
    let held = taken.expect("checked above");
    assert!(
        HeldLocks::try_acquire_all(&[resource], &LockOwner::preliminary())
            .unwrap()
            .is_err(),
        "a real kernel lock must still exclude another mutator"
    );
    drop(held);
}

#[test]
fn contention_remains_authoritative_when_holder_text_is_absent() {
    let fixture = TestDir::new("lock-holder-absent");
    let _guard = StateRootGuard::set(fixture.path());
    let root = std::fs::canonicalize(fixture.path()).unwrap();
    let resource = LockResource::describe(
        LockResourceKind::BackingStore,
        LockAccess::Mutate,
        &root,
        &root.join("s"),
    )
    .unwrap();
    let held = HeldLocks::acquire(
        std::slice::from_ref(&resource),
        LockPolicy::immediate(),
        &LockOwner {
            transaction: "hidden".to_owned(),
            pid: 4242,
        },
    )
    .expect("fixture holder");
    for record in holder_records(&resource) {
        std::fs::remove_file(record).expect("remove advisory text");
    }

    let contention = HeldLocks::try_acquire_all(&[resource], &LockOwner::preliminary())
        .unwrap()
        .expect_err("kernel lock still contends");
    assert!(contention.holder.is_none());
    drop(held);
}

#[test]
fn one_reader_never_erases_another_readers_holder_record() {
    let fixture = TestDir::new("lock-holder-overlap");
    let _guard = StateRootGuard::set(fixture.path());
    let root = std::fs::canonicalize(fixture.path()).unwrap();
    let resource = LockResource::describe(
        LockResourceKind::DiscoveryEntry,
        LockAccess::Observe,
        &root,
        &root.join("s"),
    )
    .unwrap();
    let reader_a = HeldLocks::acquire(
        std::slice::from_ref(&resource),
        LockPolicy::immediate(),
        &LockOwner {
            transaction: "reader-a".to_owned(),
            pid: 1001,
        },
    )
    .expect("reader a");
    let reader_b = HeldLocks::acquire(
        std::slice::from_ref(&resource),
        LockPolicy::immediate(),
        &LockOwner {
            transaction: "reader-b".to_owned(),
            pid: 1002,
        },
    )
    .expect("reader b");
    assert_eq!(holder_records(&resource).len(), 2);

    drop(reader_a);
    let remaining = holder_records(&resource);
    assert_eq!(remaining.len(), 1);
    let remaining_text = std::fs::read_to_string(&remaining[0]).unwrap();
    assert!(remaining_text.contains("transaction=reader-b"));

    let mut mutated = resource.clone();
    mutated.access = LockAccess::Mutate;
    let contention = HeldLocks::try_acquire_all(&[mutated], &LockOwner::preliminary())
        .unwrap()
        .expect_err("remaining reader excludes mutation");
    let holder = contention.holder.expect("remaining reader is diagnostic");
    assert!(holder.contains("transaction=reader-b"), "{holder}");
    assert!(!holder.contains("transaction=reader-a"), "{holder}");

    drop(reader_b);
    assert!(holder_records(&resource).is_empty());
}

#[test]
fn resources_without_a_shared_key_do_not_serialize() {
    let fixture = TestDir::new("lock-independent");
    let _guard = StateRootGuard::set(fixture.path());
    let root = std::fs::canonicalize(fixture.path()).unwrap();
    let first = LockResource::describe_unanchored(
        LockResourceKind::BackingStore,
        crate::lock::LockAccess::Mutate,
        &root.join("session-a/root/.claude/skills"),
    );
    let second = LockResource::describe_unanchored(
        LockResourceKind::BackingStore,
        crate::lock::LockAccess::Mutate,
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
    let resource = LockResource::describe(
        LockResourceKind::BackingStore,
        crate::lock::LockAccess::Mutate,
        &root,
        &store,
    )
    .unwrap();

    let recorded = crate::journal::JournalLock::from(&resource);
    let rebuilt: LockResource = recorded.to_resource();

    assert_eq!(rebuilt.lock_keys(), resource.lock_keys());
    assert_eq!(rebuilt, resource);
}
