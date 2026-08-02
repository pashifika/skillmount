//! The platform boundary that inspects, creates, places, and removes directory link entries.
//!
//! Everything above this module reasons about *abstract* create/reuse actions. This module is the
//! only place that touches a real link, and it makes exactly one promise: a caller can never ask
//! it to delete a directory tree. There is no recursive removal operation, and the only removal
//! that exists takes recorded ownership evidence and refuses anything that does not match it.
//!
//! The trait is sealed. A backend must be able to prove its own platform semantics with native
//! tests, so third-party implementations would silently widen a contract whose whole value is that
//! it is narrow.
//!
//! Applying a mount plan, journaling, and recovery remain outside this module: it exposes the
//! primitives a transaction needs and takes no policy decision about when to use them.

pub mod resolve;

#[cfg(any(windows, test))]
mod reparse;
#[cfg(test)]
pub(crate) mod testing;
#[cfg(test)]
mod tests;
#[cfg(unix)]
mod unix;
#[cfg(unix)]
mod unix_ffi;
#[cfg(all(unix, test))]
mod unix_tests;
#[cfg(windows)]
mod windows;
#[cfg(windows)]
mod windows_ffi;
#[cfg(all(windows, test))]
mod windows_tests;
#[cfg(any(windows, test))]
mod winpath;

use std::fmt;
use std::path::{Path, PathBuf};

use crate::domain::LinkMode;
use crate::error::LinkError;

/// Restricts [`LinkBackend`] implementations to this crate.
mod sealed {
    /// Private supertrait that cannot be named outside the crate.
    pub trait Sealed {}
}

/// How an entry appears when it is inspected without following it.
///
/// A symbolic link and a Windows junction are distinguished here, unlike in
/// [`crate::mount::resolve::PathKind`], because the *implementation* matters once ownership has to
/// be proved: removing a junction and removing a symbolic link verify different evidence.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum EntryKind {
    /// Nothing exists at the path.
    Missing,
    /// A regular directory that is not a link.
    Directory,
    /// A regular file.
    File,
    /// A symbolic link, whatever its target turns out to be.
    Symlink,
    /// A Windows mount-point reparse point.
    Junction,
    /// An entry that cannot back a Skill namespace, such as a device node, a socket, or a reparse
    /// point with a tag this backend does not implement.
    Other,
}

impl EntryKind {
    /// Returns the stable label used in diagnostics.
    #[must_use]
    pub const fn label(self) -> &'static str {
        match self {
            Self::Missing => "missing",
            Self::Directory => "regular directory",
            Self::File => "regular file",
            Self::Symlink => "symbolic link",
            Self::Junction => "junction",
            Self::Other => "unsupported entry",
        }
    }
}

/// The link implementation an entry was created with.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CreatedLinkKind {
    /// A directory symbolic link.
    Symlink,
    /// A Windows junction.
    Junction,
}

impl CreatedLinkKind {
    /// Returns the entry kind this implementation appears as when inspected.
    #[must_use]
    pub const fn entry_kind(self) -> EntryKind {
        match self {
            Self::Symlink => EntryKind::Symlink,
            Self::Junction => EntryKind::Junction,
        }
    }

    /// Returns the stable label used in diagnostics and in the future journal.
    #[must_use]
    pub const fn label(self) -> &'static str {
        match self {
            Self::Symlink => "symlink",
            Self::Junction => "junction",
        }
    }
}

/// The immediate target of one link entry.
///
/// The raw value is kept because it is the only thing a later removal can compare against without
/// touching the target, and because a diagnostic that rewrites what is stored on disk hides the
/// very layout the operator has to fix.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LinkTarget {
    /// The target exactly as stored, including any Windows namespace prefix.
    pub raw: PathBuf,
    /// `raw` made usable: namespace prefix removed, and resolved against the link's parent when
    /// it is relative. Never canonicalized, because the target may not exist.
    pub resolved: PathBuf,
}

