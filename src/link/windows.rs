//! The Windows directory-link backend.
//!
//! Windows has two directory indirections and they are not interchangeable. A directory symbolic
//! link is what `SkillMount` wants: agents resolve it, it may point anywhere, and creating one needs
//! either Developer Mode or an elevated process. A junction needs no privilege but only ever
//! points at a local directory, and whether a given agent follows one is not something this layer
//! can promise.
//!
//! So `auto` tries the symbolic link first and falls back to a junction for exactly one failure:
//! `ERROR_PRIVILEGE_NOT_HELD`. Any other failure keeps its original error. A fallback that
//! triggered on "some error" would turn an unrelated fault — a full disk, a denied ACL — into a
//! silently different mount implementation.

use std::fs;
use std::io;
use std::os::windows::ffi::{OsStrExt, OsStringExt};
use std::os::windows::fs::MetadataExt;
use std::path::{Path, PathBuf};

use windows_sys::Win32::Foundation::ERROR_PRIVILEGE_NOT_HELD;
use windows_sys::Win32::Storage::FileSystem::FILE_ATTRIBUTE_REPARSE_POINT;

use crate::domain::LinkMode;
use crate::error::LinkError;
use crate::link::resolve::targets_match;
use crate::link::{
    CreatedLink, CreatedLinkKind, EntryKind, LinkBackend, LinkRequest, LinkTarget, OwnedDirectory,
    Ownership, PathEntry, PathPlacement, PlatformIdentity, RemoveOutcome, sealed, verify_ownership,
};

use super::reparse::{self, IO_REPARSE_TAG_MOUNT_POINT, IO_REPARSE_TAG_SYMLINK};
use super::windows_ffi::{self, Access};
use super::winpath;

/// The Windows backend.
pub(super) struct WindowsBackend;

impl sealed::Sealed for WindowsBackend {}

