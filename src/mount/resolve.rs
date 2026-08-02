//! No-follow entry classification for read-only planning.
//!
//! The walk itself lives in [`crate::link::resolve`], which drives the platform backend. This
//! module is the planning-facing view of that walk: it collapses the backend's states into the
//! smaller vocabulary planning reasons about, and it maps the backend's errors back onto the
//! wrapper exit categories a caller already depends on.

use std::path::{Path, PathBuf};

use crate::error::{AppError, LinkError};
use crate::link::resolve::{ChainState, resolve_chain};

pub use crate::link::resolve::MAX_LINK_DEPTH;

/// How an entry appears when it is inspected without implicitly following it.
///
/// Windows junctions and POSIX symbolic links are deliberately not distinguished here. Both are
/// directory indirections, both are reported by `symlink_metadata` as links, and every rule in
/// the V2 design that mentions them treats them identically. The link *implementation* only
/// matters when one is created, which is a later change.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PathKind {
    /// Nothing exists at the path.
    Missing,
    /// A regular directory.
    Directory,
    /// A directory link whose chain terminates in a directory.
    DirectoryLink,
    /// A link chain that reaches a path which does not exist.
    BrokenLink,
    /// A link chain that revisits an earlier hop.
    CyclicLink,
    /// A link chain longer than [`MAX_LINK_DEPTH`].
    DepthExceeded,
    /// An entry that cannot serve as a directory, such as a regular file.
    NotDirectory,
}

impl PathKind {
    /// Returns whether the entry currently holds, or could hold, Skills.
    #[must_use]
    pub const fn is_usable_namespace(self) -> bool {
        matches!(self, Self::Missing | Self::Directory | Self::DirectoryLink)
    }

    /// Returns whether the entry is an unresolvable state that planning must not step over.
    #[must_use]
    pub const fn is_ambiguous(self) -> bool {
        matches!(
            self,
            Self::BrokenLink | Self::CyclicLink | Self::DepthExceeded | Self::NotDirectory
        )
    }

    /// Returns the stable label used in read-only output and diagnostics.
    #[must_use]
    pub const fn label(self) -> &'static str {
        match self {
            Self::Missing => "missing",
            Self::Directory => "regular directory",
            Self::DirectoryLink => "directory link",
            Self::BrokenLink => "broken link",
            Self::CyclicLink => "link cycle",
            Self::DepthExceeded => "link chain deeper than the supported maximum",
            Self::NotDirectory => "non-directory entry",
        }
    }
}

/// The observed state of one entry, including any link chain it travels.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ResolvedEntry {
    /// Visible path that was classified.
    pub entry: PathBuf,
    /// Classification of the visible entry.
    pub kind: PathKind,
    /// Each link target in traversal order, exactly as stored on disk.
    pub link_chain: Vec<PathBuf>,
    /// Canonical terminal directory, present only for a directory or a resolvable directory link.
    pub terminal: Option<PathBuf>,
}

impl ResolvedEntry {
    /// Returns whether this entry and `other` resolve to the same terminal directory.
    ///
    /// Two unresolvable entries are never equal: an unresolvable state carries no identity that a
    /// later mutation could rely on.
    #[must_use]
    pub fn shares_terminal_with(&self, other: &Self) -> bool {
        match (&self.terminal, &other.terminal) {
            (Some(left), Some(right)) => left == right,
            _ => false,
        }
    }
}

/// Classifies an entry and resolves any directory-link chain.
///
/// Broken, cyclic, over-deep, and non-directory layouts are returned as states rather than
/// errors. The V2 design only mandates failure for the *authoritative* Codex entry and for a
/// conflicting destination; an unusable ancestor scope has a different consequence. Returning the
/// state lets each caller apply its own rule instead of forcing one policy into the resolver.
///
/// # Errors
///
/// Returns [`AppError::MissingInput`] when the operating system reports a failure other than a
/// missing path, such as a permission error, or when a resolved directory cannot be canonicalized.
pub fn classify(entry: &Path) -> Result<ResolvedEntry, AppError> {
    let chain = resolve_chain(crate::link::platform_backend(), entry).map_err(planning_error)?;
    let kind = match chain.state {
        ChainState::Missing => PathKind::Missing,
        ChainState::Directory => PathKind::Directory,
        ChainState::LinkToDirectory => PathKind::DirectoryLink,
        ChainState::Broken => PathKind::BrokenLink,
        ChainState::Cyclic => PathKind::CyclicLink,
        ChainState::DepthExceeded => PathKind::DepthExceeded,
        ChainState::Unsupported => PathKind::NotDirectory,
    };
    Ok(ResolvedEntry {
        entry: entry.to_path_buf(),
        kind,
        link_chain: chain.hops.into_iter().map(|hop| hop.raw).collect(),
        terminal: chain.terminal,
    })
}

/// Maps a backend failure onto the exit category planning already reports.
///
/// An entry the host refuses to describe is a missing-input failure at this layer, exactly as it
/// was when this module called `std::fs` directly. Only the failures that cannot arise during
/// read-only classification keep the backend's own category.
fn planning_error(error: LinkError) -> AppError {
    match error {
        LinkError::Inspect { path, reason } => AppError::MissingInput { path, reason },
        other => AppError::Link(other),
    }
}

