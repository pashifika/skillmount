//! An in-memory [`LinkBackend`] that records every call it receives.
//!
//! Shared resolution has branches no host can produce on demand. A macOS runner cannot build a
//! junction, a Windows runner without Developer Mode cannot build a symbolic link, and neither can
//! conjure a filesystem where a link chain is exactly one hop too deep without creating forty real
//! entries. This backend models those layouts directly, so every branch of the walker is exercised
//! on both platforms and in the same order.
//!
//! It is also a conformance check on the contract itself. It refuses to replace anything, refuses
//! to remove a regular directory, and refuses operations no platform supports — so a caller that
//! starts depending on lenient behavior fails here first.

use std::cell::RefCell;
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::sync::{Mutex, MutexGuard};

use crate::domain::LinkMode;
use crate::error::LinkError;
use crate::link::resolve::targets_match;
use crate::link::{
    CreatedLink, CreatedLinkKind, EntryKind, LinkBackend, LinkRequest, LinkTarget, OwnedDirectory,
    Ownership, PathEntry, PlacementOutcome, PlacementResidue, PlatformIdentity, RemoveOutcome,
    directory_placement_mismatch, link_placement_mismatch, sealed, verify_ownership,
};
use crate::paths::lexical_normalize;

/// Deterministic boundary exposed only to in-crate tests.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum HookPoint {
    /// Immediately before staged ownership is inspected for placement.
    BeforePlacementVerification,
    /// After staged ownership is proved and immediately before the placement mutation.
    BeforePlacementMutation,
    /// After the placement mutation and before destination ownership is inspected.
    AfterPlacementMutation,
    /// After a link entry is created and before ownership evidence is established.
    AfterLinkCreation,
    /// After link ownership is established and before creation returns it to the transaction.
    AfterLinkVerification,
    /// After a helper directory is created and before ownership evidence is established.
    AfterDirectoryCreation,
    /// After removal ownership is proved and immediately before the removal mutation.
    BeforeRemovalMutation,
}

/// One invocation of a deterministic backend test boundary.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct HookEvent {
    /// Boundary that was reached.
    pub(crate) point: HookPoint,
    /// Entry being verified or mutated.
    pub(crate) path: PathBuf,
    /// Placement destination, when the boundary belongs to placement.
    pub(crate) destination: Option<PathBuf>,
}

type Hook = Box<dyn FnMut(HookEvent) -> Result<(), LinkError>>;

thread_local! {
    static TEST_HOOK: RefCell<Option<Hook>> = RefCell::new(None);
}

/// Runs `operation` with one thread-local backend hook installed.
pub(crate) fn with_hook<R>(
    hook: impl FnMut(HookEvent) -> Result<(), LinkError> + 'static,
    operation: impl FnOnce() -> R,
) -> R {
    TEST_HOOK.with(|slot| {
        assert!(
            slot.borrow_mut().replace(Box::new(hook)).is_none(),
            "backend test hooks cannot be nested"
        );
    });
    let _guard = HookGuard;
    operation()
}

/// Announces one backend boundary to the current test hook.
pub(crate) fn reach_hook(
    point: HookPoint,
    path: &Path,
    destination: Option<&Path>,
) -> Result<(), LinkError> {
    TEST_HOOK.with(|slot| {
        let mut hook = slot.borrow_mut();
        let Some(hook) = hook.as_mut() else {
            return Ok(());
        };
        hook(HookEvent {
            point,
            path: path.to_path_buf(),
            destination: destination.map(Path::to_path_buf),
        })
    })
}

struct HookGuard;

impl Drop for HookGuard {
    fn drop(&mut self) {
        TEST_HOOK.with(|slot| {
            slot.borrow_mut().take();
        });
    }
}

/// One modelled entry.
#[derive(Debug, Clone, PartialEq, Eq)]
enum Entry {
    /// A regular directory.
    Directory,
    /// A regular file.
    File,
    /// An entry that can never back a namespace.
    Other,
    /// A directory link and the target stored in it, exactly as a real one would store it.
    Link {
        /// Target as stored, which may be relative.
        target: PathBuf,
        /// Implementation the entry models.
        kind: CreatedLinkKind,
    },
}

