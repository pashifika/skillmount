//! Shared backend-contract tests driven by the in-memory backend.
//!
//! Every case here runs identically on macOS and on Windows, because the filesystem is modelled
//! rather than created. That is the point: a link cycle, a forty-one hop chain, and a junction all
//! have to be reasoned about on both platforms, and only one of the two can actually build each.

use std::path::{Path, PathBuf};

use crate::domain::LinkMode;
use crate::error::{ExitCategory, LinkError};
use crate::link::resolve::{ChainState, ComparablePath, MAX_LINK_DEPTH, resolve_chain};
use crate::link::testing::{Call, RecordingBackend};
use crate::link::{
    CreatedLink, CreatedLinkKind, EntryKind, LinkBackend, LinkRequest, OwnershipMismatch,
    PlacementOutcome, RemoveOutcome,
};

fn chain(backend: &RecordingBackend, entry: &str) -> crate::link::resolve::ResolvedChain {
    resolve_chain(backend, Path::new(entry)).expect("the modelled host never fails to look")
}

fn create(
    backend: &RecordingBackend,
    source: &str,
    staged: &str,
    mode: LinkMode,
) -> Result<CreatedLink, LinkError> {
    backend.create_directory_link(&LinkRequest {
        source: PathBuf::from(source),
        staged_path: PathBuf::from(staged),
        mode,
    })
}

#[test]
fn a_relative_multi_hop_chain_reaches_its_terminal_and_records_raw_targets() {
    let backend = RecordingBackend::new()
        .with_directory("/root/nested/store")
        .with_symlink("/root/middle", "nested/store")
        .with_symlink("/root/entry", "middle");

    let resolved = chain(&backend, "/root/entry");

    assert_eq!(resolved.state, ChainState::LinkToDirectory);
    assert_eq!(resolved.terminal, Some(PathBuf::from("/root/nested/store")));
    assert_eq!(
        resolved
            .hops
            .iter()
            .map(|hop| hop.raw.clone())
            .collect::<Vec<_>>(),
        [PathBuf::from("middle"), PathBuf::from("nested/store")],
        "each hop is recorded exactly as it is stored, not as it resolves"
    );
    assert_eq!(resolved.entry.kind, EntryKind::Symlink);
}

#[test]
fn an_absolute_chain_and_a_junction_chain_reach_the_same_terminal() {
    let backend = RecordingBackend::new()
        .with_directory("/root/store")
        .with_symlink("/root/by-symlink", "/root/store")
        .with_junction("/root/by-junction", "/root/store");

    let by_symlink = chain(&backend, "/root/by-symlink");
    let by_junction = chain(&backend, "/root/by-junction");

    assert_eq!(by_symlink.entry.kind, EntryKind::Symlink);
    assert_eq!(by_junction.entry.kind, EntryKind::Junction);
    assert!(by_symlink.shares_terminal_with(&by_junction));
    assert_eq!(
        by_junction.require_directory().unwrap(),
        Path::new("/root/store")
    );
}

#[test]
fn a_broken_hop_is_distinguished_from_an_entry_that_was_never_there() {
    let backend = RecordingBackend::new().with_symlink("/root/broken", "/root/absent");

    let broken = chain(&backend, "/root/broken");
    let missing = chain(&backend, "/root/nothing-here");

    assert_eq!(broken.state, ChainState::Broken);
    assert_eq!(broken.entry.kind, EntryKind::Symlink);
    assert_eq!(missing.state, ChainState::Missing);
    assert_eq!(missing.entry.kind, EntryKind::Missing);
    assert!(broken.terminal.is_none());
}

#[test]
fn a_cycle_is_detected_by_identity_rather_than_by_running_out_of_depth() {
    let backend = RecordingBackend::new()
        .with_symlink("/root/a", "/root/b")
        .with_symlink("/root/b", "/root/a");

    let resolved = chain(&backend, "/root/a");

    assert_eq!(resolved.state, ChainState::Cyclic);
    assert!(
        backend.calls().len() < MAX_LINK_DEPTH,
        "a cycle must stop as soon as an entry repeats, not at the depth backstop"
    );
}

