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
use crate::link::resolve::ComparablePath;
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
/// How a session uses a lock resource.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum LockAccess {
    /// Namespace evidence that `SkillMount` reads but never changes.
    Observe,
    /// A resource that the transaction may create, modify, or remove.
    Mutate,
}

impl LockAccess {
    /// Returns the stable label used in read-only output and journals.
    #[must_use]
    pub const fn label(self) -> &'static str {
        match self {
            Self::Observe => "observe",
            Self::Mutate => "mutate",
        }
    }

    /// Parses an access label recorded in a journal.
    #[must_use]
    pub fn parse(label: &str) -> Option<Self> {
        match label {
            "observe" => Some(Self::Observe),
            "mutate" => Some(Self::Mutate),
            _ => None,
        }
    }

    /// Returns whether this held access satisfies `required`.
    #[must_use]
    pub const fn satisfies(self, required: Self) -> bool {
        matches!(
            (self, required),
            (Self::Observe, Self::Observe) | (Self::Mutate, _)
        )
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

/// A resource and the access a transaction coordinates while planning is still read-only.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LockResource {
    /// What the resource protects.
    pub kind: LockResourceKind,
    /// How the session uses the resource.
    pub access: LockAccess,
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
    pub fn describe(
        kind: LockResourceKind,
        access: LockAccess,
        anchor: &Path,
        path: &Path,
    ) -> Result<Self, AppError> {
        let suffix = path.strip_prefix(anchor).map_err(|_| {
            AppError::Internal(format!(
                "lock resource {} is not beneath its anchor {}",
                path.display(),
                anchor.display()
            ))
        })?;
        Ok(Self {
            kind,
            access,
            path: path.to_path_buf(),
            identity: LockResourceIdentity {
                anchor: anchor.to_path_buf(),
                suffix: suffix.to_path_buf(),
                physical: physical_identity(path),
            },
        })
    }

    /// Describes a shared absolute path with a root anchor that cannot move as directories appear.
    ///
    /// User, administrator, settings, plugin, and compatibility roots may be missing during one
    /// session and created before another. Anchoring at the deepest existing directory would give
    /// those two observations different logical keys. The absolute path's volume root is stable
    /// across that transition and is never transaction-owned.
    ///
    /// # Errors
    ///
    /// Returns [`AppError::Internal`] when `path` is not absolute.
    pub fn describe_shared(
        kind: LockResourceKind,
        access: LockAccess,
        path: &Path,
    ) -> Result<Self, AppError> {
        if !path.is_absolute() {
            return Err(AppError::Internal(format!(
                "shared lock resource {} is not absolute",
                path.display()
            )));
        }
        let anchor = path
            .ancestors()
            .last()
            .filter(|candidate| !candidate.as_os_str().is_empty())
            .ok_or_else(|| {
                AppError::Internal(format!(
                    "shared lock resource {} has no stable root anchor",
                    path.display()
                ))
            })?;
        Self::describe(kind, access, anchor, path)
    }

    /// Describes an already-classified shared entry beneath its stable volume-root anchor.
    ///
    /// This is the resolved-entry counterpart to [`LockResource::describe_shared`]. Every
    /// discovery entry that can be addressed across projects uses this representation so an
    /// external observer and a project writer derive the same logical key while the entry is
    /// missing. The resolved terminal still supplies the physical identity after it exists.
    ///
    /// # Errors
    ///
    /// Returns [`AppError::Internal`] when the entry path is not absolute.
    pub fn describe_shared_entry(
        access: LockAccess,
        resolved: &ResolvedEntry,
    ) -> Result<Self, AppError> {
        let mut resource =
            Self::describe_shared(LockResourceKind::DiscoveryEntry, access, &resolved.entry)?;
        resource.identity.physical = resolved.terminal.as_deref().and_then(identity_of_existing);
        Ok(resource)
    }

    /// Describes a classified discovery entry under both the shared and legacy logical identities.
    ///
    /// The first resource uses the volume-root identity required for cross-project readers and
    /// writers. The second preserves the identity emitted by `origin/dev/0.3.x`: project entries
    /// used their project anchor, while external entries used the deepest existing anchor.
    ///
    /// # Errors
    ///
    /// Returns [`AppError::Internal`] when the shared path is not absolute or when the entry is not
    /// beneath a supplied legacy anchor.
    pub(crate) fn describe_shared_and_legacy_entry(
        access: LockAccess,
        legacy_anchor: Option<&Path>,
        resolved: &ResolvedEntry,
    ) -> Result<[Self; 2], AppError> {
        let shared = Self::describe_shared_entry(access, resolved)?;
        let legacy = if let Some(anchor) = legacy_anchor {
            Self::describe_entry(LockResourceKind::DiscoveryEntry, access, anchor, resolved)?
        } else {
            Self::describe_unanchored(LockResourceKind::DiscoveryEntry, access, &resolved.entry)
        };
        Ok([shared, legacy])
    }

    /// Describes a shared path under its volume-root and legacy deepest-existing identities.
    ///
    /// This is the non-classified counterpart used for external inputs and traversed directories
    /// that `origin/dev/0.3.x` described with [`LockResource::describe_unanchored`].
    ///
    /// # Errors
    ///
    /// Returns [`AppError::Internal`] when `path` is not absolute.
    pub(crate) fn describe_shared_and_legacy_unanchored(
        kind: LockResourceKind,
        access: LockAccess,
        path: &Path,
    ) -> Result<[Self; 2], AppError> {
        Ok([
            Self::describe_shared(kind, access, path)?,
            Self::describe_unanchored(kind, access, path),
        ])
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
        access: LockAccess,
        anchor: &Path,
        resolved: &ResolvedEntry,
    ) -> Result<Self, AppError> {
        let mut resource = Self::describe(kind, access, anchor, &resolved.entry)?;
        resource.identity.physical = resolved.terminal.as_deref().and_then(identity_of_existing);
        Ok(resource)
    }

    /// Describes a resource whose anchor is the deepest directory that currently exists.
    ///
    /// Used where every component below the anchor is created by this plan and belongs to this
    /// run alone, such as a Claude staging root under a unique session identifier. Cross-run key
    /// stability does not apply there: no other process ever addresses the same session path, so
    /// the anchor moving deeper on a later run cannot cause two processes to miss each other.
    /// Anything shared between runs must also use a stable explicit or volume-root identity; a
    /// legacy compatibility companion may additionally retain this deepest-existing identity.
    #[must_use]
    pub fn describe_unanchored(kind: LockResourceKind, access: LockAccess, path: &Path) -> Self {
        let (anchor, suffix) = split_existing_anchor(path);
        Self {
            kind,
            access,
            path: path.to_path_buf(),
            identity: LockResourceIdentity {
                anchor,
                suffix,
                physical: physical_identity(path),
            },
        }
    }

    /// Returns whether this resource's durable logical identity grants mutation over `path`.
    ///
    /// `LockResource::path` is diagnostic spelling only and cannot grant or revoke authority. The
    /// required path is checked both as supplied and after canonicalizing only its parent chain:
    /// existing ancestors may have an equivalent canonical spelling (for example a Windows long
    /// name for a caller-supplied 8.3 path), while the final entry must never be followed because
    /// it can be a transaction-owned symlink or junction. The normalized logical identity is the
    /// value that actually derives the held lock key.
    ///
    /// Callers retain and acquire this resource rather than synthesizing one for `path`, so an
    /// existing resource's physical key remains part of the required authority.
    #[must_use]
    pub(crate) fn authorizes_mutation_of(&self, path: &Path) -> bool {
        if !self.access.satisfies(LockAccess::Mutate) {
            return false;
        }

        let logical = ComparablePath::new(&self.identity.logical_path());
        let raw = ComparablePath::new(path);
        if raw.key().starts_with(logical.key()) {
            return true;
        }

        no_follow_canonical_spelling(path).is_some_and(|candidate| {
            ComparablePath::new(&candidate)
                .key()
                .starts_with(logical.key())
        })
    }

    /// Returns whether this resource grants mutation under the canonical shared-path logical key.
    ///
    /// Joined-path equality is insufficient because the logical-key hash preserves `anchor` and
    /// `suffix` as separate inputs. Re-describing the path through the volume-root shared identity
    /// proves that an independent observer hashes the same key.
    #[must_use]
    pub(crate) fn authorizes_exact_mutation_of(&self, path: &Path) -> bool {
        let Ok(shared) = Self::describe_shared(self.kind, LockAccess::Mutate, path) else {
            return false;
        };
        self.authorizes_mutation_of(path)
            && key::logical(self.kind, &self.identity)
                == key::logical(shared.kind, &shared.identity)
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

/// Returns a canonical spelling of `path` without resolving its final entry.
///
/// Recovery candidates may already be symlinks or junctions created by the transaction. Following
/// that entry would compare authority with its target rather than with the owned directory entry.
fn no_follow_canonical_spelling(path: &Path) -> Option<PathBuf> {
    let parent = path.parent()?;
    let final_name = path.file_name()?;
    let (anchor, suffix) = split_existing_anchor(parent);
    if anchor.as_os_str().is_empty() {
        return None;
    }
    Some(anchor.join(suffix).join(final_name))
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
