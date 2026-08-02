//! The macOS directory-link backend, built on standard Unix primitives.
//!
//! macOS has one directory indirection, the symbolic link, so this backend is small. What it does
//! not do matters more than what it does: it never removes a regular directory, never follows an
//! entry it is about to unlink, and never replaces a destination.
//!
//! Finder aliases are deliberately unsupported. An alias is a regular file carrying a bookmark
//! that only Cocoa resolves; the kernel does not follow it, so a child agent would find a file
//! where it expected a Skill directory. It classifies as [`EntryKind::File`] and is rejected.

use std::fs;
use std::io;
use std::os::unix::fs::MetadataExt;
use std::path::{Path, PathBuf};

use crate::domain::LinkMode;
use crate::error::LinkError;
use crate::link::resolve::targets_match;
use crate::link::{
    CreatedLink, CreatedLinkKind, EntryKind, LinkBackend, LinkRequest, LinkTarget, OwnedDirectory,
    Ownership, PathEntry, PlacementMismatch, PlacementOutcome, PlacementResidue, PlatformIdentity,
    RemoveOutcome, directory_placement_mismatch, link_placement_mismatch, sealed, verify_ownership,
};

/// The macOS backend.
pub(super) struct UnixBackend;

impl sealed::Sealed for UnixBackend {}

impl LinkBackend for UnixBackend {
    fn inspect_no_follow(&self, path: &Path) -> Result<PathEntry, LinkError> {
        let metadata = match fs::symlink_metadata(path) {
            Ok(metadata) => metadata,
            Err(error) if error.kind() == io::ErrorKind::NotFound => {
                return Ok(PathEntry::plain(path, EntryKind::Missing));
            }
            Err(error) => return Err(inspect_error(path, &error)),
        };

        let file_type = metadata.file_type();
        let kind = if file_type.is_symlink() {
            EntryKind::Symlink
        } else if file_type.is_dir() {
            EntryKind::Directory
        } else if file_type.is_file() {
            EntryKind::File
        } else {
            EntryKind::Other
        };

        let target = match kind {
            EntryKind::Symlink => Some(read_target(path)?),
            _ => None,
        };
        Ok(PathEntry {
            path: path.to_path_buf(),
            kind,
            target,
            identity: Some(PlatformIdentity::from_pair(
                "unix",
                metadata.dev(),
                metadata.ino(),
            )),
        })
    }

    fn canonical_directory(&self, path: &Path) -> Result<PathBuf, LinkError> {
        let canonical = fs::canonicalize(path).map_err(|error| inspect_error(path, &error))?;
        let metadata = fs::metadata(&canonical).map_err(|error| inspect_error(path, &error))?;
        if !metadata.is_dir() {
            return Err(LinkError::Inspect {
                path: path.to_path_buf(),
                reason: "expected a directory".to_owned(),
            });
        }
        Ok(canonical)
    }

