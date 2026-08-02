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

/// Replaces one file and waits for the Windows namespace update to reach disk.
///
/// Kept as a crate-only wrapper so the raw Win32 call stays inside the audited FFI module while
/// the journal store can use its durability guarantee.
#[cfg(windows)]
pub(crate) fn replace_file_write_through(from: &Path, to: &Path) -> std::io::Result<()> {
    windows_ffi::replace_file_write_through(from, to)
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

/// The link implementation recorded for an entry.
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

    /// Returns the stable label used in diagnostics and in the transaction journal.
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
/// is not on its own reliable evidence that a live entry is still the recorded one.
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

    /// Rebuilds an identity from a value a journal recorded earlier.
    ///
    /// The rendering is opaque and is never interpreted, only compared, so a value written by an
    /// older run of the same platform stays usable without a migration. A value produced by a
    /// different derivation simply never compares equal, which is the conservative outcome.
    pub(crate) fn from_recorded(value: &str) -> Self {
        Self(value.to_owned())
    }

    /// Returns the stable rendering, for hashing into a lock key or writing to a journal.
    pub(crate) fn as_str(&self) -> &str {
        &self.0
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

/// Ownership evidence for the link entry recorded at the initial evidence boundary.
///
/// Cleanup requests removal only after the inspected entry matches every field recorded here.
/// Windows retains the no-follow handle that supplied those fields through removal; Unix rechecks
/// the pathname immediately before unlink under cooperating-session locks, with the residual
/// non-cooperating race recorded in ADR 0014. Windows can exclude ordinary writers but not the
/// attribute-only access that may mutate reparse metadata, so ADR 0016 makes identity the authority
/// for that retained object after the eligibility check. Recording the canonical source *and* the
/// raw target *and* the identity is deliberate. The raw target still describes the entry after its
/// target has disappeared, which keeps a dangling link removable; the canonical source is what
/// diagnostics quote. But the identity is the only field that distinguishes the recorded entry from
/// an identical later replacement, so an entry recorded without one is never requested for removal
/// and reports [`OwnershipMismatch::IdentityUnavailable`] instead.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CreatedLink {
    /// Path the entry currently occupies: the staged sibling until it is placed.
    pub path: PathBuf,
    /// Implementation observed at the initial evidence boundary.
    pub kind: CreatedLinkKind,
    /// Target exactly as written to the entry.
    pub target: PathBuf,
    /// Canonical directory the entry refers to.
    pub source_canonical: PathBuf,
    /// Platform identity captured at the initial evidence boundary, when the host reports one.
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

/// Ownership evidence for the directory recorded at the initial evidence boundary.
///
/// A helper directory gets the same treatment as a link, and for the same reason: "it is empty" is
/// not proof that it is *this* transaction's directory. A user who removed the mounts by hand and
/// left the store behind would otherwise have their directory deleted by a cleanup that only
/// checked emptiness.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct OwnedDirectory {
    /// Path the directory currently occupies.
    pub path: PathBuf,
    /// Platform identity captured at the initial evidence boundary, when the host reports one.
    pub identity: Option<PlatformIdentity>,
}

impl OwnedDirectory {
    /// Returns the same evidence recorded against `path`.
    #[must_use]
    pub fn relocated_to(&self, path: &Path) -> Self {
        Self {
            path: path.to_path_buf(),
            identity: self.identity.clone(),
        }
    }
}

/// Why placement could not prove that it still addressed the recorded staged entry.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PlacementMismatch {
    /// The recorded staged entry disappeared before the operation could establish ownership.
    Missing,
    /// A live entry exists, but it does not match the recorded evidence.
    Ownership(OwnershipMismatch),
    /// The destination could not be inspected after pathname placement completed.
    InspectionFailed(String),
}

impl PlacementMismatch {
    /// Returns the stable label used in diagnostics.
    #[must_use]
    pub fn label(&self) -> &str {
        match self {
            Self::Missing => "the recorded staged entry is missing",
            Self::Ownership(mismatch) => mismatch.label(),
            Self::InspectionFailed(reason) => reason,
        }
    }
}

