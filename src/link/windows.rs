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
use std::os::windows::io::OwnedHandle;
use std::path::{Path, PathBuf};

use windows_sys::Win32::Foundation::ERROR_PRIVILEGE_NOT_HELD;
use windows_sys::Win32::Storage::FileSystem::{
    FILE_ATTRIBUTE_DIRECTORY, FILE_ATTRIBUTE_REPARSE_POINT,
};

use crate::domain::LinkMode;
use crate::error::LinkError;
use crate::link::resolve::targets_match;
use crate::link::{
    CreatedLink, CreatedLinkKind, EntryKind, LinkBackend, LinkRequest, LinkTarget, OwnedDirectory,
    Ownership, PathEntry, PlacementMismatch, PlacementOutcome, PlacementResidue, PlatformIdentity,
    RemoveOutcome, directory_placement_mismatch, link_placement_mismatch, sealed,
    verify_directory_ownership, verify_ownership,
};

use super::reparse::{self, IO_REPARSE_TAG_MOUNT_POINT, IO_REPARSE_TAG_SYMLINK};
use super::windows_ffi::{self, Access};
use super::winpath;

/// The Windows backend.
pub(super) struct WindowsBackend;

/// One entry observed through the same no-follow handle retained for a possible mutation.
struct OpenedEntry {
    handle: OwnedHandle,
    entry: PathEntry,
}

impl sealed::Sealed for WindowsBackend {}

impl LinkBackend for WindowsBackend {
    fn inspect_no_follow(&self, path: &Path) -> Result<PathEntry, LinkError> {
        Ok(Self::open_entry(path, Access::Inspect)?.map_or_else(
            || PathEntry::plain(path, EntryKind::Missing),
            |opened| opened.entry,
        ))
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
            return Self::create_junction(request, &source_canonical);
        }

        let error = match std::os::windows::fs::symlink_dir(&source_canonical, &request.staged_path)
        {
            Ok(()) => {
                // `CreateSymbolicLinkW` returns status rather than a handle. The first no-follow
                // open below establishes evidence for later operations but cannot prove continuity
                // from the create call; ADR 0015 records that residual window.
                #[cfg(test)]
                if let Err(error) = super::testing::reach_hook(
                    super::testing::HookPoint::AfterLinkCreation,
                    &request.staged_path,
                    None,
                ) {
                    return Err(retained_create_error(request, &error.to_string()));
                }
                let opened = match Self::open_entry(&request.staged_path, Access::Remove) {
                    Ok(Some(opened)) => opened,
                    Ok(None) => {
                        return Err(retained_create_error(
                            request,
                            "the created symbolic link disappeared before ownership could be proved",
                        ));
                    }
                    Err(error) => {
                        return Err(retained_create_error(request, &error.to_string()));
                    }
                };
                let target = match opened.entry.target.as_ref() {
                    Some(target)
                        if opened.entry.kind == EntryKind::Symlink
                            && targets_match(&source_canonical, &target.raw) =>
                    {
                        target.raw.clone()
                    }
                    _ => {
                        return Err(retained_create_error(
                            request,
                            "the initial staged entry observation did not match the required symbolic link",
                        ));
                    }
                };
                #[cfg(test)]
                if let Err(error) = super::testing::reach_hook(
                    super::testing::HookPoint::AfterLinkVerification,
                    &request.staged_path,
                    None,
                ) {
                    return Err(rollback_create_error(
                        request,
                        &opened.handle,
                        &error.to_string(),
                    ));
                }
                return Ok(CreatedLink {
                    path: request.staged_path.clone(),
                    kind: CreatedLinkKind::Symlink,
                    target,
                    source_canonical,
                    identity: opened.entry.identity,
                });
            }
            Err(error) => error,
        };