    fn create_directory_link(&self, request: &LinkRequest) -> Result<CreatedLink, LinkError> {
        if request.mode == LinkMode::Junction {
            return Err(LinkError::Unsupported {
                path: request.staged_path.clone(),
                reason: "junctions exist only on Windows".to_owned(),
            });
        }

        let source_canonical =
            self.canonical_directory(&request.source)
                .map_err(|error| LinkError::Create {
                    destination: request.staged_path.clone(),
                    source: request.source.clone(),
                    reason: error.to_string(),
                })?;

        // The canonical source is written as the target rather than the path the caller supplied.
        // A relative or aliased target would make later ownership verification depend on the
        // working directory of whichever process performs the cleanup.
        std::os::unix::fs::symlink(&source_canonical, &request.staged_path).map_err(|error| {
            LinkError::Create {
                destination: request.staged_path.clone(),
                source: request.source.clone(),
                reason: error.to_string(),
            }
        })?;
        #[cfg(test)]
        if let Err(error) = super::testing::reach_hook(
            super::testing::HookPoint::AfterLinkCreation,
            &request.staged_path,
            None,
        ) {
            return Err(retained_create_error(request, &error.to_string()));
        }

        // Unix has no unlink-by-identity operation. If this inspection fails or observes a
        // replacement, a pathname rollback could delete an entry a non-cooperating process put at
        // the same name. Retaining the staged path is therefore the only sound failure behavior.
        let created = match self.inspect_no_follow(&request.staged_path) {
            Ok(created) => created,
            Err(error) => return Err(retained_create_error(request, &error.to_string())),
        };
        let Some(created_target) = created.target.as_ref() else {
            return Err(retained_create_error(
                request,
                "the staged entry could not be proved to be the symbolic link just created",
            ));
        };
        if created.kind != EntryKind::Symlink
            || !targets_match(&source_canonical, &created_target.raw)
            || created.identity.is_none()
        {
            return Err(retained_create_error(
                request,
                "the staged entry could not be proved to be the symbolic link just created",
            ));
        }
        #[cfg(test)]
        if let Err(error) = super::testing::reach_hook(
            super::testing::HookPoint::AfterLinkVerification,
            &request.staged_path,
            None,
        ) {
            return Err(retained_create_error(request, &error.to_string()));
        }
        Ok(CreatedLink {
            path: request.staged_path.clone(),
            kind: CreatedLinkKind::Symlink,
            target: created_target.raw.clone(),
            source_canonical,
            identity: created.identity,
        })
    }

    fn create_directory(&self, path: &Path) -> Result<OwnedDirectory, LinkError> {
        super::create_directory_entry(self, path)
    }

    fn place_no_replace(
        &self,
        staged: &CreatedLink,
        destination: &Path,
    ) -> Result<PlacementOutcome<CreatedLink>, LinkError> {
        #[cfg(test)]
        super::testing::reach_hook(
            super::testing::HookPoint::BeforePlacementVerification,
            &staged.path,
            Some(destination),
        )?;
        let live = self.inspect_no_follow(&staged.path)?;
        if let Some(mismatch) = link_placement_mismatch(&live, staged, |target| {
            targets_match(&staged.target, &target.raw)
        }) {
            return Ok(PlacementOutcome::OwnershipMismatch(PlacementResidue {
                path: staged.path.clone(),
                mismatch,
            }));
        }
        #[cfg(test)]
        super::testing::reach_hook(
            super::testing::HookPoint::BeforePlacementMutation,
            &staged.path,
            Some(destination),
        )?;
        if !place_path_no_replace(&staged.path, destination)? {
            return Ok(PlacementOutcome::DestinationExists);
        }
        #[cfg(test)]
        super::testing::reach_hook(
            super::testing::HookPoint::AfterPlacementMutation,
            destination,
            None,
        )?;

        let placed = staged.relocated_to(destination);
        let live = match self.inspect_no_follow(destination) {
            Ok(live) => live,
            Err(error) => {
                return Ok(PlacementOutcome::OwnershipMismatch(PlacementResidue {
                    path: destination.to_path_buf(),
                    mismatch: PlacementMismatch::InspectionFailed(error.to_string()),
                }));
            }
        };
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
        #[cfg(test)]
        super::testing::reach_hook(
            super::testing::HookPoint::BeforePlacementVerification,
            &staged.path,
            Some(destination),
        )?;
        let live = self.inspect_no_follow(&staged.path)?;
        if let Some(mismatch) = directory_placement_mismatch(&live, staged) {
            return Ok(PlacementOutcome::OwnershipMismatch(PlacementResidue {
                path: staged.path.clone(),
                mismatch,
            }));
        }
        #[cfg(test)]
        super::testing::reach_hook(
            super::testing::HookPoint::BeforePlacementMutation,
            &staged.path,
            Some(destination),
        )?;
        if !place_path_no_replace(&staged.path, destination)? {
            return Ok(PlacementOutcome::DestinationExists);
        }
        #[cfg(test)]
        super::testing::reach_hook(
            super::testing::HookPoint::AfterPlacementMutation,
            destination,
            None,
        )?;

        let placed = staged.relocated_to(destination);
        let live = match self.inspect_no_follow(destination) {
            Ok(live) => live,
            Err(error) => {
                return Ok(PlacementOutcome::OwnershipMismatch(PlacementResidue {
                    path: destination.to_path_buf(),
                    mismatch: PlacementMismatch::InspectionFailed(error.to_string()),
                }));
            }
        };
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
        super::remove_owned_directory(self, recorded)
    }