/// An entry placement left untouched because ownership could not be established.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PlacementResidue {
    /// Path whose current entry was retained.
    pub path: PathBuf,
    /// Evidence mismatch that prevented the backend from claiming it.
    pub mismatch: PlacementMismatch,
}

/// The result of placing a staged entry at its destination.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PlacementOutcome<T> {
    /// The entry now occupies the destination; the value carries the relocated evidence.
    Placed(T),
    /// A destination appeared after staging. Nothing was replaced, and the staged pathname remains
    /// available for ownership-verified rollback.
    DestinationExists,
    /// The staged or final entry did not match the recorded evidence and was left untouched.
    OwnershipMismatch(PlacementResidue),
}

/// Why a recorded entry is no longer this process's to remove.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum OwnershipMismatch {
    /// The path now holds a regular directory.
    RegularDirectory,
    /// The path now holds something that is not a directory link.
    NotALink,
    /// The path no longer holds a regular directory.
    NotADirectory,
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
            Self::NotADirectory => "the entry is no longer a regular directory",
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
    /// The verified directory holds entries this transaction did not put there.
    ///
    /// Distinct from an ownership mismatch: the directory *is* the recorded one, and it is still
    /// not removable, because removing it would take the contents with it.
    NotEmpty,
}

/// The narrow filesystem contract the transaction layer builds on.
///
/// Every method addresses one entry. None of them accepts a recursion depth, a glob, or a
/// directory tree, which is what makes "`SkillMount` never deletes your Skills" a property of the
/// interface rather than a claim about its callers.
///
/// A backend holds no state between calls and is exposed as a process-global instance, so it is
/// `Send + Sync`; each call already carries everything it needs.
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

    /// Creates one empty directory at `path`, which must not already exist.
    ///
    /// The entry is inspected before it is returned, so the caller receives identity for the
    /// object observed at the initial evidence boundary. The supported create APIs return status,
    /// not an object capability, so this does not claim continuity across the create-to-observation
    /// window; ADR 0015 records that residual scope.
    ///
    /// # Errors
    ///
    /// Returns [`LinkError::Create`] when the path is occupied or the host refuses the creation.
    fn create_directory(&self, path: &Path) -> Result<OwnedDirectory, LinkError>;

    /// Atomically moves a staged link onto `destination` without replacing anything.
    ///
    /// The operation consumes recorded ownership evidence, verifies that the staged entry still
    /// matches it, and returns evidence for the object established at the destination. A backend
    /// must never expose its raw path-rename primitive through this contract.
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
    ) -> Result<PlacementOutcome<CreatedLink>, LinkError>;

    /// Atomically moves a staged directory onto `destination` without replacing anything.
    ///
    /// The operation has the same evidence and no-replace requirements as
    /// [`LinkBackend::place_no_replace`], but returns relocated directory evidence.
    ///
    /// # Errors
    ///
    /// Returns [`LinkError::Place`] when the host reports a failure other than contention, and
    /// [`LinkError::Inspect`] when pre-placement ownership cannot be inspected.
    fn place_directory_no_replace(
        &self,
        staged: &OwnedDirectory,
        destination: &Path,
    ) -> Result<PlacementOutcome<OwnedDirectory>, LinkError>;

    /// Removes one link entry after it matches the recorded evidence.
    ///
    /// Removal never descends into the target and refuses a regular directory outright. Windows
    /// verifies and disposes the same no-follow handle; ADR 0016 scopes mutable reparse metadata
    /// after that check without weakening the retained object's identity authority. Unix performs a
    /// final no-follow check and pathname unlink while product callers hold cooperative locks; ADR
    /// 0014 records the remaining race with processes that do not honor those advisory locks.
    ///
    /// # Errors
    ///
    /// Returns [`LinkError::Inspect`] when the entry cannot be classified and
    /// [`LinkError::Remove`] when the verified entry cannot be unlinked.
    fn remove_link_entry(&self, recorded: &CreatedLink) -> Result<RemoveOutcome, LinkError>;

    /// Removes the recorded helper directory, and only while it is still empty.
    ///
    /// Emptiness is enforced by the operating system rather than checked first: the removal call
    /// refuses a directory with contents, so there is no window in which something could appear
    /// between the check and the removal. Windows binds the check and disposition to one handle;
    /// Unix rechecks the pathname at its last available boundary. Together with the identity
    /// comparison this keeps "`SkillMount` never deletes your Skills" true for directories as well
    /// as links — there is still no recursive removal anywhere on this interface.
    ///
    /// # Errors
    ///
    /// Returns [`LinkError::Inspect`] when the entry cannot be classified and
    /// [`LinkError::Remove`] when the verified directory cannot be removed for a reason other than
    /// holding entries.
    fn remove_empty_directory(&self, recorded: &OwnedDirectory)
    -> Result<RemoveOutcome, LinkError>;
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
            // Identity is checked before the target: an entry replaced after evidence was recorded
            // may point at the same directory but is still not the recorded entry, and only the
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

