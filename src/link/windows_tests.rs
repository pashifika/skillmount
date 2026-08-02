//! Native Windows backend tests, run against a real filesystem.
//!
//! One behavior here depends on how the runner is configured rather than on the code: whether a
//! directory symbolic link can be created at all. It is not skipped. It is a fork where both sides
//! assert something, so a runner without Developer Mode still proves the junction path and a runner
//! with it still proves the symbolic-link path.
//!
//! Two rules are proved against values rather than against a live filesystem, because no CI runner
//! reliably has what they need: junction eligibility, which would need a network share and a second
//! volume, and the symlink-failure decision table, which would need a host that refuses the
//! privilege. Both are pure functions for exactly that reason.
//!
//! Every case that removes something asserts a sentinel file inside the target directory
//! afterwards. `RemoveDirectoryW` detaches a reparse point rather than descending into it, and this
//! is the assertion that would fail if that ever stopped being true.

use std::fs;
use std::io;
use std::os::windows::ffi::OsStrExt;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Barrier};
use std::thread;

use windows_sys::Win32::Foundation::{
    ERROR_ACCESS_DENIED, ERROR_DISK_FULL, ERROR_PRIVILEGE_NOT_HELD,
};

use crate::domain::LinkMode;
use crate::error::LinkError;
use crate::link::resolve::{ChainState, resolve_chain, targets_match};
use crate::link::{
    CreatedLink, CreatedLinkKind, EntryKind, LinkRequest, OwnershipMismatch, PlacementOutcome,
    RemoveOutcome, platform_backend,
};
use crate::test_support::TestDir;

use super::windows::{
    SymlinkFailure, classify_symlink_failure, is_privilege_failure, junction_eligibility,
    link_target,
};

/// The file every source directory carries, so a test can prove removal never reached it.
const SENTINEL: &str = "SKILL.md";

struct Fixture {
    _dir: TestDir,
    root: PathBuf,
}

impl Fixture {
    /// Builds a fixture rooted at a canonical path.
    ///
    /// Canonicalization yields the verbatim `\\?\` form, which is also what lets the long-path
    /// case below build a path past the 260-character limit with ordinary `std::fs` calls.
    fn new(label: &str) -> Self {
        let dir = TestDir::new(label);
        let root = fs::canonicalize(dir.path()).expect("the fixture root exists");
        Self { _dir: dir, root }
    }