    fn remove_link_entry(&self, recorded: &CreatedLink) -> Result<RemoveOutcome, LinkError> {
        let live = self.inspect_no_follow(&recorded.path)?;
        match verify_ownership(&live, recorded, |target| {
            targets_match(&recorded.target, &target.raw)
        }) {
            Ownership::Absent => Ok(RemoveOutcome::AlreadyAbsent),
            Ownership::Mismatch(mismatch) => Ok(RemoveOutcome::OwnershipMismatch(mismatch)),
            Ownership::Owned => {
                #[cfg(test)]
                super::testing::reach_hook(
                    super::testing::HookPoint::BeforeRemovalMutation,
                    &recorded.path,
                    None,
                )?;

                // Product callers hold every logical and physical SkillMount lock across this
                // final check and unlink, which excludes another cooperating session. Those locks
                // are advisory state-file locks, not a capability over this directory. A process
                // that ignores them can still replace the pathname after this inspection and
                // before `remove_file`; macOS exposes no unlink-by-identity operation to close that
                // residual window (ADR 0014).
                let live = self.inspect_no_follow(&recorded.path)?;
                match verify_ownership(&live, recorded, |target| {
                    targets_match(&recorded.target, &target.raw)
                }) {
                    Ownership::Absent => Ok(RemoveOutcome::AlreadyAbsent),
                    Ownership::Mismatch(mismatch) => Ok(RemoveOutcome::OwnershipMismatch(mismatch)),
                    // `remove_file` unlinks the symbolic link itself. `remove_dir_all` would follow
                    // it into the user's Skill source, so no code path here may ever reach for it.
                    Ownership::Owned => fs::remove_file(&recorded.path)
                        .map(|()| RemoveOutcome::Removed)
                        .map_err(|error| LinkError::Remove {
                            path: recorded.path.clone(),
                            reason: error.to_string(),
                        }),
                }
            }
        }
    }
}

/// Runs the backend-private atomic pathname rename, returning `false` for destination contention.
fn place_path_no_replace(staged: &Path, destination: &Path) -> Result<bool, LinkError> {
    match super::unix_ffi::rename_no_replace(staged, destination) {
        Ok(()) => Ok(true),
        Err(error) if error.kind() == io::ErrorKind::AlreadyExists => Ok(false),
        Err(error) if error.kind() == io::ErrorKind::Unsupported => Err(LinkError::Unsupported {
            path: destination.to_path_buf(),
            reason: format!("no-replace renaming is unavailable here: {error}"),
        }),
        Err(error) => Err(LinkError::Place {
            staged: staged.to_path_buf(),
            destination: destination.to_path_buf(),
            reason: error.to_string(),
        }),
    }
}

fn read_target(path: &Path) -> Result<LinkTarget, LinkError> {
    let raw = fs::read_link(path).map_err(|error| inspect_error(path, &error))?;
    // A relative target resolves against the directory holding the link, never the CWD.
    let resolved = if raw.is_absolute() {
        raw.clone()
    } else {
        path.parent().unwrap_or_else(|| Path::new("")).join(&raw)
    };
    Ok(LinkTarget { raw, resolved })
}

fn inspect_error(path: &Path, error: &io::Error) -> LinkError {
    LinkError::Inspect {
        path: path.to_path_buf(),
        reason: error.to_string(),
    }
}

fn retained_create_error(request: &LinkRequest, reason: &str) -> LinkError {
    LinkError::Create {
        destination: request.staged_path.clone(),
        source: request.source.clone(),
        reason: format!(
            "{reason}; no pathname rollback was attempted because ownership could not be proved \
             across unlink; inspect the retained staged path {}",
            request.staged_path.display()
        ),
    }
}
