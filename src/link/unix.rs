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
    CreatedLink, CreatedLinkKind, EntryKind, LinkBackend, LinkRequest, LinkTarget, Ownership,
    PathEntry, PlacementOutcome, PlatformIdentity, RemoveOutcome, sealed, verify_ownership,
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

        // A failure here would otherwise leave the link this function just created. The junction
        // path on Windows rolls back the directory it makes for the same reason; this keeps both
        // creation paths agreeing on who owns a half-finished entry.
        let created = self
            .inspect_no_follow(&request.staged_path)
            .inspect_err(|_| {
                let _ = fs::remove_file(&request.staged_path);
            })?;
        Ok(CreatedLink {
            path: request.staged_path.clone(),
            kind: CreatedLinkKind::Symlink,
            target: source_canonical.clone(),
            source_canonical,
            identity: created.identity,
        })
    }

    fn place_no_replace(
        &self,
        staged: &CreatedLink,
        destination: &Path,
    ) -> Result<PlacementOutcome, LinkError> {
        match super::unix_ffi::rename_no_replace(&staged.path, destination) {
            Ok(()) => Ok(PlacementOutcome::Placed(staged.relocated_to(destination))),
            Err(error) if error.kind() == io::ErrorKind::AlreadyExists => {
                Ok(PlacementOutcome::DestinationExists)
            }
            Err(error) if error.kind() == io::ErrorKind::Unsupported => {
                Err(LinkError::Unsupported {
                    path: destination.to_path_buf(),
                    reason: format!("no-replace renaming is unavailable here: {error}"),
                })
            }
            Err(error) => Err(LinkError::Place {
                staged: staged.path.clone(),
                destination: destination.to_path_buf(),
                reason: error.to_string(),
            }),
        }
    }

    fn remove_link_entry(&self, recorded: &CreatedLink) -> Result<RemoveOutcome, LinkError> {
        let live = self.inspect_no_follow(&recorded.path)?;
        match verify_ownership(&live, recorded, |target| {
            targets_match(&recorded.target, &target.raw)
        }) {
            Ownership::Absent => Ok(RemoveOutcome::AlreadyAbsent),
            Ownership::Mismatch(mismatch) => Ok(RemoveOutcome::OwnershipMismatch(mismatch)),
            // `remove_file` unlinks the symbolic link itself. `remove_dir_all` would follow it into
            // the user's own Skill source, which is why no code path here may ever reach for it.
            Ownership::Owned => fs::remove_file(&recorded.path)
                .map(|()| RemoveOutcome::Removed)
                .map_err(|error| LinkError::Remove {
                    path: recorded.path.clone(),
                    reason: error.to_string(),
                }),
        }
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