/// One call the backend received, in order.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum Call {
    /// [`LinkBackend::inspect_no_follow`].
    Inspect(PathBuf),
    /// [`LinkBackend::canonical_directory`].
    Canonicalize(PathBuf),
    /// [`LinkBackend::create_directory_link`].
    Create(PathBuf),
    /// [`LinkBackend::place_no_replace`].
    Place(PathBuf, PathBuf),
    /// [`LinkBackend::remove_link_entry`].
    Remove(PathBuf),
}

/// Takes the lock, treating poisoning as the test bug it would be.
///
/// The state is behind a mutex rather than a `RefCell` because [`LinkBackend`] is `Send + Sync`,
/// which the placement-race tests rely on.
trait Locked<T> {
    fn locked(&self) -> MutexGuard<'_, T>;
}

impl<T> Locked<T> for Mutex<T> {
    fn locked(&self) -> MutexGuard<'_, T> {
        self.lock()
            .expect("a modelled filesystem is only poisoned by an already-failing test")
    }
}

/// A modelled filesystem that answers the backend contract.
#[derive(Debug, Default)]
pub(crate) struct RecordingBackend {
    entries: Mutex<BTreeMap<PathBuf, (Entry, u64)>>,
    calls: Mutex<Vec<Call>>,
    next_identity: Mutex<u64>,
    replace_destination_after_next_place: Mutex<bool>,
}

impl RecordingBackend {
    /// Builds an empty modelled filesystem.
    pub(crate) fn new() -> Self {
        Self::default()
    }

    /// Adds a regular directory.
    pub(crate) fn with_directory(self, path: &str) -> Self {
        self.insert(path, Entry::Directory)
    }

    /// Adds a regular file.
    pub(crate) fn with_file(self, path: &str) -> Self {
        self.insert(path, Entry::File)
    }

    /// Adds an entry that is neither a directory, a file, nor a supported link.
    pub(crate) fn with_other(self, path: &str) -> Self {
        self.insert(path, Entry::Other)
    }

    /// Adds a symbolic link storing `target` exactly as given, relative or absolute.
    pub(crate) fn with_symlink(self, path: &str, target: &str) -> Self {
        self.insert(
            path,
            Entry::Link {
                target: PathBuf::from(target),
                kind: CreatedLinkKind::Symlink,
            },
        )
    }

    /// Adds a junction storing `target`.
    pub(crate) fn with_junction(self, path: &str, target: &str) -> Self {
        self.insert(
            path,
            Entry::Link {
                target: PathBuf::from(target),
                kind: CreatedLinkKind::Junction,
            },
        )
    }

    /// Adds a symbolic link to an already-built model, for layouts assembled in a loop.
    pub(crate) fn add_symlink(&self, path: &str, target: &str) {
        let identity = self.take_identity();
        self.entries.locked().insert(
            key(Path::new(path)),
            (
                Entry::Link {
                    target: PathBuf::from(target),
                    kind: CreatedLinkKind::Symlink,
                },
                identity,
            ),
        );
    }

    /// Replaces a path with a newly identified regular directory.
    pub(crate) fn replace_with_directory(&self, path: &Path) {
        let identity = self.take_identity();
        self.entries
            .locked()
            .insert(key(path), (Entry::Directory, identity));
    }

    /// Returns every call received so far, in order.
    pub(crate) fn calls(&self) -> Vec<Call> {
        self.calls.locked().clone()
    }

    /// Returns whether an entry currently exists, for asserting what an operation left behind.
    pub(crate) fn contains(&self, path: &Path) -> bool {
        self.entries.locked().contains_key(&key(path))
    }

    /// Replaces the destination with a modelled file after the next raw placement mutation.
    ///
    /// This models the Unix pathname window between rename and destination verification. It is a
    /// one-shot seam so parallel contract cases cannot accidentally affect a later operation.
    pub(crate) fn replace_destination_after_next_place_with_file(&self) {
        *self.replace_destination_after_next_place.locked() = true;
    }

    fn insert(self, path: &str, entry: Entry) -> Self {
        let identity = self.take_identity();
        self.entries
            .locked()
            .insert(key(Path::new(path)), (entry, identity));
        self
    }

    fn take_identity(&self) -> u64 {
        let mut next = self.next_identity.locked();
        *next += 1;
        *next
    }

    fn lookup(&self, path: &Path) -> Option<(Entry, u64)> {
        self.entries.locked().get(&key(path)).cloned()
    }