impl LinkBackend for WindowsBackend {
    fn inspect_no_follow(&self, path: &Path) -> Result<PathEntry, LinkError> {
        let metadata = match fs::symlink_metadata(path) {
            Ok(metadata) => metadata,
            Err(error) if error.kind() == io::ErrorKind::NotFound => {
                return Ok(PathEntry::plain(path, EntryKind::Missing));
            }
            Err(error) => return Err(inspect_error(path, &error)),
        };

        // The handle is opened without traversal, so every value read through it describes the
        // entry itself and never the directory it may point at.
        //
        // A failure here is not an inspection failure. `symlink_metadata` already succeeded, so
        // the entry exists and its attributes are known; another process holding it without
        // sharing must not turn a classification into an error. What is lost is the identity and,
        // for a reparse point, the tag — and an entry whose tag cannot be read is reported as
        // unsupported rather than guessed at, which is the fail-closed answer: it can back no
        // namespace and can never be mistaken for a link this process owns.
        let handle = windows_ffi::open_no_follow(path, Access::Inspect).ok();
        let identity = handle
            .as_ref()
            .and_then(|handle| windows_ffi::file_identity(handle).ok())
            .map(platform_identity);

        let plain = |kind| {
            Ok(PathEntry {
                path: path.to_path_buf(),
                kind,
                target: None,
                identity: identity.clone(),
            })
        };
        if metadata.file_attributes() & FILE_ATTRIBUTE_REPARSE_POINT == 0 {
            return plain(attribute_kind(&metadata));
        }

        let decoded = handle
            .and_then(|handle| windows_ffi::read_reparse_point(&handle).ok())
            .map(|buffer| reparse::parse(&buffer));
        let point = match decoded {
            Some(Ok(point)) => point,
            // A tag this backend does not own but which is *not* a name surrogate does not
            // redirect: a cloud placeholder, a deduplication stub, a WIM-backed file. Windows
            // resolves those as the ordinary directory or file their attributes describe, and so
            // must this — reporting them as unusable would refuse a Skill store for living in a
            // synced folder.
            Some(Err(reparse::ReparseError::UnsupportedTag(tag)))
                if !reparse::is_name_surrogate(tag) =>
            {
                return plain(attribute_kind(&metadata));
            }
            // A surrogate tag this backend cannot decode *does* redirect, somewhere it cannot
            // describe, so it stays unusable. So does a buffer it could not read at all.
            _ => return plain(EntryKind::Other),
        };
        let kind = match point.tag {
            IO_REPARSE_TAG_MOUNT_POINT => EntryKind::Junction,
            IO_REPARSE_TAG_SYMLINK => EntryKind::Symlink,
            _ => EntryKind::Other,
        };
        Ok(PathEntry {
            path: path.to_path_buf(),
            kind,
            target: Some(link_target(path, &point.substitute_name)),
            identity,
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
        // The verbatim `\\?\` form `canonicalize` returns is kept verbatim. Every other layer of
        // the crate derives canonical paths the same way, and a form that differs only here would
        // make a catalog source and a discovery terminal compare unequal while naming one
        // directory. Namespace differences are folded at comparison time instead, by
        // `ComparablePath` and `targets_match`.
        Ok(canonical)
    }

    fn create_directory_link(&self, request: &LinkRequest) -> Result<CreatedLink, LinkError> {
        let source_canonical = self
            .canonical_directory(&request.source)
            .map_err(|error| create_error(request, error.to_string()))?;

        if request.mode == LinkMode::Junction {
            check_junction_eligibility(self, request, &source_canonical)?;
            return self.create_junction(request, &source_canonical);
        }

        let error = match std::os::windows::fs::symlink_dir(&source_canonical, &request.staged_path)
        {
            Ok(()) => {
                // Same rollback as `create_junction`: `RemoveDirectoryW` is also what detaches
                // a directory symbolic link.
                let created = self
                    .inspect_no_follow(&request.staged_path)
                    .inspect_err(|_| {
                        let _ = windows_ffi::remove_directory_entry(&request.staged_path);
                    })?;
                return Ok(CreatedLink {
                    path: request.staged_path.clone(),
                    kind: CreatedLinkKind::Symlink,
                    target: source_canonical.clone(),
                    source_canonical,
                    identity: created.identity,
                });
            }
            Err(error) => error,
        };

        match classify_symlink_failure(request.mode, &error) {
            SymlinkFailure::FallBackToJunction => {
                check_junction_eligibility(self, request, &source_canonical)?;
                self.create_junction(request, &source_canonical)
            }
            SymlinkFailure::MissingPrivilege => Err(create_error(
                request,
                format!(
                    "creating a directory symbolic link needs Developer Mode or an elevated \
                     process: {error}"
                ),
            )),
            SymlinkFailure::Propagate => Err(create_error(request, error.to_string())),
        }
    }

    fn create_directory(&self, path: &Path) -> Result<OwnedDirectory, LinkError> {
        super::create_directory_entry(self, path)
    }

    fn place_path_no_replace(
        &self,
        staged: &Path,
        destination: &Path,
    ) -> Result<PathPlacement, LinkError> {
        match windows_ffi::rename_no_replace(staged, destination) {
            Ok(()) => Ok(PathPlacement::Placed),
            Err(error) if error.kind() == io::ErrorKind::AlreadyExists => {
                Ok(PathPlacement::DestinationExists)
            }
            Err(error) => Err(LinkError::Place {
                staged: staged.to_path_buf(),
                destination: destination.to_path_buf(),
                reason: error.to_string(),
            }),
        }
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
            // `RemoveDirectoryW` detaches a reparse point. It never descends, which is why no code
            // path here may reach for a recursive removal.
            Ownership::Owned => windows_ffi::remove_directory_entry(&recorded.path)
                .map(|()| RemoveOutcome::Removed)
                .map_err(|error| LinkError::Remove {
                    path: recorded.path.clone(),
                    reason: error.to_string(),
                }),
        }
    }
}

impl WindowsBackend {
    /// Creates a junction and proves the created entry resolves where it was meant to.
    ///
    /// A junction is an empty directory carrying a mount-point reparse buffer. If anything after
    /// the directory is created fails, that directory is removed again: `RemoveDirectoryW` on an
    /// empty directory or a reparse point cannot touch the source.
    fn create_junction(
        &self,
        request: &LinkRequest,
        source_canonical: &Path,
    ) -> Result<CreatedLink, LinkError> {
        // `create_dir` fails when the staged path exists, which keeps creation no-replace.
        fs::create_dir(&request.staged_path)
            .map_err(|error| create_error(request, error.to_string()))?;

        self.write_junction_data(request, source_canonical)
            .inspect_err(|_| {
                let _ = windows_ffi::remove_directory_entry(&request.staged_path);
            })
    }

