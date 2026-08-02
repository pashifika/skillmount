//! Lock-resource identities, their hashed keys, and the advisory locks taken on them.
//!
//! Describing a resource and locking it are deliberately separate. Read-only planning describes
//! every resource a run *would* contend on so it can report `WOULD WAIT` without touching lock
//! state; only a mutating session calls [`acquire::HeldLocks::acquire`].
//!
//! Two properties make the identities usable as locks. A logical key exists whether or not the
//! resource does, so a process creating a missing store and a process observing it afterwards
//! contend on the same key. A physical key exists once the resource does, so two spellings — an
//! alias, a second worktree — that reach one directory contend even though their paths differ.

pub mod acquire;
pub mod key;

#[cfg(test)]
mod tests;

use std::path::{Path, PathBuf};

use crate::error::AppError;
use crate::link::PlatformIdentity;
use crate::mount::resolve::ResolvedEntry;
use crate::paths::split_existing_anchor;

pub use key::LockKey;

/// What a lock resource protects.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum LockResourceKind {
    /// A namespace the agent searches, such as `<project>/.agents/skills`.
    DiscoveryEntry,
    /// A physical store that holds mounted Skills.
    BackingStore,
}

impl LockResourceKind {
    /// Returns the stable label used in read-only output, the journal, and the lock-key hash.
    #[must_use]
    pub const fn label(self) -> &'static str {
        match self {
            Self::DiscoveryEntry => "discovery-entry",
            Self::BackingStore => "backing-store",
        }
    }

    /// Parses a label a journal recorded earlier.
    #[must_use]
    pub fn parse(label: &str) -> Option<Self> {
        match label {
            "discovery-entry" => Some(Self::DiscoveryEntry),
            "backing-store" => Some(Self::BackingStore),
            _ => None,
        }
    }
}

/// The stable identity of one lockable resource.
///
/// The anchor and suffix are stored separately and never recombined at their source. This is the
/// whole point of the pair: the anchor is a directory that already exists and that the plan will
/// not create, so a process that runs *before* the intermediate directories exist and a process
/// that runs *after* they exist derive the same key and therefore contend with each other. Deriving
/// the key by canonicalizing the resource path directly would silently break that guarantee,
/// because canonicalization reaches deeper once the plan has created the intermediate directories.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct LockResourceIdentity {
    /// Canonical directory that already exists and that no planned action will create.
    pub anchor: PathBuf,
    /// Normalized path from the anchor to the resource.
    pub suffix: PathBuf,
    /// Physical identity, present only while the resource itself already exists.
    ///
    /// This is the same [`PlatformIdentity`] the link backend records for ownership evidence, so a
    /// lock and a removal agree on what "the same directory" means. On Windows that is the 128-bit
    /// `FILE_ID_INFO` value rather than a canonical path string, which distinguishes one directory
    /// reached through two volume mount points.
    pub physical: Option<PlatformIdentity>,
}

impl LockResourceIdentity {
    /// Returns the logical path, which exists whether or not the resource does.
    #[must_use]
    pub fn logical_path(&self) -> PathBuf {
        self.anchor.join(&self.suffix)
    }
}

/// A resource a later transaction would lock, described while planning is still read-only.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LockResource {
    /// What the resource protects.
    pub kind: LockResourceKind,
    /// Resource path as the plan intends it.
    pub path: PathBuf,
    /// Stable identity used to derive the lock key.
    pub identity: LockResourceIdentity,
}