/// An opaque platform identity for an entry that exists.
///
/// The representation is private so each platform can strengthen its derivation without changing
/// the contract. Only equality, ordering, and hashing are meaningful.
///
/// Unix uses the device and inode pair. Windows prefers the 128-bit `FILE_ID_INFO` identity and
/// falls back to the legacy volume-serial and 64-bit index pair, both read from a handle opened
/// without traversing the entry. The preference matters: Microsoft documents the legacy index as
/// neither guaranteed unique on `ReFS` nor safe from reuse after a deletion, so on such a volume it
/// is not on its own a proof that an entry is the one this process created.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct PlatformIdentity(String);

impl PlatformIdentity {
    /// Builds an identity from a volume and a fixed-width identifier.
    ///
    /// `derivation` distinguishes identities read different ways, so a value taken from a 64-bit
    /// index never compares equal to one taken from a 128-bit identifier even if the digits
    /// coincide. Two readings that disagree that far describe an entry this process cannot prove it
    /// owns, and refusing is the safe answer.
    pub(crate) fn new(derivation: &str, volume: u64, identifier: &[u8]) -> Self {
        use std::fmt::Write as _;

        let mut rendered = format!("{derivation}:{volume:x}:");
        for byte in identifier {
            let _ = write!(rendered, "{byte:02x}");
        }
        Self(rendered)
    }

    /// Builds an identity from the platform's pair of stable numbers.
    pub(crate) fn from_pair(derivation: &str, volume: u64, index: u64) -> Self {
        Self::new(derivation, volume, &index.to_be_bytes())
    }
}

impl fmt::Display for PlatformIdentity {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(&self.0)
    }
}

/// One entry as observed without following it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PathEntry {
    /// The path exactly as the caller supplied it.
    pub path: PathBuf,
    /// Classification of the visible entry, never of its target.
    pub kind: EntryKind,
    /// Immediate target, present only for a link entry.
    pub target: Option<LinkTarget>,
    /// Platform identity, present only when the entry exists and the host reports one.
    pub identity: Option<PlatformIdentity>,
}

impl PathEntry {
    /// Builds an entry that carries no target and no identity.
    pub(crate) fn plain(path: &Path, kind: EntryKind) -> Self {
        Self {
            path: path.to_path_buf(),
            kind,
            target: None,
            identity: None,
        }
    }
}

/// What a caller asks the backend to create.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LinkRequest {
    /// Directory the link must refer to. Callers pass a canonical path.
    pub source: PathBuf,
    /// Transaction-unique sibling the entry is created at, before placement.
    pub staged_path: PathBuf,
    /// Requested implementation. [`LinkMode::Auto`] is resolved by the backend, because the
    /// Windows fallback depends on privilege that is only observable at run time.
    pub mode: LinkMode,
}

/// Ownership evidence for a link entry this process created.
///
/// A later cleanup removes an entry only when the live entry still matches every field recorded
/// here. Recording the canonical source *and* the raw target *and* the identity is deliberate. The
/// raw target still describes the entry after its target has disappeared, which keeps a dangling
/// link removable; the canonical source is what diagnostics quote. But the identity is the only
/// field that distinguishes this process's entry from an identical one someone else created, so an
/// entry recorded without one is never removed and reports
/// [`OwnershipMismatch::IdentityUnavailable`] instead.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CreatedLink {
    /// Path the entry currently occupies: the staged sibling until it is placed.
    pub path: PathBuf,
    /// Implementation the entry was created with.
    pub kind: CreatedLinkKind,
    /// Target exactly as written to the entry.
    pub target: PathBuf,
    /// Canonical directory the entry refers to.
    pub source_canonical: PathBuf,
    /// Platform identity captured at creation, when the host reports one.
    pub identity: Option<PlatformIdentity>,
}