    fn record(&self, call: Call) {
        self.calls.locked().push(call);
    }

    /// Moves one modelled entry without replacement and optionally fires the one-shot race seam.
    fn place_entry_no_replace(&self, staged: &Path, destination: &Path) -> Result<bool, LinkError> {
        self.record(Call::Place(staged.to_path_buf(), destination.to_path_buf()));
        if self.lookup(destination).is_some() {
            return Ok(false);
        }
        let replace_after =
            std::mem::take(&mut *self.replace_destination_after_next_place.locked());
        let replacement_identity = replace_after.then(|| self.take_identity());
        let mut entries = self.entries.locked();
        let Some(entry) = entries.remove(&key(staged)) else {
            return Err(LinkError::Place {
                staged: staged.to_path_buf(),
                destination: destination.to_path_buf(),
                reason: "the staged entry no longer exists".to_owned(),
            });
        };
        entries.insert(key(destination), entry);
        if let Some(identity) = replacement_identity {
            entries.insert(key(destination), (Entry::File, identity));
        }
        Ok(true)
    }
}

impl sealed::Sealed for RecordingBackend {}

impl LinkBackend for RecordingBackend {
    fn inspect_no_follow(&self, path: &Path) -> Result<PathEntry, LinkError> {
        self.record(Call::Inspect(path.to_path_buf()));
        let Some((entry, identity)) = self.lookup(path) else {
            return Ok(PathEntry::plain(path, EntryKind::Missing));
        };
        let identity = Some(PlatformIdentity::from_pair("model", 0, identity));
        Ok(match entry {
            Entry::Directory => PathEntry {
                path: path.to_path_buf(),
                kind: EntryKind::Directory,
                target: None,
                identity,
            },
            Entry::File => PathEntry {
                path: path.to_path_buf(),
                kind: EntryKind::File,
                target: None,
                identity,
            },
            Entry::Other => PathEntry {
                path: path.to_path_buf(),
                kind: EntryKind::Other,
                target: None,
                identity,
            },
            Entry::Link { target, kind } => {
                let resolved = if target.is_absolute() {
                    target.clone()
                } else {
                    path.parent().unwrap_or_else(|| Path::new("")).join(&target)
                };
                PathEntry {
                    path: path.to_path_buf(),
                    kind: kind.entry_kind(),
                    target: Some(LinkTarget {
                        raw: target,
                        resolved,
                    }),
                    identity,
                }
            }
        })
    }

    fn canonical_directory(&self, path: &Path) -> Result<PathBuf, LinkError> {
        self.record(Call::Canonicalize(path.to_path_buf()));
        match self.lookup(path) {
            Some((Entry::Directory, _)) => Ok(key(path)),
            _ => Err(LinkError::Inspect {
                path: path.to_path_buf(),
                reason: "expected a directory".to_owned(),
            }),
        }
    }

    fn create_directory_link(&self, request: &LinkRequest) -> Result<CreatedLink, LinkError> {
        self.record(Call::Create(request.staged_path.clone()));
        if request.mode == LinkMode::Junction {
            return Err(LinkError::Unsupported {
                path: request.staged_path.clone(),
                reason: "the modelled backend creates symbolic links only".to_owned(),
            });
        }
        if self.lookup(&request.staged_path).is_some() {
            return Err(LinkError::Create {
                destination: request.staged_path.clone(),
                source: request.source.clone(),
                reason: "the staged path is already occupied".to_owned(),
            });
        }
        let source_canonical = self.canonical_directory(&request.source)?;

        let identity = self.take_identity();
        self.entries.locked().insert(
            key(&request.staged_path),
            (
                Entry::Link {
                    target: source_canonical.clone(),
                    kind: CreatedLinkKind::Symlink,
                },
                identity,
            ),
        );
        Ok(CreatedLink {
            path: request.staged_path.clone(),
            kind: CreatedLinkKind::Symlink,
            target: source_canonical.clone(),
            source_canonical,
            identity: Some(PlatformIdentity::from_pair("model", 0, identity)),
        })
    }