    fn path(&self, relative: &str) -> PathBuf {
        self.root.join(relative)
    }

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

fn wide(path: &Path) -> Vec<u16> {
    path.as_os_str().encode_wide().collect()
}

/// Returns whether this runner can create a directory symbolic link.
///
/// Windows needs Developer Mode or an elevated process. The probe is a real creation attempt
/// rather than a capability query, because the capability query and the creation call have
/// disagreed in practice.
fn host_permits_symlinks(fixture: &Fixture) -> bool {
    let source = fixture.source("probe-source");
    let probe = fixture.path("probe-link");
    match std::os::windows::fs::symlink_dir(&source, &probe) {
        Ok(()) => {
            fs::remove_dir(&probe).expect("the probe link is removed, not its target");
            true
        }
        Err(error) if is_privilege_failure(&error) => false,
        Err(error) => panic!("the symbolic-link probe failed for an unexpected reason: {error}"),
    }
}

#[test]
fn automatic_mode_prefers_a_symbolic_link_and_falls_back_only_when_privilege_is_missing() {
    let fixture = Fixture::new("windows-auto");
    let permitted = host_permits_symlinks(&fixture);
    let source = fixture.source("skills/rust");
    let staged = fixture.path("skills/staged");

    let created = stage(&source, &staged, LinkMode::Auto).expect("automatic mode succeeds");

    let expected = if permitted {
        CreatedLinkKind::Symlink
    } else {
        CreatedLinkKind::Junction
    };
    assert_eq!(
        created.kind, expected,
        "automatic mode must use a symbolic link wherever one can be created"
    );
    assert_eq!(
        resolve_chain(platform_backend(), &staged)
            .expect("the entry resolves")
            .terminal
            .as_deref(),
        Some(source.as_path())
    );
}

#[test]
fn only_automatic_mode_meeting_a_privilege_failure_falls_back_to_a_junction() {
    let error = |code| io::Error::from_raw_os_error(i32::try_from(code).expect("the code fits"));

    assert!(is_privilege_failure(&error(ERROR_PRIVILEGE_NOT_HELD)));
    assert!(!is_privilege_failure(&error(ERROR_ACCESS_DENIED)));

    assert_eq!(
        classify_symlink_failure(LinkMode::Auto, &error(ERROR_PRIVILEGE_NOT_HELD)),
        SymlinkFailure::FallBackToJunction
    );
    assert_eq!(
        classify_symlink_failure(LinkMode::Symlink, &error(ERROR_PRIVILEGE_NOT_HELD)),
        SymlinkFailure::MissingPrivilege,
        "an explicitly requested symbolic link is never silently downgraded"
    );
    for other in [ERROR_ACCESS_DENIED, ERROR_DISK_FULL] {
        for mode in [LinkMode::Auto, LinkMode::Symlink] {
            assert_eq!(
                classify_symlink_failure(mode, &error(other)),
                SymlinkFailure::Propagate,
                "{mode:?} must keep error {other} rather than turning it into a junction"
            );
        }
    }
}

#[test]
fn a_non_privilege_failure_keeps_its_own_error_and_creates_nothing() {
    let fixture = Fixture::new("windows-other-failure");
    let source = fixture.source("skills/rust");
    // A staging path whose parent does not exist fails for a reason that is not privilege, so no
    // fallback may run.
    let staged = fixture.path("absent-parent/staged");

    let error = stage(&source, &staged, LinkMode::Auto)
        .expect_err("a missing parent is not something to work around");

    assert!(matches!(error, LinkError::Create { .. }));
    assert!(!staged.exists());
    assert!(!staged.parent().expect("a parent component").exists());
}

#[test]
fn an_explicit_junction_resolves_to_its_source_and_is_removed_without_touching_it() {
    let fixture = Fixture::new("windows-junction");
    let source = fixture.source("skills/rust");
    let staged = fixture.path("skills/staged");
    let destination = fixture.path("skills/mounted");
    let backend = platform_backend();

    let created = stage(&source, &staged, LinkMode::Junction).expect("junction creation succeeds");
    assert_eq!(created.kind, CreatedLinkKind::Junction);
    assert_eq!(
        backend
            .inspect_no_follow(&staged)
            .expect("inspection works")
            .kind,
        EntryKind::Junction,
        "a junction must not be reported as a symbolic link"
    );

    let PlacementOutcome::Placed(placed) = backend
        .place_no_replace(&created, &destination)
        .expect("placement succeeds")
    else {
        panic!("an empty destination must accept the staged entry");
    };
    let chain = resolve_chain(backend, &destination).expect("the junction resolves");
    assert_eq!(chain.state, ChainState::LinkToDirectory);
    assert!(
        targets_match(
            chain.terminal.as_deref().expect("a terminal directory"),
            &source
        ),
        "a junction must resolve to the directory it was created for"
    );

    assert_eq!(
        backend
            .remove_link_entry(&placed)
            .expect("removal succeeds"),
        RemoveOutcome::Removed
    );
    assert!(!destination.exists());
    assert_source_intact(&source);
}

#[test]
fn removal_refuses_a_regular_directory_and_never_descends_into_it() {
    let fixture = Fixture::new("windows-refuse-directory");
    let source = fixture.source("skills/rust");
    let occupied = fixture.source("mounted");
    let recorded = CreatedLink {
        path: occupied.clone(),
        kind: CreatedLinkKind::Junction,
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
fn removal_refuses_an_entry_whose_reparse_target_changed() {
    let fixture = Fixture::new("windows-retargeted");
    let source = fixture.source("skills/rust");
    let elsewhere = fixture.source("skills/other");
    let destination = fixture.path("mounted");
    let backend = platform_backend();

    let created = stage(&source, &destination, LinkMode::Junction).expect("creation succeeds");
    assert_eq!(
        backend
            .remove_link_entry(&created)
            .expect("removal succeeds"),
        RemoveOutcome::Removed
    );
    let replacement =
        stage(&elsewhere, &destination, LinkMode::Junction).expect("a replacement is created");

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
    assert_eq!(
        backend
            .remove_link_entry(&replacement)
            .expect("its own owner can still remove it"),
        RemoveOutcome::Removed
    );
    assert_source_intact(&elsewhere);
}

#[test]
fn a_junction_is_ineligible_for_a_unc_source_or_an_occupied_destination() {
    // A network share is not available on a CI runner, so the rule is proved against the values it
    // actually decides on rather than against a share that may or may not exist.
    assert!(
        junction_eligibility(Path::new(r"\\server\share\skills"), EntryKind::Missing).is_err(),
        "a UNC source must never produce a junction"
    );
    assert!(
        junction_eligibility(
            Path::new(r"\\?\UNC\server\share\skills"),
            EntryKind::Missing
        )
        .is_err(),
        "the verbatim spelling of a UNC source is still a UNC source"
    );
    assert!(
        junction_eligibility(Path::new(r"skills\rust"), EntryKind::Missing).is_err(),
        "a relative source names nothing an NT substitute name can point at"
    );
    assert!(junction_eligibility(Path::new(r"C:\Skills\rust"), EntryKind::Missing).is_ok());
    assert!(junction_eligibility(Path::new(r"\\?\C:\Skills\rust"), EntryKind::Missing).is_ok());
    assert!(
        junction_eligibility(Path::new(r"D:\Skills\rust"), EntryKind::Missing).is_ok(),
        "a junction may point at any local drive, not only the one it lives on"
    );

    for occupied in [
        EntryKind::Directory,
        EntryKind::File,
        EntryKind::Symlink,
        EntryKind::Junction,
        EntryKind::Other,
    ] {
        assert!(
            junction_eligibility(Path::new(r"C:\Skills\rust"), occupied).is_err(),
            "{occupied:?} at the destination must not be replaced"
        );
    }
}

#[test]
fn an_unreachable_unc_source_is_refused_and_quoted_back_without_creating_anything() {
    // The eligibility rule itself is proved above, against values rather than against a share no
    // CI runner reliably has. This asserts the surrounding behavior a live request must show: the
    // request fails, the operator's own spelling appears in the message, and the staged path is
    // still empty afterwards.
    let fixture = Fixture::new("windows-unc");
    let unc = PathBuf::from(r"\\localhost\this-share-does-not-exist\skills");
    let staged = fixture.path("staged");

    let error =
        stage(&unc, &staged, LinkMode::Junction).expect_err("a UNC source cannot back a junction");

    let message = error.to_string();
    assert!(matches!(error, LinkError::Create { .. }));
    assert!(
        message.contains("this-share-does-not-exist"),
        "the diagnostic must quote the path the operator supplied: {message}"
    );
    assert!(!staged.exists(), "a refused request leaves nothing behind");
}

#[test]
fn placement_preserves_a_destination_that_appeared_after_staging() {
    let fixture = Fixture::new("windows-placement-conflict");
    let source = fixture.source("skills/rust");
    let staged = fixture.path("staged");
    let destination = fixture.source("mounted");
    let backend = platform_backend();

    let created = stage(&source, &staged, LinkMode::Junction).expect("creation succeeds");

    assert_eq!(
        backend
            .place_no_replace(&created, &destination)
            .expect("placement reports rather than overwrites"),
        PlacementOutcome::DestinationExists
    );
    assert_source_intact(&destination);
    assert_eq!(
        backend.remove_link_entry(&created).expect("rollback works"),
        RemoveOutcome::Removed
    );
    assert_source_intact(&source);
}

#[test]
fn exactly_one_of_two_racing_placements_wins_the_destination() {
    let fixture = Fixture::new("windows-placement-race");
    let source = fixture.source("skills/rust");
    let destination = fixture.path("mounted");
    let backend = platform_backend();

    let staged = (0..2)
        .map(|index| {
            stage(
                &source,
                &fixture.path(&format!("staged-{index}")),
                LinkMode::Junction,
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

    assert_eq!(
        outcomes
            .iter()
            .filter(|outcome| matches!(outcome, PlacementOutcome::Placed(_)))
            .count(),
        1,
        "MoveFileExW without MOVEFILE_REPLACE_EXISTING must let exactly one racer through"
    );
    assert_source_intact(&source);
}

#[test]
fn spaces_japanese_characters_and_long_paths_address_the_intended_entries() {
    let fixture = Fixture::new("windows-native-names");
    let source = fixture.source("Program Files/スキル 集/rust review");
    let staged = fixture.path("staged entry");
    let backend = platform_backend();

    let created = stage(&source, &staged, LinkMode::Junction).expect("creation succeeds");
    assert!(targets_match(&created.source_canonical, &source));
    assert_eq!(
        backend
            .remove_link_entry(&created)
            .expect("removal succeeds"),
        RemoveOutcome::Removed
    );
    assert_source_intact(&source);

    // A path past the legacy 260-character limit, which only resolves because every system call
    // goes out in the extended `\\?\` form.
    //
    // The component count is derived from the fixture root rather than fixed, because that root
    // carries the runner's user name, the process id, and a nanosecond nonce. A fixed count that
    // clears the limit on one runner falls short on another, which is what happened here.
    let component = "長い名前";
    let per_component = component.chars().count() + 1;
    let needed = 320_usize.saturating_sub(wide(&fixture.root).len()) / per_component + 1;
    let deep = fixture.source(&(0..needed).map(|_| component).collect::<Vec<_>>().join("/"));
    assert!(
        wide(&deep).len() > 260,
        "the long-path case must actually be long: {} units over {needed} components",
        wide(&deep).len()
    );
    let long_link = stage(&deep, &fixture.path("long"), LinkMode::Junction)
        .expect("a long source is still a source");
    assert_eq!(
        resolve_chain(backend, &fixture.path("long"))
            .expect("the long junction resolves")
            .state,
        ChainState::LinkToDirectory
    );
    assert_eq!(
        backend
            .remove_link_entry(&long_link)
            .expect("removal succeeds"),
        RemoveOutcome::Removed
    );
    assert_source_intact(&deep);
}

#[test]
fn a_stored_substitute_name_is_reported_raw_and_still_compares_equal_to_the_source() {
    let fixture = Fixture::new("windows-spellings");
    let source = fixture.source("skills/rust");
    let staged = fixture.path("staged");
    let backend = platform_backend();

    let created = stage(&source, &staged, LinkMode::Junction).expect("creation succeeds");
    let stored = backend
        .inspect_no_follow(&staged)
        .expect("inspection works")
        .target
        .expect("a junction has a target");

    assert!(
        stored.raw.to_string_lossy().starts_with(r"\??\"),
        "the raw target is reported exactly as the reparse buffer stores it, got {}",
        stored.raw.display()
    );
    assert!(
        targets_match(&stored.raw, &source),
        "the stored NT-namespace form and the plain path must compare equal"
    );
    assert!(
        targets_match(&stored.resolved, &source),
        "the usable form drops the namespace prefix without changing which directory it names"
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
fn a_relative_substitute_name_resolves_against_the_link_parent() {
    // A relative symbolic link needs the privilege this runner may not have, so the rule is proved
    // against the function that applies it. The stored form is kept exactly; only the usable form
    // is joined onto the parent, and never onto the current directory.
    let link = Path::new(r"C:\project\.agents\skills\rust");
    let target = link_target(
        link,
        &"..\\..\\store\\rust".encode_utf16().collect::<Vec<_>>(),
    );

    assert_eq!(target.raw, Path::new(r"..\..\store\rust"));
    assert!(targets_match(
        &target.resolved,
        Path::new(r"C:\project\store\rust")
    ));

    let absolute = link_target(
        link,
        &r"\??\C:\store\rust".encode_utf16().collect::<Vec<_>>(),
    );
    assert_eq!(
        absolute.raw,
        Path::new(r"\??\C:\store\rust"),
        "the NT namespace prefix is reported as stored"
    );
    assert_eq!(absolute.resolved, Path::new(r"C:\store\rust"));
}

#[test]
fn a_broken_junction_is_still_reported_as_a_link_entry() {
    let fixture = Fixture::new("windows-broken");
    let source = fixture.source("skills/rust");
    let junction = fixture.path("mounted");
    let backend = platform_backend();

    let created = stage(&source, &junction, LinkMode::Junction).expect("creation succeeds");
    fs::remove_file(source.join(SENTINEL)).expect("the sentinel is removed first");
    fs::remove_dir(&source).expect("the target directory is removed out from under the junction");

    let entry = backend
        .inspect_no_follow(&junction)
        .expect("inspection works");
    assert_eq!(
        entry.kind,
        EntryKind::Junction,
        "a dangling junction must stay distinguishable from a missing destination"
    );
    assert_eq!(
        resolve_chain(backend, &junction)
            .expect("resolution reports")
            .state,
        ChainState::Broken
    );
    assert_eq!(
        backend
            .remove_link_entry(&created)
            .expect("its owner can still clean it up"),
        RemoveOutcome::Removed
    );
}

#[test]
fn a_case_variant_is_the_same_entry_only_when_the_volume_says_so() {
    let fixture = Fixture::new("windows-case");
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
        // NTFS is case-insensitive unless per-directory case sensitivity is enabled, so both
        // spellings must report one identity. Comparing paths instead would report two directories.
        EntryKind::Directory => assert_eq!(
            stored_entry.identity, variant_entry.identity,
            "one directory reached by two spellings must have one identity"
        ),
        EntryKind::Missing => assert_eq!(
            stored_entry.kind,
            EntryKind::Directory,
            "with case sensitivity enabled only the stored spelling exists"
        ),
        other => panic!("unexpected classification {other:?}"),
    }
}

#[test]
fn creation_refuses_an_occupied_staging_path() {
    let fixture = Fixture::new("windows-occupied");
    let source = fixture.source("skills/rust");
    let occupied = fixture.source("staged");

    for mode in [LinkMode::Auto, LinkMode::Junction] {
        let error = stage(&source, &occupied, mode)
            .expect_err("an occupied staging path must not be replaced");
        assert!(matches!(error, LinkError::Create { .. }), "{mode:?}");
        assert_source_intact(&occupied);
    }
}

#[test]
fn a_name_with_unpaired_surrogates_is_carried_verbatim_whether_or_not_the_host_accepts_one() {
    use std::ffi::OsString;
    use std::os::windows::ffi::OsStringExt;

    let fixture = Fixture::new("windows-surrogate");
    // An unpaired surrogate is exactly why Rust stores Windows paths as WTF-8 rather than UTF-8.
    // NTFS usually accepts one; both outcomes are asserted rather than one being skipped.
    let source = fixture.root.join(OsString::from_wide(&[
        0xD800,
        u16::from(b's'),
        u16::from(b'k'),
    ]));
    let backend = platform_backend();

    if fs::create_dir(&source).is_err() {
        let entry = backend
            .inspect_no_follow(&source)
            .expect("an unrepresentable name is still a path that can be looked at");
        assert_eq!(entry.kind, EntryKind::Missing);
        assert_eq!(
            entry.path, source,
            "the reported path is unit-for-unit the one that was passed in"
        );
        return;
    }

    fs::write(source.join(SENTINEL), "sentinel").expect("the sentinel is written");
    let staged = fixture.path("staged");
    let created = stage(&source, &staged, LinkMode::Junction).expect("creation succeeds");
    assert!(targets_match(&created.source_canonical, &source));
    assert_eq!(
        backend
            .remove_link_entry(&created)
            .expect("removal succeeds"),
        RemoveOutcome::Removed
    );
    assert_source_intact(&source);
}