impl LockResource {
    /// Describes a resource beneath an anchor that the plan will not create.
    ///
    /// `anchor` must be an existing canonical directory containing `path`; callers pass a project
    /// root or a session-root base, both of which are resolved before planning begins.
    ///
    /// # Errors
    ///
    /// Returns [`AppError::Internal`] when `path` is not beneath `anchor`, which is a planning
    /// invariant rather than an operator-visible condition.
    pub fn describe(kind: LockResourceKind, anchor: &Path, path: &Path) -> Result<Self, AppError> {
        let suffix = path.strip_prefix(anchor).map_err(|_| {
            AppError::Internal(format!(
                "lock resource {} is not beneath its anchor {}",
                path.display(),
                anchor.display()
            ))
        })?;
        Ok(Self {
            kind,
            path: path.to_path_buf(),
            identity: LockResourceIdentity {
                anchor: anchor.to_path_buf(),
                suffix: suffix.to_path_buf(),
                physical: physical_identity(path),
            },
        })
    }

    /// Describes a resource from an already-classified entry.
    ///
    /// The physical identity is taken from the terminal directory, so two discovery entries that
    /// reach one store through different link routes report the same physical resource and
    /// therefore serialize against each other.
    ///
    /// # Errors
    ///
    /// Returns [`AppError::Internal`] when the entry is not beneath `anchor`.
    pub fn describe_entry(
        kind: LockResourceKind,
        anchor: &Path,
        resolved: &ResolvedEntry,
    ) -> Result<Self, AppError> {
        let mut resource = Self::describe(kind, anchor, &resolved.entry)?;
        resource.identity.physical = resolved.terminal.as_deref().and_then(identity_of_existing);
        Ok(resource)
    }

    /// Describes a resource whose anchor is the deepest directory that currently exists.
    ///
    /// Used where every component below the anchor is created by this plan and belongs to this
    /// run alone, such as a Claude staging root under a unique session identifier. Cross-run key
    /// stability does not apply there: no other process ever addresses the same session path, so
    /// the anchor moving deeper on a later run cannot cause two processes to miss each other.
    /// Anything shared between runs must use [`LockResource::describe`] with an explicit anchor.
    #[must_use]
    pub fn describe_unanchored(kind: LockResourceKind, path: &Path) -> Self {
        let (anchor, suffix) = split_existing_anchor(path);
        Self {
            kind,
            path: path.to_path_buf(),
            identity: LockResourceIdentity {
                anchor,
                suffix,
                physical: physical_identity(path),
            },
        }
    }

    /// Returns the deterministic ordering key used to acquire locks without deadlocking.
    ///
    /// Ordering is by logical path first so a resource keeps its position whether or not it exists
    /// yet; the kind only breaks ties between two names that normalize identically. Acquisition
    /// sorts by the *hashed* keys this produces, which [`LockResource::lock_keys`] returns.
    #[must_use]
    pub fn ordering_key(&self) -> (PathBuf, LockResourceKind) {
        (self.identity.logical_path(), self.kind)
    }

    /// Returns every hashed key a session must hold to own this resource.
    ///
    /// Always the logical key, plus the physical key when the resource already exists. Both are
    /// required rather than either: the logical key alone would let two worktrees mutate one shared
    /// store, and the physical key alone would not exist yet for a store this run is about to
    /// create.
    #[must_use]
    pub fn lock_keys(&self) -> Vec<LockKey> {
        let mut keys = vec![key::logical(self.kind, &self.identity)];
        if let Some(physical) = &self.identity.physical {
            keys.push(key::physical(physical));
        }
        keys
    }
}

/// Returns the platform identity of the directory a path ultimately reaches.
///
/// Resolution is deliberate: an alias and its target must produce the same value, which is what
/// makes two worktrees serialize against one backing store. A path that does not exist, or that is
/// not a directory, has no identity and contributes only a logical key.
fn physical_identity(path: &Path) -> Option<PlatformIdentity> {
    let backend = crate::link::platform_backend();
    let canonical = backend.canonical_directory(path).ok()?;
    identity_of_existing(&canonical)
}

/// Returns the platform identity of a path already known to be canonical.
fn identity_of_existing(canonical: &Path) -> Option<PlatformIdentity> {
    crate::link::platform_backend()
        .inspect_no_follow(canonical)
        .ok()?
        .identity
}
