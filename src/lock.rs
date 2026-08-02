//! Lock-resource identities that are discoverable before any lock is acquired.
//!
//! Acquiring locks belongs to the later transaction change. This module only *describes* the
//! resources a run would contend on, which is what read-only planning needs in order to report
//! `WOULD WAIT` without touching lock state.

use std::fmt;
use std::path::{Path, PathBuf};

use crate::error::AppError;
use crate::mount::resolve::ResolvedEntry;
use crate::paths::split_existing_anchor;

/// What a lock resource protects.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum LockResourceKind {
    /// A namespace the agent searches, such as `<project>/.agents/skills`.
    DiscoveryEntry,
    /// A physical store that holds mounted Skills.
    BackingStore,
}

impl LockResourceKind {
    /// Returns the stable label used in read-only output and in the future lock-key hash.
    #[must_use]
    pub const fn label(self) -> &'static str {
        match self {
            Self::DiscoveryEntry => "discovery-entry",
            Self::BackingStore => "backing-store",
        }
    }
}

/// An opaque identity for a resource that currently exists on disk.
///
/// The representation is private so the per-platform derivation can be strengthened without
/// changing the planning contract. Only equality and ordering are meaningful.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct PhysicalIdentity(String);

impl fmt::Display for PhysicalIdentity {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(&self.0)
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
    pub physical: Option<PhysicalIdentity>,
}

impl LockResourceIdentity {
    /// Returns the logical key, which exists whether or not the resource does.
    #[must_use]
    pub fn logical_key(&self) -> PathBuf {
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
        resource.identity.physical = resolved.terminal.as_deref().and_then(physical_identity);
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
    /// Ordering is by logical key first so a resource keeps its position whether or not it exists
    /// yet; the kind only breaks ties between two names that normalize identically. The final
    /// implementation sorts by the hash of these values, which the transaction change adds.
    #[must_use]
    pub fn ordering_key(&self) -> (PathBuf, LockResourceKind) {
        (self.identity.logical_key(), self.kind)
    }
}

/// Returns the device and inode pair that uniquely identifies an existing Unix directory.
#[cfg(unix)]
fn physical_identity(path: &Path) -> Option<PhysicalIdentity> {
    use std::os::unix::fs::MetadataExt;

    let metadata = std::fs::metadata(path).ok()?;
    Some(PhysicalIdentity(format!(
        "{:x}:{:x}",
        metadata.dev(),
        metadata.ino()
    )))
}

/// Returns the canonical terminal path that identifies an existing Windows directory.
///
/// The volume-serial and file-index pair is only reachable through the unstable
/// `windows_by_handle` API or raw FFI, and this crate sets `unsafe_code = "forbid"`.
/// Canonicalization already collapses symbolic links and junctions, so this key distinguishes
/// every layout `SkillMount` itself creates. It does not distinguish one directory reached through
/// two different volume mount points. That residual case needs a real file index and is recorded
/// as follow-up work for the change that introduces the Windows platform backend, which has to
/// revisit the `unsafe` lint anyway. Until then the logical key still serializes those callers,
/// because both spellings share the anchor and suffix.
#[cfg(windows)]
fn physical_identity(path: &Path) -> Option<PhysicalIdentity> {
    let canonical = std::fs::canonicalize(path).ok()?;
    Some(PhysicalIdentity(canonical.display().to_string()))
}

#[cfg(test)]
mod tests {
    use super::{LockResource, LockResourceKind};
    use crate::mount::resolve::classify;
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
            before.identity.logical_key(),
            after.identity.logical_key(),
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

        assert_eq!(
            before.identity.suffix,
            std::path::Path::new(".codex/skills")
        );
        assert_eq!(
            unanchored.identity.suffix,
            std::path::Path::new(""),
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
            direct.identity.logical_key(),
            through_link.identity.logical_key()
        );
        assert_eq!(
            direct.identity.physical, through_link.identity.physical,
            "aliases of one store must serialize against each other"
        );
    }

    #[test]
    fn a_resource_outside_its_anchor_is_an_internal_error() {
        let fixture = TestDir::new("lock-outside-anchor");
        let anchor = std::fs::canonicalize(fixture.path()).unwrap();

        let error = LockResource::describe(
            LockResourceKind::BackingStore,
            &anchor,
            std::path::Path::new("/elsewhere/skills"),
        )
        .expect_err("a resource outside the anchor is a planning invariant violation");

        assert_eq!(error.category(), crate::error::ExitCategory::Internal);
    }
}