    fn create_directory(&self, path: &Path) -> Result<OwnedDirectory, LinkError> {
        self.record(Call::Create(path.to_path_buf()));
        if self.lookup(path).is_some() {
            return Err(LinkError::Create {
                destination: path.to_path_buf(),
                source: path.to_path_buf(),
                reason: "the staged path is already occupied".to_owned(),
            });
        }
        let identity = self.take_identity();
        self.entries
            .locked()
            .insert(key(path), (Entry::Directory, identity));
        Ok(OwnedDirectory {
            path: path.to_path_buf(),
            identity: Some(PlatformIdentity::from_pair("model", 0, identity)),
        })
    }

    fn place_no_replace(
        &self,
        staged: &CreatedLink,
        destination: &Path,
    ) -> Result<PlacementOutcome<CreatedLink>, LinkError> {
        let live = self.inspect_no_follow(&staged.path)?;
        if let Some(mismatch) = link_placement_mismatch(&live, staged, |target| {
            targets_match(&staged.target, &target.raw)
        }) {
            return Ok(PlacementOutcome::OwnershipMismatch(PlacementResidue {
                path: staged.path.clone(),
                mismatch,
            }));
        }
        if !self.place_entry_no_replace(&staged.path, destination)? {
            return Ok(PlacementOutcome::DestinationExists);
        }

        let placed = staged.relocated_to(destination);
        let live = self.inspect_no_follow(destination)?;
        if let Some(mismatch) = link_placement_mismatch(&live, &placed, |target| {
            targets_match(&placed.target, &target.raw)
        }) {
            return Ok(PlacementOutcome::OwnershipMismatch(PlacementResidue {
                path: destination.to_path_buf(),
                mismatch,
            }));
        }
        Ok(PlacementOutcome::Placed(placed))
    }

    fn place_directory_no_replace(
        &self,
        staged: &OwnedDirectory,
        destination: &Path,
    ) -> Result<PlacementOutcome<OwnedDirectory>, LinkError> {
        let live = self.inspect_no_follow(&staged.path)?;
        if let Some(mismatch) = directory_placement_mismatch(&live, staged) {
            return Ok(PlacementOutcome::OwnershipMismatch(PlacementResidue {
                path: staged.path.clone(),
                mismatch,
            }));
        }
        if !self.place_entry_no_replace(&staged.path, destination)? {
            return Ok(PlacementOutcome::DestinationExists);
        }

        let placed = staged.relocated_to(destination);
        let live = self.inspect_no_follow(destination)?;
        if let Some(mismatch) = directory_placement_mismatch(&live, &placed) {
            return Ok(PlacementOutcome::OwnershipMismatch(PlacementResidue {
                path: destination.to_path_buf(),
                mismatch,
            }));
        }
        Ok(PlacementOutcome::Placed(placed))
    }

    fn remove_empty_directory(
        &self,
        recorded: &OwnedDirectory,
    ) -> Result<RemoveOutcome, LinkError> {
        self.record(Call::Remove(recorded.path.clone()));
        let live = self.inspect_no_follow(&recorded.path)?;
        match crate::link::verify_directory_ownership(&live, recorded) {
            Ownership::Absent => Ok(RemoveOutcome::AlreadyAbsent),
            Ownership::Mismatch(mismatch) => Ok(RemoveOutcome::OwnershipMismatch(mismatch)),
            Ownership::Owned => {
                let prefix = key(&recorded.path);
                let occupied = self
                    .entries
                    .locked()
                    .keys()
                    .any(|candidate| candidate != &prefix && candidate.starts_with(&prefix));
                if occupied {
                    return Ok(RemoveOutcome::NotEmpty);
                }
                self.entries.locked().remove(&prefix);
                Ok(RemoveOutcome::Removed)
            }
        }
    }

    fn remove_link_entry(&self, recorded: &CreatedLink) -> Result<RemoveOutcome, LinkError> {
        self.record(Call::Remove(recorded.path.clone()));
        let live = self.inspect_no_follow(&recorded.path)?;
        match verify_ownership(&live, recorded, |target| {
            targets_match(&recorded.target, &target.raw)
        }) {
            Ownership::Absent => Ok(RemoveOutcome::AlreadyAbsent),
            Ownership::Mismatch(mismatch) => Ok(RemoveOutcome::OwnershipMismatch(mismatch)),
            Ownership::Owned => {
                self.entries.locked().remove(&key(&recorded.path));
                Ok(RemoveOutcome::Removed)
            }
        }
    }
}

fn key(path: &Path) -> PathBuf {
    lexical_normalize(path)
}