        match classify_symlink_failure(request.mode, &error) {
            SymlinkFailure::FallBackToJunction => {
                check_junction_eligibility(self, request, &source_canonical)?;
                Self::create_junction(request, &source_canonical)
            }
            SymlinkFailure::MissingPrivilege => Err(symlink_privilege_error(
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
        fs::create_dir(path).map_err(|error| directory_create_error(path, error.to_string()))?;
        // `CreateDirectoryW` returns status rather than a handle. ADR 0015 scopes ownership
        // evidence to the first no-follow open below rather than claiming object continuity here.
        #[cfg(test)]
        if let Err(error) = super::testing::reach_hook(
            super::testing::HookPoint::AfterDirectoryCreation,
            path,
            None,
        ) {
            return Err(retained_directory_create_error(path, &error.to_string()));
        }
        let opened = match Self::open_entry(path, Access::Inspect) {
            Ok(Some(opened)) => opened,
            Ok(None) => {
                return Err(retained_directory_create_error(
                    path,
                    "the created directory disappeared before ownership could be proved",
                ));
            }
            Err(error) => {
                return Err(retained_directory_create_error(path, &error.to_string()));
            }
        };
        if opened.entry.kind != EntryKind::Directory || opened.entry.identity.is_none() {
            return Err(retained_directory_create_error(
                path,
                "the initial staged entry observation was not a directory with stable identity",
            ));
        }
        Ok(OwnedDirectory {
            path: path.to_path_buf(),
            identity: opened.entry.identity,
        })
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
        let Some(opened) = Self::open_entry(&staged.path, Access::Delete)? else {
            return Ok(PlacementOutcome::OwnershipMismatch(PlacementResidue {
                path: staged.path.clone(),
                mismatch: PlacementMismatch::Missing,
            }));
        };
        if let Some(mismatch) = link_placement_mismatch(&opened.entry, staged, |target| {
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
        if !rename_opened_no_replace(&opened.handle, &staged.path, destination)? {
            let live = self.inspect_no_follow(&staged.path)?;
            if let Some(mismatch) = link_placement_mismatch(&live, staged, |target| {
                targets_match(&staged.target, &target.raw)
            }) {
                return Ok(PlacementOutcome::OwnershipMismatch(PlacementResidue {
                    path: staged.path.clone(),
                    mismatch,
                }));
            }
            return Ok(PlacementOutcome::DestinationExists);
        }
        #[cfg(test)]
        super::testing::reach_hook(
            super::testing::HookPoint::AfterPlacementMutation,
            destination,
            None,
        )?;

        let mut placed = staged.relocated_to(destination);
        placed.identity.clone_from(&opened.entry.identity);
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
        let Some(opened) = Self::open_entry(&staged.path, Access::Delete)? else {
            return Ok(PlacementOutcome::OwnershipMismatch(PlacementResidue {
                path: staged.path.clone(),
                mismatch: PlacementMismatch::Missing,
            }));
        };
        if let Some(mismatch) = directory_placement_mismatch(&opened.entry, staged) {
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
        if !rename_opened_no_replace(&opened.handle, &staged.path, destination)? {
            let live = self.inspect_no_follow(&staged.path)?;
            if let Some(mismatch) = directory_placement_mismatch(&live, staged) {
                return Ok(PlacementOutcome::OwnershipMismatch(PlacementResidue {
                    path: staged.path.clone(),
                    mismatch,
                }));
            }
            return Ok(PlacementOutcome::DestinationExists);
        }
        #[cfg(test)]
        super::testing::reach_hook(
            super::testing::HookPoint::AfterPlacementMutation,
            destination,
            None,
        )?;

        let placed = OwnedDirectory {
            path: destination.to_path_buf(),
            identity: opened.entry.identity.clone(),
        };
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
        let Some(opened) = Self::open_entry(&recorded.path, Access::Remove)? else {
            return Ok(RemoveOutcome::AlreadyAbsent);
        };
        match verify_directory_ownership(&opened.entry, recorded) {
            Ownership::Absent => Ok(RemoveOutcome::AlreadyAbsent),
            Ownership::Mismatch(mismatch) => Ok(RemoveOutcome::OwnershipMismatch(mismatch)),
            Ownership::Owned => {
                #[cfg(test)]
                super::testing::reach_hook(
                    super::testing::HookPoint::BeforeRemovalMutation,
                    &recorded.path,
                    None,
                )?;
                match windows_ffi::delete_by_handle(&opened.handle) {
                    Ok(()) => {
                        let removed_identity = opened.entry.identity.clone();
                        drop(opened);
                        self.confirm_removal(&recorded.path, removed_identity.as_ref())
                    }
                    Err(error) if super::is_not_empty(&error) => Ok(RemoveOutcome::NotEmpty),
                    Err(error) => Err(LinkError::Remove {
                        path: recorded.path.clone(),
                        reason: error.to_string(),
                    }),
                }
            }
        }
    }

    fn remove_link_entry(&self, recorded: &CreatedLink) -> Result<RemoveOutcome, LinkError> {
        let Some(opened) = Self::open_entry(&recorded.path, Access::Remove)? else {
            return Ok(RemoveOutcome::AlreadyAbsent);
        };
        match verify_ownership(&opened.entry, recorded, |target| {
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
                windows_ffi::delete_by_handle(&opened.handle).map_err(|error| {
                    LinkError::Remove {
                        path: recorded.path.clone(),
                        reason: error.to_string(),
                    }
                })?;
                let removed_identity = opened.entry.identity.clone();
                drop(opened);
                self.confirm_removal(&recorded.path, removed_identity.as_ref())
            }
        }
    }
}

/// Renames the already-verified open object, returning `false` for destination contention.
fn rename_opened_no_replace(
    handle: &OwnedHandle,
    staged: &Path,
    destination: &Path,
) -> Result<bool, LinkError> {
    match windows_ffi::rename_by_handle_no_replace(handle, destination) {
        Ok(()) => Ok(true),
        Err(error) if error.kind() == io::ErrorKind::AlreadyExists => Ok(false),
        Err(error) => Err(LinkError::Place {
            staged: staged.to_path_buf(),
            destination: destination.to_path_buf(),
            reason: error.to_string(),
        }),
    }
}

impl WindowsBackend {
    /// Confirms that closing a successful POSIX-disposition handle removed the recorded object.
    ///
    /// A different entry may legitimately take the old pathname immediately after the close. Its
    /// different identity proves the disposed object is no longer there without treating the new
    /// occupant as an error or attempting to remove it.
    fn confirm_removal(
        &self,
        path: &Path,
        removed_identity: Option<&PlatformIdentity>,
    ) -> Result<RemoveOutcome, LinkError> {
        let live = self
            .inspect_no_follow(path)
            .map_err(|error| LinkError::Remove {
                path: path.to_path_buf(),
                reason: format!(
                    "handle disposition succeeded but namespace removal could not be confirmed: \
                     {error}"
                ),
            })?;
        if live.kind == EntryKind::Missing || live.identity.as_ref() != removed_identity {
            return Ok(RemoveOutcome::Removed);
        }
        Err(LinkError::Remove {
            path: path.to_path_buf(),
            reason: "handle disposition returned success but the recorded entry remained visible \
                     after the handle closed"
                .to_owned(),
        })
    }

    /// Opens and fully observes one entry without following a reparse point.
    ///
    /// Every required value comes from `handle`. The caller may discard it for read-only
    /// inspection or retain it through a rename/disposition mutation.
    fn open_entry(path: &Path, access: Access) -> Result<Option<OpenedEntry>, LinkError> {
        let handle = match windows_ffi::open_no_follow(path, access) {
            Ok(handle) => handle,
            Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(None),
            Err(error) => return Err(inspect_error(path, &error)),
        };
        let entry = Self::observe_handle(path, &handle)?;
        Ok(Some(OpenedEntry { handle, entry }))
    }

    /// Builds a complete observation from a handle the caller already owns.
    fn observe_handle(path: &Path, handle: &OwnedHandle) -> Result<PathEntry, LinkError> {
        let information =
            windows_ffi::entry_information(handle).map_err(|error| inspect_error(path, &error))?;
        let identity = Some(platform_identity(information.identity));
        let plain = |kind| PathEntry {
            path: path.to_path_buf(),
            kind,
            target: None,
            identity: identity.clone(),
        };

        let entry = if information.attributes & FILE_ATTRIBUTE_REPARSE_POINT == 0 {
            plain(attribute_kind(information.attributes))
        } else {
            let buffer = windows_ffi::read_reparse_point(handle)
                .map_err(|error| inspect_error(path, &error))?;
            match reparse::parse(&buffer) {
                Ok(point) => {
                    let kind = match point.tag {
                        IO_REPARSE_TAG_MOUNT_POINT => EntryKind::Junction,
                        IO_REPARSE_TAG_SYMLINK => EntryKind::Symlink,
                        _ => EntryKind::Other,
                    };
                    PathEntry {
                        path: path.to_path_buf(),
                        kind,
                        target: Some(link_target(path, &point.substitute_name)),
                        identity: identity.clone(),
                    }
                }
                // Reparse tags without the name-surrogate bit annotate an ordinary file or
                // directory rather than redirecting it. Cloud placeholders and deduplication
                // entries must remain usable as ordinary Skill stores.
                Err(reparse::ReparseError::UnsupportedTag(tag))
                    if !reparse::is_name_surrogate(tag) =>
                {
                    plain(attribute_kind(information.attributes))
                }
                // An unknown name surrogate redirects somewhere this backend cannot describe. It
                // is an observed but unsupported entry, distinct from an I/O or decoder failure.
                Err(reparse::ReparseError::UnsupportedTag(_)) => plain(EntryKind::Other),
                Err(error) => {
                    return Err(LinkError::Inspect {
                        path: path.to_path_buf(),
                        reason: format!("could not decode reparse data: {error}"),
                    });
                }
            }
        };

        Ok(entry)
    }

    /// Creates a junction and establishes handle evidence that it resolves where it was meant to.
    ///
    /// A junction is an empty directory carrying a mount-point reparse buffer. Once the new
    /// directory is opened and identified, the same handle writes, reads back, and if necessary
    /// rolls back the entry. A failure before that evidence exists leaves the pathname untouched.
    /// ADR 0015 records that a replacement crossing the status-only create boundary can therefore
    /// be adopted and receive the mount-point data before SkillMount can distinguish it.
    fn create_junction(
        request: &LinkRequest,
        source_canonical: &Path,
    ) -> Result<CreatedLink, LinkError> {
        // `create_dir` fails when the staged path exists, which keeps creation no-replace.
        fs::create_dir(&request.staged_path)
            .map_err(|error| create_error(request, error.to_string()))?;
        // `CreateDirectoryW` returns status rather than a handle. ADR 0015 scopes ownership
        // evidence to the first no-follow open below rather than claiming object continuity here.
        #[cfg(test)]
        if let Err(error) = super::testing::reach_hook(
            super::testing::HookPoint::AfterDirectoryCreation,
            &request.staged_path,
            None,
        ) {
            return Err(retained_create_error(request, &error.to_string()));
        }
        let opened = match Self::open_entry(&request.staged_path, Access::WriteReparseData) {
            Ok(Some(opened)) => opened,
            Ok(None) => {
                return Err(retained_create_error(
                    request,
                    "the created junction directory disappeared before ownership could be proved",
                ));
            }
            Err(error) => return Err(retained_create_error(request, &error.to_string())),
        };
        if opened.entry.kind != EntryKind::Directory || opened.entry.identity.is_none() {
            return Err(retained_create_error(
                request,
                "the initial staged entry observation was not a directory with stable identity",
            ));
        }

        let source_wide = to_wide(source_canonical);
        let substitute_name = winpath::to_nt_substitute_name(&source_wide);
        let buffer = match reparse::build_mount_point(&substitute_name, &source_wide) {
            Ok(buffer) => buffer,
            Err(error) => {
                return Err(rollback_create_error(
                    request,
                    &opened.handle,
                    &error.to_string(),
                ));
            }
        };
        if let Err(error) = windows_ffi::write_reparse_point(&opened.handle, &buffer) {
            return Err(rollback_create_error(
                request,
                &opened.handle,
                &error.to_string(),
            ));
        }
        #[cfg(test)]
        if let Err(error) = super::testing::reach_hook(
            super::testing::HookPoint::AfterLinkCreation,
            &request.staged_path,
            None,
        ) {
            return Err(rollback_create_error(
                request,
                &opened.handle,
                &error.to_string(),
            ));
        }

        Self::finish_junction_creation(request, source_canonical, &opened)
    }

    /// Reads back and validates the junction through the handle that wrote it.
    fn finish_junction_creation(
        request: &LinkRequest,
        source_canonical: &Path,
        opened: &OpenedEntry,
    ) -> Result<CreatedLink, LinkError> {
        // The buffer is written by hand, so the created entry is read back and checked rather than
        // assumed. A junction that decodes to the wrong path would silently mount the wrong Skill.
        let created = match Self::observe_handle(&request.staged_path, &opened.handle) {
            Ok(created) => created,
            Err(error) => {
                return Err(rollback_create_error(
                    request,
                    &opened.handle,
                    &error.to_string(),
                ));
            }
        };
        if created.kind != EntryKind::Junction {
            return Err(rollback_create_error(
                request,
                &opened.handle,
                &format!(
                    "the created entry is a {} rather than a junction",
                    created.kind.label()
                ),
            ));
        }
        let Some(created_target) = created.target.as_ref() else {
            return Err(rollback_create_error(
                request,
                &opened.handle,
                "the created junction has no reparse target",
            ));
        };
        if !targets_match(source_canonical, &created_target.resolved) {
            return Err(rollback_create_error(
                request,
                &opened.handle,
                "the created junction does not resolve to its intended source",
            ));
        }
        #[cfg(test)]
        if let Err(error) = super::testing::reach_hook(
            super::testing::HookPoint::AfterLinkVerification,
            &request.staged_path,
            None,
        ) {
            return Err(rollback_create_error(
                request,
                &opened.handle,
                &error.to_string(),
            ));
        }

        Ok(CreatedLink {
            path: request.staged_path.clone(),
            kind: CreatedLinkKind::Junction,
            target: created_target.raw.clone(),
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

/// Classifies an entry from handle attributes, for everything that does not redirect.
fn attribute_kind(attributes: u32) -> EntryKind {
    if attributes & FILE_ATTRIBUTE_DIRECTORY != 0 {
        EntryKind::Directory
    } else {
        EntryKind::File
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

pub(super) fn symlink_privilege_error(request: &LinkRequest, reason: String) -> LinkError {
    LinkError::SymlinkPrivilegeUnavailable {
        destination: request.staged_path.clone(),
        source: request.source.clone(),
        reason,
    }
}

/// Reports a creation failure without issuing a pathname rollback against an unproved entry.
fn retained_create_error(request: &LinkRequest, reason: &str) -> LinkError {
    create_error(
        request,
        format!(
            "{reason}; no pathname rollback was attempted because ownership could not be proved; \
             inspect the retained staged path {}",
            request.staged_path.display()
        ),
    )
}

/// Rolls back the entry whose initial evidence is bound to `handle` and preserves both failures.
fn rollback_create_error(request: &LinkRequest, handle: &OwnedHandle, reason: &str) -> LinkError {
    match windows_ffi::delete_by_handle(handle) {
        Ok(()) => create_error(
            request,
            format!("{reason}; the verified staged entry was rolled back through its handle"),
        ),
        Err(rollback) => create_error(
            request,
            format!(
                "{reason}; handle-bound rollback failed: {rollback}; retained staged path {}",
                request.staged_path.display()
            ),
        ),
    }
}

fn directory_create_error(path: &Path, reason: String) -> LinkError {
    LinkError::Create {
        destination: path.to_path_buf(),
        source: path.to_path_buf(),
        reason,
    }
}

fn retained_directory_create_error(path: &Path, reason: &str) -> LinkError {
    directory_create_error(
        path,
        format!(
            "{reason}; no pathname rollback was attempted because ownership could not be proved; \
             inspect the retained staged path {}",
            path.display()
        ),
    )
}
