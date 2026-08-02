//! Native macOS backend tests, run against a real filesystem.
//!
//! The modelled backend proves the walker; these prove the platform. Every case that removes
//! something asserts a sentinel file inside the source directory afterwards, because "the link went
//! away" and "the link and the user's Skills went away" look identical from the destination side.

use std::fs;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Barrier};
use std::thread;

use crate::domain::LinkMode;
use crate::error::LinkError;
use crate::link::resolve::{ChainState, resolve_chain};
use crate::link::{
    CreatedLink, CreatedLinkKind, EntryKind, LinkRequest, OwnershipMismatch, PlacementMismatch,
    PlacementOutcome, RemoveOutcome, platform_backend,
};
use crate::test_support::TestDir;

use super::testing::{HookPoint, with_hook};

/// The file every source directory carries, so a test can prove removal never reached it.
const SENTINEL: &str = "SKILL.md";

struct Fixture {
    _dir: TestDir,
    root: PathBuf,
}

impl Fixture {
    /// Builds a fixture rooted at a canonical path.
    ///
    /// The temporary directory itself sits behind `/private/var` on macOS, so an uncanonicalized
    /// root would make every expected terminal path wrong for reasons that have nothing to do with
    /// the code under test.
    fn new(label: &str) -> Self {
        let dir = TestDir::new(label);
        let root = fs::canonicalize(dir.path()).expect("the fixture root exists");
        Self { _dir: dir, root }
    }

    fn path(&self, relative: &str) -> PathBuf {
        self.root.join(relative)
    }

    /// Creates a source directory holding a sentinel and returns it.
    fn source(&self, relative: &str) -> PathBuf {
        let path = self.path(relative);
        fs::create_dir_all(&path).expect("the source directory is created");
        fs::write(path.join(SENTINEL), "---\nname: fixture\n---\n")
            .expect("the sentinel is written");
        path
    }
}

/// Fails unless the sentinel inside `source` is still there.
///
/// Every removal case calls this. "The link went away" and "the link and the user's Skills went
/// away" look identical from the destination side, so the source is what has to be asserted.
fn assert_source_intact(source: &Path) {
    assert!(
        source.join(SENTINEL).is_file(),
        "removing a link must never reach {}",
        source.display()
    );
}

fn stage(source: &Path, staged: &Path, mode: LinkMode) -> Result<CreatedLink, LinkError> {
    platform_backend().create_directory_link(&LinkRequest {
        source: source.to_path_buf(),
        staged_path: staged.to_path_buf(),
        mode,
    })
}

fn injected_hook_error(path: &Path, boundary: &str) -> LinkError {
    LinkError::Inspect {
        path: path.to_path_buf(),
        reason: format!("injected failure at {boundary}"),
    }
}

#[test]
fn a_staged_link_is_placed_removed_and_leaves_its_source_untouched() {
    let fixture = Fixture::new("unix-lifecycle");
    let source = fixture.source("skills/rust");
    let staged = fixture.path("skills/.rust.staged");
    let destination = fixture.path("skills/mounted");
    let backend = platform_backend();

    let created = stage(&source, &staged, LinkMode::Auto).expect("creation succeeds");
    assert_eq!(created.kind, CreatedLinkKind::Symlink);
    assert_eq!(created.source_canonical, source);
    assert!(created.identity.is_some(), "macOS always reports dev/ino");

    let PlacementOutcome::Placed(placed) = backend
        .place_no_replace(&created, &destination)
        .expect("placement succeeds")
    else {
        panic!("an empty destination must accept the staged entry");
    };
    assert!(
        !staged.exists(),
        "the staged sibling is consumed by placement"
    );
    assert_eq!(
        placed.identity, created.identity,
        "a rename moves the directory entry and keeps the inode"
    );

    let chain = resolve_chain(backend, &destination).expect("the destination resolves");
    assert_eq!(chain.state, ChainState::LinkToDirectory);
    assert_eq!(chain.terminal.as_deref(), Some(source.as_path()));
    assert_eq!(chain.entry.kind, EntryKind::Symlink);

    assert_eq!(
        backend
            .remove_link_entry(&placed)
            .expect("removal succeeds"),
        RemoveOutcome::Removed
    );
    assert_eq!(
        backend
            .inspect_no_follow(&destination)
            .expect("inspection succeeds")
            .kind,
        EntryKind::Missing
    );
    assert_source_intact(&source);
}