impl CreatedLink {
    /// Returns the same evidence recorded against `path`.
    ///
    /// A rename moves the directory entry without changing what it points at or which inode it
    /// occupies, so every other field survives placement unchanged.
    #[must_use]
    pub fn relocated_to(&self, path: &Path) -> Self {
        Self {
            path: path.to_path_buf(),
            ..self.clone()
        }
    }
}

/// The result of placing a staged entry at its destination.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PlacementOutcome {
    /// The entry now occupies the destination; the value carries the relocated evidence.
    Placed(CreatedLink),
    /// A destination appeared after staging. Nothing was replaced and the staged entry is still
    /// present, so the caller can roll it back with verified removal.
    DestinationExists,
}

/// Why a recorded entry is no longer this process's to remove.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum OwnershipMismatch {
    /// The path now holds a regular directory.
    RegularDirectory,
    /// The path now holds something that is not a directory link.
    NotALink,
    /// The link implementation differs from the recorded one.
    KindChanged,
    /// The link points somewhere other than the recorded target.
    TargetChanged,
    /// The entry has a different platform identity than the recorded one.
    IdentityChanged,
    /// One of the two identities is missing, so ownership cannot be established.
    IdentityUnavailable,
}

impl OwnershipMismatch {
    /// Returns the stable label used in diagnostics.
    #[must_use]
    pub const fn label(self) -> &'static str {
        match self {
            Self::RegularDirectory => "a regular directory replaced the entry",
            Self::NotALink => "the entry is no longer a directory link",
            Self::KindChanged => "the link implementation changed",
            Self::TargetChanged => "the link points somewhere else",
            Self::IdentityChanged => "the entry was replaced by a different one",
            Self::IdentityUnavailable => "the entry cannot be proved to be the recorded one",
        }
    }
}

/// The result of removing a recorded link entry.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RemoveOutcome {
    /// The recorded entry was verified and unlinked. Its target was not touched.
    Removed,
    /// Nothing exists at the recorded path, so there is nothing to undo.
    AlreadyAbsent,
    /// The live entry does not match the recorded evidence and was left exactly as it is.
    OwnershipMismatch(OwnershipMismatch),
}

/// The narrow filesystem contract the transaction layer builds on.
///
/// Every method addresses one entry. None of them accepts a recursion depth, a glob, or a
/// directory tree, which is what makes "`SkillMount` never deletes your Skills" a property of the
/// interface rather than a claim about its callers.
///
/// A backend holds no state between calls, so it is `Send + Sync`: the transaction layer applies
/// independent actions concurrently, and each call already carries everything it needs.
pub trait LinkBackend: sealed::Sealed + Send + Sync {
    /// Classifies `path` without following it.
    ///
    /// A missing path is a successful classification, not an error: a caller planning a new mount
    /// and a caller verifying an old one both need to tell "nothing is there" apart from "I could
    /// not look".
    ///
    /// # Errors
    ///
    /// Returns [`LinkError::Inspect`] when the host reports a failure other than a missing path.
    fn inspect_no_follow(&self, path: &Path) -> Result<PathEntry, LinkError>;

    /// Resolves an existing directory to the canonical path that identifies it.
    ///
    /// This is the one operation that deliberately follows links, because "which directory is
    /// this really" is the question it answers. The result is whatever the platform's own
    /// canonical form is, including the verbatim `\\?\` prefix on Windows, so that a canonical
    /// path from this backend and one from anywhere else in the crate are the same value.
    /// Namespace differences are folded by [`resolve::ComparablePath`] at comparison time.
    ///
    /// # Errors
    ///
    /// Returns [`LinkError::Inspect`] when the path cannot be resolved or is not a directory.
    fn canonical_directory(&self, path: &Path) -> Result<PathBuf, LinkError>;

    /// Creates a directory link at the request's staged path.
    ///
    /// The staged path must not exist. Creation never replaces an entry and never copies the
    /// source, so a request that cannot be satisfied fails instead of degrading.
    ///
    /// # Errors
    ///
    /// Returns [`LinkError::Create`] when the requested implementation is unavailable, ineligible,
    /// or rejected by the host, and [`LinkError::Unsupported`] when the requested implementation
    /// does not exist on this platform.
    fn create_directory_link(&self, request: &LinkRequest) -> Result<CreatedLink, LinkError>;