#[test]
fn the_deepest_resolvable_chain_is_accepted_and_one_hop_more_is_not() {
    // `hop-k` is a chain of `k + 1` links, so the deepest one the bound admits starts at
    // `hop-(MAX_LINK_DEPTH - 1)`. This is the assertion that pins the constant to its own
    // documentation: a bound of 40 has to accept 40 hops, not 39.
    let backend = RecordingBackend::new().with_directory("/root/store");
    let deepest = MAX_LINK_DEPTH - 1;
    for index in 0..=deepest + 1 {
        let target = if index == 0 {
            "/root/store".to_owned()
        } else {
            format!("/root/hop-{}", index - 1)
        };
        backend.add_symlink(&format!("/root/hop-{index}"), &target);
    }

    assert_eq!(
        chain(&backend, &format!("/root/hop-{deepest}")).state,
        ChainState::LinkToDirectory
    );
    assert_eq!(
        chain(&backend, &format!("/root/hop-{deepest}")).hops.len(),
        MAX_LINK_DEPTH,
        "the deepest accepted chain is exactly as long as the constant claims"
    );
    assert_eq!(
        chain(&backend, &format!("/root/hop-{}", deepest + 1)).state,
        ChainState::DepthExceeded,
        "one hop past the bound must be a state, never an unbounded walk"
    );
}

#[test]
fn a_chain_that_reaches_a_file_or_an_unsupported_entry_is_not_a_namespace() {
    let backend = RecordingBackend::new()
        .with_file("/root/notes.txt")
        .with_other("/root/device")
        .with_symlink("/root/to-file", "/root/notes.txt")
        .with_symlink("/root/to-device", "/root/device");

    for entry in ["/root/to-file", "/root/to-device", "/root/device"] {
        let resolved = chain(&backend, entry);
        assert_eq!(resolved.state, ChainState::Unsupported, "{entry}");
        assert!(resolved.terminal.is_none(), "{entry}");
    }
}

#[test]
fn an_unresolvable_chain_reports_its_own_state_rather_than_a_generic_failure() {
    let backend = RecordingBackend::new().with_symlink("/root/broken", "/root/absent");

    let error = chain(&backend, "/root/broken")
        .require_directory()
        .expect_err("a broken chain has no directory");

    assert!(matches!(error, LinkError::UnresolvableChain { .. }));
    assert!(error.to_string().contains("broken link"));
    assert_eq!(
        crate::error::AppError::from(error).category(),
        ExitCategory::Filesystem
    );
}

#[test]
fn resolution_inspects_each_hop_without_following_it() {
    let backend = RecordingBackend::new()
        .with_directory("/root/store")
        .with_symlink("/root/entry", "/root/store");

    let _ = chain(&backend, "/root/entry");

    assert_eq!(
        backend.calls(),
        [
            Call::Inspect(PathBuf::from("/root/entry")),
            Call::Inspect(PathBuf::from("/root/store")),
            Call::Canonicalize(PathBuf::from("/root/store")),
        ],
        "the walker asks for one no-follow look per hop and canonicalizes only the terminal"
    );
}

#[test]
fn comparison_keeps_the_path_the_operator_supplied() {
    let noisy = ComparablePath::new(Path::new("/root/./skills/../skills/rust"));
    let plain = ComparablePath::new(Path::new("/root/skills/rust"));

    assert!(noisy.names_same_path(&plain));
    assert_eq!(
        noisy.display_path(),
        Path::new("/root/./skills/../skills/rust"),
        "a diagnostic must quote what the operator typed, not what it normalized to"
    );
    assert_ne!(noisy.display_path(), noisy.key());
}

#[test]
fn containment_is_compared_by_component_and_not_by_prefix_text() {
    let store = ComparablePath::new(Path::new("/root/skills/a"));

    assert!(store.contains(&ComparablePath::new(Path::new("/root/skills/a/rust"))));
    assert!(store.contains(&store), "a store contains itself");
    assert!(
        !store.contains(&ComparablePath::new(Path::new("/root/skills/ab"))),
        "a sibling whose name merely starts with the same text is not contained"
    );
    assert!(!store.contains(&ComparablePath::new(Path::new("/root/skills"))));
}