    fn write_junction_data(
        &self,
        request: &LinkRequest,
        source_canonical: &Path,
    ) -> Result<CreatedLink, LinkError> {
        let source_wide = to_wide(source_canonical);
        let substitute_name = winpath::to_nt_substitute_name(&source_wide);
        let buffer = reparse::build_mount_point(&substitute_name, &source_wide)
            .map_err(|error| create_error(request, error.to_string()))?;

        let handle = windows_ffi::open_no_follow(&request.staged_path, Access::WriteReparseData)
            .map_err(|error| create_error(request, error.to_string()))?;
        windows_ffi::write_reparse_point(&handle, &buffer)
            .map_err(|error| create_error(request, error.to_string()))?;
        drop(handle);

        // The buffer is written by hand, so the created entry is read back and checked rather than
        // assumed. A junction that decodes to the wrong path would silently mount the wrong Skill.
        let created = self.inspect_no_follow(&request.staged_path)?;
        if created.kind != EntryKind::Junction {
            return Err(create_error(
                request,
                format!(
                    "the created entry is a {} rather than a junction",
                    created.kind.label()
                ),
            ));
        }
        let resolves_to_source = created
            .target
            .as_ref()
            .is_some_and(|target| targets_match(source_canonical, &target.resolved));
        if !resolves_to_source {
            return Err(create_error(
                request,
                "the created junction does not resolve to its intended source".to_owned(),
            ));
        }

        Ok(CreatedLink {
            path: request.staged_path.clone(),
            kind: CreatedLinkKind::Junction,
            target: from_wide(&substitute_name),
            source_canonical: source_canonical.to_path_buf(),
            identity: created.identity,
        })
    }
}

/// Decides whether a junction may be created, given a canonical source and a destination state.
///
/// A junction stores a local NT device path. Against a UNC or network-backed source it would name
/// something the object manager cannot resolve, so the request is refused instead of producing an
/// entry that looks right and resolves to nothing.
///
/// The rule is a pure function of two values so it can be proved without a network share, which no
/// CI runner reliably has. The backend supplies the observed values; this decides.
pub(super) fn junction_eligibility(
    source_canonical: &Path,
    destination: EntryKind,
) -> Result<(), &'static str> {
    let source_wide = to_wide(source_canonical);
    if winpath::is_unc(&source_wide) {
        return Err("a junction cannot point at a UNC or network path");
    }
    if !winpath::is_local_absolute(&source_wide) {
        return Err("a junction can only point at a fully qualified local path");
    }
    if destination != EntryKind::Missing {
        return Err("the staged path is already occupied");
    }
    Ok(())
}

/// Applies [`junction_eligibility`] to a live request.
fn check_junction_eligibility(
    backend: &WindowsBackend,
    request: &LinkRequest,
    source_canonical: &Path,
) -> Result<(), LinkError> {
    let destination = backend.inspect_no_follow(&request.staged_path)?.kind;
    junction_eligibility(source_canonical, destination)
        .map_err(|reason| create_error(request, reason.to_owned()))
}