#[test]
fn failed_post_create_link_inspection_retains_the_staged_path() {
    let fixture = Fixture::new("unix-create-residue");
    let source = fixture.source("source");
    let replacement_source = fixture.source("replacement-source");
    let staged = fixture.path("staged");
    let hook_path = staged.clone();
    let hook_target = replacement_source.clone();

    let error = with_hook(
        move |event| {
            if event.point == HookPoint::AfterLinkCreation && event.path == hook_path {
                fs::remove_file(&hook_path).expect("the created link is replaced before evidence");
                std::os::unix::fs::symlink(&hook_target, &hook_path)
                    .expect("the replacement takes the staged pathname");
                return Err(injected_hook_error(&hook_path, "link creation"));
            }
            Ok(())
        },
        || stage(&source, &staged, LinkMode::Auto),
    )
    .expect_err("an unproved created link must not be deleted by pathname");

    assert!(matches!(error, LinkError::Create { .. }));
    assert!(error.to_string().contains("retained staged path"));
    assert!(
        staged.is_symlink(),
        "the ambiguous staged entry is retained"
    );
    assert_eq!(
        fs::canonicalize(&staged).expect("the retained replacement resolves"),
        replacement_source,
        "failure before initial evidence must not pathname-delete the replacement"
    );
    fs::remove_file(&staged).expect("the fixture-owned residue is cleaned up");
    assert_source_intact(&source);
    assert_source_intact(&replacement_source);
}

#[test]
fn failed_post_create_directory_inspection_retains_the_staged_path() {
    let fixture = Fixture::new("unix-directory-create-residue");
    let staged = fixture.path("staged");
    let hook_path = staged.clone();

    let error = with_hook(
        move |event| {
            if event.point == HookPoint::AfterDirectoryCreation && event.path == hook_path {
                fs::remove_dir(&hook_path)
                    .expect("the created directory is replaced before evidence");
                fs::create_dir(&hook_path).expect("the replacement takes the staged pathname");
                fs::write(hook_path.join("their-own-work"), "keep")
                    .expect("the replacement carries operator state");
                return Err(injected_hook_error(&hook_path, "directory creation"));
            }
            Ok(())
        },
        || platform_backend().create_directory(&staged),
    )
    .expect_err("an unproved created directory must not be deleted by pathname");

    assert!(matches!(error, LinkError::Create { .. }));
    assert!(error.to_string().contains("retained staged path"));
    assert!(
        staged.join("their-own-work").is_file(),
        "the ambiguous staged replacement is retained"
    );
    fs::remove_file(staged.join("their-own-work")).expect("the fixture residue is emptied");
    fs::remove_dir(&staged).expect("the fixture-owned residue is cleaned up");
}

#[test]
fn placement_preserves_a_destination_that_appeared_after_staging() {
    let fixture = Fixture::new("unix-placement-conflict");
    let source = fixture.source("skills/rust");
    let staged = fixture.path("staged");
    let destination = fixture.source("mounted");
    let backend = platform_backend();

    let created = stage(&source, &staged, LinkMode::Auto).expect("creation succeeds");

    assert_eq!(
        backend
            .place_no_replace(&created, &destination)
            .expect("placement reports rather than overwrites"),
        PlacementOutcome::DestinationExists
    );
    assert_source_intact(&destination);
    assert_eq!(
        backend.remove_link_entry(&created).expect("rollback works"),
        RemoveOutcome::Removed,
        "the staged entry stays available for verified rollback"
    );
    assert_source_intact(&source);
}