/// Decides whether a live entry is still the directory this transaction recorded.
///
/// The identity rule is the same one links use and is here for the same reason: a directory that
/// someone recreated at the same path with the same name is a different directory, and only the
/// identity distinguishes them. A missing identity on either side therefore refuses, which leaves
/// harmless residue instead of removing a directory this process cannot match to its evidence.
pub(crate) fn verify_directory_ownership(live: &PathEntry, recorded: &OwnedDirectory) -> Ownership {
    match live.kind {
        EntryKind::Missing => Ownership::Absent,
        EntryKind::Symlink | EntryKind::Junction => {
            Ownership::Mismatch(OwnershipMismatch::KindChanged)
        }
        EntryKind::File | EntryKind::Other => Ownership::Mismatch(OwnershipMismatch::NotADirectory),
        EntryKind::Directory => match (live.identity.as_ref(), recorded.identity.as_ref()) {
            (Some(live_identity), Some(recorded_identity)) => {
                if live_identity == recorded_identity {
                    Ownership::Owned
                } else {
                    Ownership::Mismatch(OwnershipMismatch::IdentityChanged)
                }
            }
            _ => Ownership::Mismatch(OwnershipMismatch::IdentityUnavailable),
        },
    }
}

/// Returns the placement mismatch for a live link entry, or `None` when ownership is proved.
pub(crate) fn link_placement_mismatch(
    live: &PathEntry,
    recorded: &CreatedLink,
    target_matches: impl FnOnce(&LinkTarget) -> bool,
) -> Option<PlacementMismatch> {
    placement_mismatch(verify_ownership(live, recorded, target_matches))
}

/// Returns the placement mismatch for a live directory, or `None` when ownership is proved.
pub(crate) fn directory_placement_mismatch(
    live: &PathEntry,
    recorded: &OwnedDirectory,
) -> Option<PlacementMismatch> {
    placement_mismatch(verify_directory_ownership(live, recorded))
}

fn placement_mismatch(ownership: Ownership) -> Option<PlacementMismatch> {
    match ownership {
        Ownership::Owned => None,
        Ownership::Absent => Some(PlacementMismatch::Missing),
        Ownership::Mismatch(mismatch) => Some(PlacementMismatch::Ownership(mismatch)),
    }
}