    /// Atomically moves a staged entry onto `destination` without replacing anything.
    ///
    /// # Errors
    ///
    /// Returns [`LinkError::Place`] when the host reports a failure other than an occupied
    /// destination, and [`LinkError::Unsupported`] when the host cannot guarantee no-replace
    /// semantics. The guarantee is never emulated with a separate existence check.
    fn place_no_replace(
        &self,
        staged: &CreatedLink,
        destination: &Path,
    ) -> Result<PlacementOutcome, LinkError>;

    /// Removes exactly the recorded link entry after verifying it is still the recorded one.
    ///
    /// Removal unlinks one directory entry. It never descends into the target, and it refuses a
    /// regular directory outright.
    ///
    /// # Errors
    ///
    /// Returns [`LinkError::Inspect`] when the entry cannot be classified and
    /// [`LinkError::Remove`] when the verified entry cannot be unlinked.
    fn remove_link_entry(&self, recorded: &CreatedLink) -> Result<RemoveOutcome, LinkError>;
}

/// Returns the backend for the host platform.
#[cfg(unix)]
#[must_use]
pub fn platform_backend() -> &'static dyn LinkBackend {
    &unix::UnixBackend
}

/// Returns the backend for the host platform.
#[cfg(windows)]
#[must_use]
pub fn platform_backend() -> &'static dyn LinkBackend {
    &windows::WindowsBackend
}

/// What a live entry turned out to be, relative to recorded ownership evidence.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Ownership {
    /// The live entry is still the recorded one and may be unlinked.
    Owned,
    /// Nothing exists at the recorded path.
    Absent,
    /// Something else occupies the path.
    Mismatch(OwnershipMismatch),
}

/// Decides whether a live entry still matches recorded ownership evidence.
///
/// Shared by both backends so the two platforms cannot drift apart on what "still mine" means.
/// `target_matches` is supplied by the caller because comparing raw targets is the one genuinely
/// platform-specific part: Windows has to normalize namespace forms first.
pub(crate) fn verify_ownership(
    live: &PathEntry,
    recorded: &CreatedLink,
    target_matches: impl FnOnce(&LinkTarget) -> bool,
) -> Ownership {
    match live.kind {
        EntryKind::Missing => Ownership::Absent,
        EntryKind::Directory => Ownership::Mismatch(OwnershipMismatch::RegularDirectory),
        EntryKind::File | EntryKind::Other => Ownership::Mismatch(OwnershipMismatch::NotALink),
        EntryKind::Symlink | EntryKind::Junction => {
            if live.kind != recorded.kind.entry_kind() {
                return Ownership::Mismatch(OwnershipMismatch::KindChanged);
            }
            // Identity is checked before the target: an entry someone else recreated pointing at
            // the same directory is still not the entry this process created, and only the
            // identity can tell those apart.
            //
            // A missing identity on either side is therefore a refusal, not a skipped check.
            // Falling through to the target alone would remove exactly the recreated entry the
            // previous paragraph is about. Leaving an entry behind is recoverable — a later
            // cleanup, or the operator, can remove it once its identity reads again — while
            // removing someone else's is not, so the unprovable case fails closed.
            match (live.identity.as_ref(), recorded.identity.as_ref()) {
                (Some(live_identity), Some(recorded_identity)) => {
                    if live_identity != recorded_identity {
                        return Ownership::Mismatch(OwnershipMismatch::IdentityChanged);
                    }
                }
                _ => return Ownership::Mismatch(OwnershipMismatch::IdentityUnavailable),
            }
            match live.target.as_ref() {
                Some(target) if target_matches(target) => Ownership::Owned,
                _ => Ownership::Mismatch(OwnershipMismatch::TargetChanged),
            }
        }
    }
}