#[test]
fn placement_refuses_a_replacement_visible_before_the_final_rename() {
    let fixture = Fixture::new("unix-placement-precheck-replacement");
    let source = fixture.source("source");
    let replacement_source = fixture.source("replacement-source");
    let staged = fixture.path("staged");
    let destination = fixture.path("destination");
    let created = stage(&source, &staged, LinkMode::Auto).expect("creation succeeds");
    let hook_staged = staged.clone();
    let hook_target = replacement_source.clone();

    let outcome = with_hook(
        move |event| {
            if event.point == HookPoint::BeforePlacementVerification && event.path == hook_staged {
                fs::remove_file(&hook_staged).expect("the fixture removes the recorded link");
                std::os::unix::fs::symlink(&hook_target, &hook_staged)
                    .expect("a replacement takes the staged pathname");
            }
            Ok(())
        },
        || platform_backend().place_no_replace(&created, &destination),
    )
    .expect("a mismatch is a typed outcome");

    assert!(matches!(
        outcome,
        PlacementOutcome::OwnershipMismatch(ref residue)
            if residue.path == staged
                && matches!(residue.mismatch, PlacementMismatch::Ownership(_))
    ));
    assert!(staged.is_symlink(), "the replacement is left untouched");
    assert!(
        !destination.exists(),
        "placement performs no destination mutation"
    );
    fs::remove_file(&staged).expect("the fixture-owned replacement is cleaned up");
    assert_source_intact(&source);
    assert_source_intact(&replacement_source);
}

#[test]
fn placement_retains_a_final_replacement_crossing_the_pathname_window() {
    let fixture = Fixture::new("unix-placement-postcheck-replacement");
    let source = fixture.source("source");
    let replacement_source = fixture.source("replacement-source");
    let staged = fixture.path("staged");
    let destination = fixture.path("destination");
    let displaced = fixture.path("displaced");
    let created = stage(&source, &staged, LinkMode::Auto).expect("creation succeeds");
    let hook_destination = destination.clone();
    let hook_displaced = displaced.clone();
    let hook_target = replacement_source.clone();

    let outcome = with_hook(
        move |event| {
            if event.point == HookPoint::AfterPlacementMutation && event.path == hook_destination {
                fs::rename(&hook_destination, &hook_displaced)
                    .expect("the fixture moves the placed link out of the way");
                std::os::unix::fs::symlink(&hook_target, &hook_destination)
                    .expect("a replacement takes the destination pathname");
            }
            Ok(())
        },
        || platform_backend().place_no_replace(&created, &destination),
    )
    .expect("a post-placement mismatch is a typed outcome");

    assert!(matches!(
        outcome,
        PlacementOutcome::OwnershipMismatch(ref residue)
            if residue.path == destination
                && matches!(residue.mismatch, PlacementMismatch::Ownership(_))
    ));
    assert!(
        destination.is_symlink(),
        "the final replacement is retained"
    );
    assert!(
        displaced.is_symlink(),
        "the originally placed link is not lost"
    );
    assert_source_intact(&source);
    assert_source_intact(&replacement_source);
    fs::remove_file(&destination).expect("the fixture-owned replacement is cleaned up");
    fs::remove_file(&displaced).expect("the fixture-owned original is cleaned up");
}

#[test]
fn helper_directory_postcheck_retains_a_replacement_instead_of_claiming_it() {
    let fixture = Fixture::new("unix-directory-placement-postcheck");
    let backend = platform_backend();
    let staged = fixture.path("staged");
    let destination = fixture.path("destination");
    let created = backend
        .create_directory(&staged)
        .expect("directory creation succeeds");
    let hook_destination = destination.clone();

    let outcome = with_hook(
        move |event| {
            if event.point == HookPoint::AfterPlacementMutation && event.path == hook_destination {
                fs::remove_dir(&hook_destination)
                    .expect("the fixture removes the placed empty directory");
                fs::create_dir(&hook_destination)
                    .expect("a replacement directory takes the destination");
            }
            Ok(())
        },
        || backend.place_directory_no_replace(&created, &destination),
    )
    .expect("a post-placement mismatch is a typed outcome");

    assert!(matches!(
        outcome,
        PlacementOutcome::OwnershipMismatch(ref residue)
            if residue.path == destination
                && matches!(residue.mismatch, PlacementMismatch::Ownership(_))
    ));
    assert!(
        destination.is_dir(),
        "the replacement directory is retained"
    );
    fs::remove_dir(&destination).expect("the fixture-owned replacement is cleaned up");
}