#[cfg(test)]
mod tests {
    use super::{MAX_LINK_DEPTH, PathKind, classify};
    use crate::test_support::{TestDir, symlink_dir_or_skip};
    use std::path::Path;

    #[test]
    fn absent_and_ordinary_entries_are_classified_without_following() {
        let fixture = TestDir::new("classify-basic");
        let directory = fixture.dir("store");
        let file = fixture.file("regular.txt", "not a namespace");

        assert_eq!(
            classify(&fixture.path().join("nothing-here")).unwrap().kind,
            PathKind::Missing
        );
        let resolved = classify(&directory).unwrap();
        assert_eq!(resolved.kind, PathKind::Directory);
        assert_eq!(
            resolved.terminal,
            Some(std::fs::canonicalize(&directory).unwrap())
        );
        assert!(resolved.link_chain.is_empty());
        assert_eq!(classify(&file).unwrap().kind, PathKind::NotDirectory);
    }

    #[test]
    fn relative_and_absolute_single_hop_links_reach_the_same_terminal() {
        let fixture = TestDir::new("classify-single-hop");
        let store = fixture.dir("store");
        let relative = fixture.path().join("relative-link");
        let absolute = fixture.path().join("absolute-link");
        if !symlink_dir_or_skip(Path::new("store"), &relative) {
            return;
        }
        assert!(symlink_dir_or_skip(&store, &absolute));

        let canonical = std::fs::canonicalize(&store).unwrap();
        for link in [&relative, &absolute] {
            let resolved = classify(link).unwrap();
            assert_eq!(resolved.kind, PathKind::DirectoryLink, "{}", link.display());
            assert_eq!(resolved.terminal, Some(canonical.clone()));
            assert_eq!(resolved.link_chain.len(), 1);
        }
    }

    #[test]
    fn a_relative_multi_hop_chain_records_every_target() {
        let fixture = TestDir::new("classify-multi-hop");
        let store = fixture.dir("nested/store");
        let middle = fixture.path().join("middle");
        let entry = fixture.path().join("entry");
        if !symlink_dir_or_skip(Path::new("nested/store"), &middle) {
            return;
        }
        assert!(symlink_dir_or_skip(Path::new("middle"), &entry));

        let resolved = classify(&entry).unwrap();

        assert_eq!(resolved.kind, PathKind::DirectoryLink);
        assert_eq!(
            resolved.terminal,
            Some(std::fs::canonicalize(store).unwrap())
        );
        assert_eq!(
            resolved.link_chain,
            [Path::new("middle"), Path::new("nested/store")],
            "each hop is recorded exactly as stored on disk"
        );
    }

    #[test]
    fn broken_cyclic_and_over_deep_chains_are_states_rather_than_panics() {
        let fixture = TestDir::new("classify-unresolvable");
        let broken = fixture.path().join("broken");
        if !symlink_dir_or_skip(Path::new("does-not-exist"), &broken) {
            return;
        }
        assert_eq!(classify(&broken).unwrap().kind, PathKind::BrokenLink);

        let first = fixture.path().join("cycle-a");
        let second = fixture.path().join("cycle-b");
        assert!(symlink_dir_or_skip(Path::new("cycle-b"), &first));
        assert!(symlink_dir_or_skip(Path::new("cycle-a"), &second));
        assert_eq!(classify(&first).unwrap().kind, PathKind::CyclicLink);

        let store = fixture.dir("deep-store");
        let mut previous = store;
        for index in 0..=MAX_LINK_DEPTH {
            let link = fixture.path().join(format!("hop-{index}"));
            assert!(symlink_dir_or_skip(&previous, &link));
            previous = link;
        }
        assert_eq!(classify(&previous).unwrap().kind, PathKind::DepthExceeded);
    }

    #[test]
    fn a_link_to_a_regular_file_is_not_a_usable_namespace() {
        let fixture = TestDir::new("classify-file-link");
        let file = fixture.file("target.txt", "contents");
        let link = fixture.path().join("entry");
        if !symlink_dir_or_skip(&file, &link) {
            return;
        }

        let resolved = classify(&link).unwrap();

        assert_eq!(resolved.kind, PathKind::NotDirectory);
        assert!(resolved.terminal.is_none());
        assert!(resolved.kind.is_ambiguous());
        assert!(!resolved.kind.is_usable_namespace());
    }

    #[test]
    fn two_routes_to_one_directory_share_a_terminal() {
        let fixture = TestDir::new("classify-shared-terminal");
        let store = fixture.dir("store");
        let link = fixture.path().join("alias");
        if !symlink_dir_or_skip(&store, &link) {
            return;
        }

        assert!(
            classify(&store)
                .unwrap()
                .shares_terminal_with(&classify(&link).unwrap())
        );
    }

    #[test]
    fn unresolvable_entries_never_share_a_terminal_with_anything() {
        let fixture = TestDir::new("classify-no-identity");
        let missing = classify(&fixture.path().join("absent")).unwrap();

        assert!(!missing.shares_terminal_with(&missing));
    }
}