#[cfg(unix)]
#[test]
fn a_non_unicode_path_survives_inspection_and_comparison() {
    use crate::link::resolve::targets_match;
    use std::ffi::OsString;
    use std::os::unix::ffi::OsStringExt;

    let raw = PathBuf::from(OsString::from_vec(b"/root/sk\xffills".to_vec()));
    let comparable = ComparablePath::new(&raw);

    assert_eq!(comparable.display_path(), raw, "no lossy rewrite anywhere");
    assert!(comparable.names_same_path(&ComparablePath::new(&raw)));
    assert!(targets_match(&raw, &raw));
    assert!(!targets_match(&raw, Path::new("/root/skills")));
}

#[cfg(windows)]
#[test]
fn a_path_with_unpaired_surrogates_survives_inspection_and_comparison() {
    use crate::link::resolve::targets_match;
    use std::ffi::OsString;
    use std::os::windows::ffi::OsStringExt;

    // The Windows counterpart of a non-UTF-8 Unix path. An unpaired surrogate is representable in
    // a filename and is exactly why Rust stores Windows paths as WTF-8, so normalization must pass
    // it through rather than replacing it.
    let raw = PathBuf::from(OsString::from_wide(&[
        u16::from(b'C'),
        u16::from(b':'),
        u16::from(b'\\'),
        0xD800,
        u16::from(b's'),
    ]));
    let comparable = ComparablePath::new(&raw);

    assert_eq!(comparable.display_path(), raw, "no lossy rewrite anywhere");
    assert!(comparable.names_same_path(&ComparablePath::new(&raw)));
    assert!(targets_match(&raw, &raw));
    assert!(!targets_match(&raw, Path::new(r"C:\s")));
}

#[test]
fn creation_refuses_to_replace_an_occupied_staging_path() {
    let backend = RecordingBackend::new()
        .with_directory("/root/source")
        .with_file("/root/staged");

    let error = create(&backend, "/root/source", "/root/staged", LinkMode::Auto)
        .expect_err("an occupied staging path must not be replaced");

    assert!(matches!(error, LinkError::Create { .. }));
    assert!(backend.contains(Path::new("/root/staged")));
}

#[test]
fn placement_preserves_a_destination_that_appeared_after_staging() {
    let backend = RecordingBackend::new()
        .with_directory("/root/source")
        .with_directory("/root/destination");
    let staged =
        create(&backend, "/root/source", "/root/staged", LinkMode::Auto).expect("staging succeeds");

    let outcome = backend
        .place_no_replace(&staged, Path::new("/root/destination"))
        .expect("placement reports rather than fails");

    assert_eq!(outcome, PlacementOutcome::DestinationExists);
    assert!(
        backend.contains(Path::new("/root/staged")),
        "the staged entry stays available for verified rollback"
    );
}

#[test]
fn a_placed_link_is_removed_by_its_recorded_identity_and_the_source_survives() {
    let backend = RecordingBackend::new().with_directory("/root/source");
    let staged =
        create(&backend, "/root/source", "/root/staged", LinkMode::Auto).expect("staging succeeds");
    let PlacementOutcome::Placed(placed) = backend
        .place_no_replace(&staged, Path::new("/root/destination"))
        .expect("placement succeeds")
    else {
        panic!("an empty destination must accept the staged entry");
    };

    assert_eq!(
        backend
            .remove_link_entry(&placed)
            .expect("removal succeeds"),
        RemoveOutcome::Removed
    );
    assert!(!backend.contains(Path::new("/root/destination")));
    assert!(
        backend.contains(Path::new("/root/source")),
        "removing a link must never reach its target"
    );
    assert_eq!(
        backend
            .remove_link_entry(&placed)
            .expect("removal is idempotent"),
        RemoveOutcome::AlreadyAbsent
    );
}

/// Builds the evidence a removal would have recorded for `/root/destination`.
fn recorded_link(identity: Option<crate::link::PlatformIdentity>) -> CreatedLink {
    CreatedLink {
        path: PathBuf::from("/root/destination"),
        kind: CreatedLinkKind::Symlink,
        target: PathBuf::from("/root/source"),
        source_canonical: PathBuf::from("/root/source"),
        identity,
    }
}