/// What a failed `symlink_dir` call means for the request that produced it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum SymlinkFailure {
    /// Automatic mode met the one failure a junction can work around.
    FallBackToJunction,
    /// A symbolic link was asked for by name and the host will not grant the privilege.
    MissingPrivilege,
    /// Anything else. The original error is reported unchanged.
    Propagate,
}

/// Decides what to do about a failed directory-symlink creation.
///
/// This is a pure function of the mode and the operating-system error so that the rule is provable
/// on any runner. It matters: the composite path — automatic mode meeting a real privilege refusal
/// and producing a junction — can only be observed on a host that actually lacks the privilege, and
/// a CI runner that happens to have Developer Mode would otherwise leave the decision untested.
pub(super) fn classify_symlink_failure(mode: LinkMode, error: &io::Error) -> SymlinkFailure {
    if !is_privilege_failure(error) {
        // A full disk or a denied ACL is not something a different link implementation fixes.
        // Falling back here would turn an unrelated fault into a silently different mount.
        return SymlinkFailure::Propagate;
    }
    match mode {
        LinkMode::Auto => SymlinkFailure::FallBackToJunction,
        LinkMode::Symlink | LinkMode::Junction => SymlinkFailure::MissingPrivilege,
    }
}

/// Returns whether the failure is specifically the missing symbolic-link privilege.
pub(super) fn is_privilege_failure(error: &io::Error) -> bool {
    error.raw_os_error() == i32::try_from(ERROR_PRIVILEGE_NOT_HELD).ok()
}

/// Renders whichever identity the volume reported.
///
/// The two forms are tagged apart rather than merged: the legacy 64-bit index is documented as
/// neither unique on `ReFS` nor safe from reuse after deletion, so an entry read one way must never
/// look identical to an entry read the other.
fn platform_identity(identity: windows_ffi::FileIdentity) -> PlatformIdentity {
    match identity {
        windows_ffi::FileIdentity::Wide { volume, id } => {
            PlatformIdentity::new("win-id128", volume, &id)
        }
        windows_ffi::FileIdentity::Legacy { volume, index } => {
            PlatformIdentity::from_pair("win-index64", u64::from(volume), index)
        }
    }
}

/// Classifies an entry from its attributes alone, for everything that does not redirect.
fn attribute_kind(metadata: &fs::Metadata) -> EntryKind {
    if metadata.is_dir() {
        EntryKind::Directory
    } else if metadata.is_file() {
        EntryKind::File
    } else {
        EntryKind::Other
    }
}

/// Builds the immediate target of a reparse point from its substitute name.
pub(super) fn link_target(path: &Path, substitute_name: &[u16]) -> LinkTarget {
    let raw = from_wide(substitute_name);
    // The substitute name carries the `\??\` object-namespace prefix that no ordinary path API
    // accepts, so the usable form has it removed. A relative symbolic-link target has no prefix
    // and resolves against the directory holding the link, never the current directory.
    let usable = from_wide(&winpath::comparison_key(substitute_name));
    let resolved = if usable.is_absolute() {
        usable
    } else {
        path.parent().unwrap_or_else(|| Path::new("")).join(usable)
    };
    LinkTarget { raw, resolved }
}

fn to_wide(path: &Path) -> Vec<u16> {
    path.as_os_str().encode_wide().collect()
}

fn from_wide(wide: &[u16]) -> PathBuf {
    PathBuf::from(std::ffi::OsString::from_wide(wide))
}

fn inspect_error(path: &Path, error: &io::Error) -> LinkError {
    LinkError::Inspect {
        path: path.to_path_buf(),
        reason: error.to_string(),
    }
}

fn create_error(request: &LinkRequest, reason: String) -> LinkError {
    LinkError::Create {
        destination: request.staged_path.clone(),
        source: request.source.clone(),
        reason,
    }
}