#[test]
fn exactly_one_of_two_racing_placements_wins_the_destination() {
    let fixture = Fixture::new("unix-placement-race");
    let source = fixture.source("skills/rust");
    let destination = fixture.path("mounted");
    let backend = platform_backend();

    let staged = (0..2)
        .map(|index| {
            stage(
                &source,
                &fixture.path(&format!("staged-{index}")),
                LinkMode::Auto,
            )
            .expect("creation succeeds")
        })
        .collect::<Vec<_>>();

    let barrier = Arc::new(Barrier::new(staged.len()));
    let outcomes = thread::scope(|scope| {
        let handles = staged
            .iter()
            .map(|entry| {
                let barrier = Arc::clone(&barrier);
                let destination = destination.clone();
                scope.spawn(move || {
                    barrier.wait();
                    backend
                        .place_no_replace(entry, &destination)
                        .expect("placement reports rather than fails")
                })
            })
            .collect::<Vec<_>>();
        handles
            .into_iter()
            .map(|handle| handle.join().expect("no placement panics"))
            .collect::<Vec<_>>()
    });

    let winners = outcomes
        .iter()
        .filter(|outcome| matches!(outcome, PlacementOutcome::Placed(_)))
        .count();
    assert_eq!(
        winners, 1,
        "a no-replace rename must let exactly one racer through, never both"
    );
    assert_eq!(
        resolve_chain(backend, &destination)
            .expect("the destination resolves")
            .terminal
            .as_deref(),
        Some(source.as_path())
    );
    assert_source_intact(&source);
}

#[test]
fn removal_refuses_a_regular_directory_and_never_descends_into_it() {
    let fixture = Fixture::new("unix-refuse-directory");
    let source = fixture.source("skills/rust");
    let occupied = fixture.source("mounted");
    let recorded = CreatedLink {
        path: occupied.clone(),
        kind: CreatedLinkKind::Symlink,
        target: source.clone(),
        source_canonical: source.clone(),
        identity: None,
    };

    assert_eq!(
        platform_backend()
            .remove_link_entry(&recorded)
            .expect("removal reports"),
        RemoveOutcome::OwnershipMismatch(OwnershipMismatch::RegularDirectory)
    );
    assert_source_intact(&occupied);
    assert_source_intact(&source);
}

#[test]
fn removal_refuses_a_link_that_now_points_somewhere_else() {
    let fixture = Fixture::new("unix-retargeted");
    let source = fixture.source("skills/rust");
    let elsewhere = fixture.source("skills/other");
    let destination = fixture.path("mounted");
    let backend = platform_backend();

    let created = stage(&source, &destination, LinkMode::Auto).expect("creation succeeds");
    fs::remove_file(&destination).expect("the fixture retargets the entry");
    std::os::unix::fs::symlink(&elsewhere, &destination).expect("the replacement link is created");

    let outcome = backend
        .remove_link_entry(&created)
        .expect("removal reports rather than fails");
    assert!(
        matches!(
            outcome,
            RemoveOutcome::OwnershipMismatch(
                OwnershipMismatch::TargetChanged | OwnershipMismatch::IdentityChanged
            )
        ),
        "a replaced entry must be refused, got {outcome:?}"
    );
    assert!(destination.is_symlink(), "the replacement is left in place");
    assert_source_intact(&elsewhere);
}

#[test]
fn removal_rechecks_after_the_test_boundary_and_retains_a_replacement() {
    let fixture = Fixture::new("unix-removal-final-recheck");
    let source = fixture.source("source");
    let replacement_source = fixture.source("replacement-source");
    let path = fixture.path("mounted");
    let created = stage(&source, &path, LinkMode::Auto).expect("creation succeeds");
    let hook_path = path.clone();
    let hook_target = replacement_source.clone();

    let outcome = with_hook(
        move |event| {
            if event.point == HookPoint::BeforeRemovalMutation && event.path == hook_path {
                fs::remove_file(&hook_path).expect("the fixture removes the verified link");
                std::os::unix::fs::symlink(&hook_target, &hook_path)
                    .expect("a replacement takes the pathname before the final recheck");
            }
            Ok(())
        },
        || platform_backend().remove_link_entry(&created),
    )
    .expect("a replacement is reported rather than removed");

    assert!(matches!(
        outcome,
        RemoveOutcome::OwnershipMismatch(
            OwnershipMismatch::TargetChanged | OwnershipMismatch::IdentityChanged
        )
    ));
    assert!(path.is_symlink(), "the replacement remains at the pathname");
    assert_source_intact(&source);
    assert_source_intact(&replacement_source);
    fs::remove_file(&path).expect("the fixture-owned replacement is cleaned up");
}