#[test]
fn removal_refuses_every_entry_that_is_no_longer_the_recorded_one() {
    // Kind mismatches are decided before identity is consulted, so they are refused whether or
    // not one was recorded.
    let cases = [
        (
            RecordingBackend::new().with_directory("/root/destination"),
            OwnershipMismatch::RegularDirectory,
        ),
        (
            RecordingBackend::new().with_file("/root/destination"),
            OwnershipMismatch::NotALink,
        ),
        (
            RecordingBackend::new().with_junction("/root/destination", "/root/source"),
            OwnershipMismatch::KindChanged,
        ),
    ];

    for (backend, expected) in cases {
        assert_eq!(
            backend
                .remove_link_entry(&recorded_link(None))
                .expect("removal reports"),
            RemoveOutcome::OwnershipMismatch(expected),
            "{expected:?}"
        );
        assert!(
            backend.contains(Path::new("/root/destination")),
            "a mismatched entry is left exactly as it is"
        );
    }

    // A retargeted link is only reachable once identity agrees, so this case takes the live
    // entry's own identity and changes only where the link points.
    let backend = RecordingBackend::new().with_symlink("/root/destination", "/root/elsewhere");
    let live = backend
        .inspect_no_follow(Path::new("/root/destination"))
        .expect("inspection works");

    assert_eq!(
        backend
            .remove_link_entry(&recorded_link(live.identity))
            .expect("removal reports"),
        RemoveOutcome::OwnershipMismatch(OwnershipMismatch::TargetChanged)
    );
    assert!(backend.contains(Path::new("/root/destination")));
}

#[test]
fn an_entry_whose_identity_is_unavailable_is_never_removed() {
    // The live entry is exactly what was recorded — same kind, same target — and is still refused,
    // because without an identity on both sides nothing distinguishes it from an identical entry
    // another process created at the same path.
    let backend = RecordingBackend::new()
        .with_directory("/root/source")
        .with_symlink("/root/destination", "/root/source");

    assert_eq!(
        backend
            .remove_link_entry(&recorded_link(None))
            .expect("removal reports"),
        RemoveOutcome::OwnershipMismatch(OwnershipMismatch::IdentityUnavailable)
    );
    assert!(
        backend.contains(Path::new("/root/destination")),
        "leaving an entry behind is recoverable; removing someone else's is not"
    );
}

#[test]
fn an_entry_recreated_at_the_same_path_and_target_is_still_not_ours() {
    let backend = RecordingBackend::new().with_directory("/root/source");
    let mine = create(
        &backend,
        "/root/source",
        "/root/destination",
        LinkMode::Auto,
    )
    .expect("creation succeeds");
    backend.remove_link_entry(&mine).expect("removal succeeds");
    let theirs = create(
        &backend,
        "/root/source",
        "/root/destination",
        LinkMode::Auto,
    )
    .expect("someone else creates the same link");

    assert_ne!(mine.identity, theirs.identity);
    assert_eq!(
        backend.remove_link_entry(&mine).expect("removal reports"),
        RemoveOutcome::OwnershipMismatch(OwnershipMismatch::IdentityChanged)
    );
}

#[test]
fn a_link_implementation_the_backend_does_not_provide_is_refused_rather_than_substituted() {
    let backend = RecordingBackend::new().with_directory("/root/source");

    let error = create(&backend, "/root/source", "/root/staged", LinkMode::Junction)
        .expect_err("an unavailable implementation must fail");

    assert!(matches!(error, LinkError::Unsupported { .. }));
    assert!(
        !backend.contains(Path::new("/root/staged")),
        "a failed request leaves nothing behind, least of all a copied tree"
    );
}

#[test]
fn a_link_request_carries_the_mode_the_caller_asked_for() {
    let request = LinkRequest {
        source: PathBuf::from("/root/source"),
        staged_path: PathBuf::from("/root/staged"),
        mode: LinkMode::Symlink,
    };

    assert_eq!(request.mode, LinkMode::Symlink);
    assert_eq!(CreatedLinkKind::Junction.entry_kind(), EntryKind::Junction);
    assert_eq!(CreatedLinkKind::Symlink.entry_kind(), EntryKind::Symlink);
}