/// Unix implementation of [`LinkBackend::create_directory`].
///
/// `create_dir` fails when the path exists, which is what keeps creation no-replace without a
/// separate existence check. A failed post-create observation retains the path: Unix cannot bind a
/// later rollback unlink to the directory observed at creation; ADR 0015 also records that the
/// status-only create call cannot prove object continuity into this first observation.
#[cfg(unix)]
pub(crate) fn create_directory_entry(
    backend: &dyn LinkBackend,
    path: &Path,
) -> Result<OwnedDirectory, LinkError> {
    std::fs::create_dir(path).map_err(|error| LinkError::Create {
        destination: path.to_path_buf(),
        source: path.to_path_buf(),
        reason: error.to_string(),
    })?;
    #[cfg(test)]
    if let Err(error) = testing::reach_hook(testing::HookPoint::AfterDirectoryCreation, path, None)
    {
        return Err(retained_directory_create_error(path, &error.to_string()));
    }
    let created = match backend.inspect_no_follow(path) {
        Ok(created) => created,
        Err(error) => return Err(retained_directory_create_error(path, &error.to_string())),
    };
    if created.kind != EntryKind::Directory || created.identity.is_none() {
        return Err(retained_directory_create_error(
            path,
            "the initial staged entry observation was not a directory with stable identity",
        ));
    }
    Ok(OwnedDirectory {
        path: path.to_path_buf(),
        identity: created.identity,
    })
}

#[cfg(unix)]
fn retained_directory_create_error(path: &Path, reason: &str) -> LinkError {
    LinkError::Create {
        destination: path.to_path_buf(),
        source: path.to_path_buf(),
        reason: format!(
            "{reason}; no pathname rollback was attempted because ownership could not be proved \
             across removal; inspect the retained staged path {}",
            path.display()
        ),
    }
}

/// Shared implementation of [`LinkBackend::remove_empty_directory`].
#[cfg(unix)]
pub(crate) fn remove_owned_directory(
    backend: &dyn LinkBackend,
    recorded: &OwnedDirectory,
) -> Result<RemoveOutcome, LinkError> {
    let live = backend.inspect_no_follow(&recorded.path)?;
    match verify_directory_ownership(&live, recorded) {
        Ownership::Absent => Ok(RemoveOutcome::AlreadyAbsent),
        Ownership::Mismatch(mismatch) => Ok(RemoveOutcome::OwnershipMismatch(mismatch)),
        Ownership::Owned => {
            #[cfg(test)]
            testing::reach_hook(
                testing::HookPoint::BeforeRemovalMutation,
                &recorded.path,
                None,
            )?;
            // Recheck at the last boundary available to Unix. Product callers hold cooperative
            // SkillMount locks here, but a non-cooperating process can still race this pathname
            // before `remove_dir`; ADR 0014 records that residual limitation.
            let live = backend.inspect_no_follow(&recorded.path)?;
            match verify_directory_ownership(&live, recorded) {
                Ownership::Absent => Ok(RemoveOutcome::AlreadyAbsent),
                Ownership::Mismatch(mismatch) => Ok(RemoveOutcome::OwnershipMismatch(mismatch)),
                Ownership::Owned => match std::fs::remove_dir(&recorded.path) {
                    Ok(()) => Ok(RemoveOutcome::Removed),
                    // The operating system enforces emptiness, so a directory that gained contents
                    // between the identity check and this call is refused rather than emptied.
                    Err(error) if is_not_empty(&error) => Ok(RemoveOutcome::NotEmpty),
                    Err(error) => Err(LinkError::Remove {
                        path: recorded.path.clone(),
                        reason: error.to_string(),
                    }),
                },
            }
        }
    }
}

/// Returns whether a removal failed because the directory still holds entries.
///
/// `ErrorKind::DirectoryNotEmpty` is newer than the crate's minimum supported Rust version, so the
/// raw code is matched instead. Both codes come from the platform crate rather than a literal:
/// `ENOTEMPTY` is 66 on macOS and 39 on Linux, and hard-coding either would misclassify a real
/// failure as a harmless one on the other host.
fn is_not_empty(error: &std::io::Error) -> bool {
    #[cfg(unix)]
    let not_empty = libc::ENOTEMPTY;
    #[cfg(windows)]
    let not_empty = i32::try_from(windows_sys::Win32::Foundation::ERROR_DIR_NOT_EMPTY)
        .expect("the error code fits in an i32");

    error.raw_os_error() == Some(not_empty)
}