#[test]
fn a_relative_and_an_absolute_link_reach_one_directory_and_keep_their_stored_targets() {
    let fixture = Fixture::new("unix-relative");
    let source = fixture.source("skills/rust");
    let relative = fixture.path("skills/relative");
    let absolute = fixture.path("skills/absolute");
    std::os::unix::fs::symlink(Path::new("rust"), &relative).expect("the relative link is created");
    std::os::unix::fs::symlink(&source, &absolute).expect("the absolute link is created");
    let backend = platform_backend();

    let by_relative = resolve_chain(backend, &relative).expect("the relative link resolves");
    let by_absolute = resolve_chain(backend, &absolute).expect("the absolute link resolves");

    assert_eq!(by_relative.terminal.as_deref(), Some(source.as_path()));
    assert!(by_relative.shares_terminal_with(&by_absolute));
    assert_eq!(
        by_relative.hops[0].raw,
        Path::new("rust"),
        "the stored target is reported exactly as stored"
    );
    assert_eq!(by_absolute.hops[0].raw, source);
}

#[test]
fn broken_and_cyclic_layouts_are_states_and_change_nothing() {
    let fixture = Fixture::new("unix-unresolvable");
    let broken = fixture.path("broken");
    let first = fixture.path("cycle-a");
    let second = fixture.path("cycle-b");
    std::os::unix::fs::symlink(fixture.path("absent"), &broken)
        .expect("the broken link is created");
    std::os::unix::fs::symlink(&second, &first).expect("the first cycle link is created");
    std::os::unix::fs::symlink(&first, &second).expect("the second cycle link is created");
    let backend = platform_backend();

    assert_eq!(
        resolve_chain(backend, &broken)
            .expect("resolution reports")
            .state,
        ChainState::Broken
    );
    assert_eq!(
        resolve_chain(backend, &first)
            .expect("resolution reports")
            .state,
        ChainState::Cyclic
    );
    assert!(broken.is_symlink(), "resolution removes nothing");
    assert!(first.is_symlink() && second.is_symlink());
}

#[test]
fn a_case_variant_is_the_same_entry_only_when_the_volume_says_so() {
    let fixture = Fixture::new("unix-case");
    let stored = fixture.source("Skills");
    let variant = fixture.path("skills");
    let backend = platform_backend();

    let stored_entry = backend
        .inspect_no_follow(&stored)
        .expect("inspection works");
    let variant_entry = backend
        .inspect_no_follow(&variant)
        .expect("inspection works");

    match variant_entry.kind {
        // APFS is case-insensitive by default, so both spellings must resolve to one identity.
        // Comparing paths instead would report two different directories.
        EntryKind::Directory => assert_eq!(
            stored_entry.identity, variant_entry.identity,
            "one directory reached by two spellings must have one identity"
        ),
        EntryKind::Missing => assert_eq!(
            stored_entry.kind,
            EntryKind::Directory,
            "on a case-sensitive volume only the stored spelling exists"
        ),
        other => panic!("unexpected classification {other:?}"),
    }
}

#[test]
fn a_name_with_japanese_characters_and_spaces_survives_the_whole_round_trip() {
    let fixture = Fixture::new("unix-unicode-names");
    let source = fixture.source("スキル 集/rust review");
    let staged = fixture.path("staged");
    let backend = platform_backend();

    let created = stage(&source, &staged, LinkMode::Auto).expect("creation succeeds");

    assert_eq!(created.source_canonical, source);
    assert_eq!(
        resolve_chain(backend, &staged)
            .expect("the link resolves")
            .terminal
            .as_deref(),
        Some(source.as_path()),
        "a native name must not be rewritten anywhere in the round trip"
    );
    assert_eq!(
        backend
            .remove_link_entry(&created)
            .expect("removal succeeds"),
        RemoveOutcome::Removed
    );
    assert_source_intact(&source);
}

#[test]
fn a_non_unicode_name_is_carried_verbatim_whether_or_not_the_host_accepts_one() {
    use std::ffi::OsString;
    use std::os::unix::ffi::OsStringExt;

    let fixture = Fixture::new("unix-native-names");
    let source = fixture
        .root
        .join(OsString::from_vec(b"sk\xffills".to_vec()));
    let backend = platform_backend();

    // APFS requires a filename to be valid UTF-8 and rejects this one with `EILSEQ`, so on a stock
    // macOS host the entry cannot be created at all. Both outcomes are asserted rather than one
    // being skipped: where the name can exist the backend must address it, and where it cannot the
    // backend must still report the path it was handed instead of a lossy rendering of it.
    if fs::create_dir(&source).is_err() {
        let entry = backend
            .inspect_no_follow(&source)
            .expect("an unrepresentable name is still a path that can be looked at");
        assert_eq!(entry.kind, EntryKind::Missing);
        assert_eq!(
            entry.path, source,
            "the reported path is byte-for-byte the one that was passed in"
        );
        let error = stage(&source, &fixture.path("staged"), LinkMode::Auto)
            .expect_err("a source the host refuses cannot be mounted");
        assert!(matches!(error, LinkError::Create { .. }));
        return;
    }

    fs::write(source.join(SENTINEL), "sentinel").expect("the sentinel is written");
    let staged = fixture.path("staged");
    let created = stage(&source, &staged, LinkMode::Auto).expect("creation succeeds");
    assert_eq!(created.source_canonical, source);
    assert_eq!(
        resolve_chain(backend, &staged)
            .expect("the link resolves")
            .terminal
            .as_deref(),
        Some(source.as_path())
    );
    assert_eq!(
        backend
            .remove_link_entry(&created)
            .expect("removal succeeds"),
        RemoveOutcome::Removed
    );
    assert_source_intact(&source);
}

#[test]
fn a_finder_alias_is_a_regular_file_and_can_neither_be_followed_nor_mounted() {
    let fixture = Fixture::new("unix-alias");
    // A Finder alias is an ordinary file carrying a bookmark blob. Only Cocoa resolves it; the
    // kernel does not, so an agent following one would find a file where a Skill should be.
    let alias = fixture.path("skills-alias");
    fs::create_dir_all(alias.parent().expect("the fixture root")).expect("the parent exists");
    fs::write(&alias, b"book\0\0\0\0mark").expect("the alias stand-in is written");
    let backend = platform_backend();

    assert_eq!(
        backend
            .inspect_no_follow(&alias)
            .expect("inspection works")
            .kind,
        EntryKind::File
    );
    assert_eq!(
        resolve_chain(backend, &alias)
            .expect("resolution reports")
            .state,
        ChainState::Unsupported
    );
    assert!(matches!(
        stage(&alias, &fixture.path("staged"), LinkMode::Auto),
        Err(LinkError::Create { .. })
    ));
}

#[test]
fn a_junction_is_refused_rather_than_silently_downgraded_to_a_symlink() {
    let fixture = Fixture::new("unix-no-junction");
    let source = fixture.source("skills/rust");
    let staged = fixture.path("staged");

    let error = stage(&source, &staged, LinkMode::Junction)
        .expect_err("junctions do not exist on this platform");

    assert!(matches!(error, LinkError::Unsupported { .. }));
    assert!(
        !staged.exists(),
        "a refused request leaves nothing behind, least of all a copied tree"
    );
}

#[test]
fn creation_refuses_an_occupied_staging_path() {
    let fixture = Fixture::new("unix-occupied");
    let source = fixture.source("skills/rust");
    let occupied = fixture.source("staged");

    let error = stage(&source, &occupied, LinkMode::Auto)
        .expect_err("an occupied staging path must not be replaced");

    assert!(matches!(error, LinkError::Create { .. }));
    assert_source_intact(&occupied);
}
