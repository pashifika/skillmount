//! The audited Windows system-call surface.
//!
//! This is the only Windows module allowed to contain `unsafe`, and it is deliberately the
//! smallest module in the crate that could do the job. Each function wraps exactly one Win32 call,
//! converts its result into an `io::Error` immediately, and returns owned Rust values. No
//! `windows_sys` type crosses back out of it, so a reviewer auditing the raw-pointer surface reads
//! this file and nothing else.
//!
//! Four things the standard library cannot do bring us here: opening a directory without following
//! its reparse point, reading and writing a reparse buffer, reading a stable file identity, and
//! renaming without replacement. Everything else in the Windows backend uses safe `std::fs`.
//!
//! Handles are wrapped in [`std::os::windows::io::OwnedHandle`] at the boundary, so closing them is
//! std's responsibility and no `Drop` implementation here can leak or double-close one.

#![allow(unsafe_code)]

use std::io;
use std::os::windows::ffi::OsStrExt;
use std::os::windows::io::{AsRawHandle, FromRawHandle, OwnedHandle};
use std::path::Path;
use std::ptr;

use windows_sys::Win32::Foundation::{FILETIME, GENERIC_WRITE, HANDLE, INVALID_HANDLE_VALUE};
use windows_sys::Win32::Storage::FileSystem::{
    BY_HANDLE_FILE_INFORMATION, CreateFileW, FILE_FLAG_BACKUP_SEMANTICS,
    FILE_FLAG_OPEN_REPARSE_POINT, FILE_READ_ATTRIBUTES, FILE_SHARE_DELETE, FILE_SHARE_READ,
    FILE_SHARE_WRITE, GetFileInformationByHandle, MoveFileExW, OPEN_EXISTING, RemoveDirectoryW,
};
use windows_sys::Win32::System::IO::DeviceIoControl;
use windows_sys::Win32::System::Ioctl::{FSCTL_GET_REPARSE_POINT, FSCTL_SET_REPARSE_POINT};

use super::reparse::MAXIMUM_REPARSE_DATA_BUFFER_SIZE;
use super::winpath;

/// Whether a handle is opened to read an entry or to rewrite its reparse data.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum Access {
    /// Enough to read attributes, identity, and reparse data.
    Inspect,
    /// Enough to write a reparse buffer, which Windows requires `GENERIC_WRITE` for.
    WriteReparseData,
}

impl Access {
    const fn desired(self) -> u32 {
        match self {
            Self::Inspect => FILE_READ_ATTRIBUTES,
            Self::WriteReparseData => GENERIC_WRITE,
        }
    }
}

/// Opens an entry without traversing it.
///
/// `FILE_FLAG_OPEN_REPARSE_POINT` is what makes this a no-follow open: without it the call lands
/// on the link's target and every identity and reparse read afterwards describes the wrong entry.
/// `FILE_FLAG_BACKUP_SEMANTICS` is required to open a directory at all.
///
/// # Errors
///
/// Returns the operating-system error, including [`io::ErrorKind::NotFound`] for a missing entry.
pub(super) fn open_no_follow(path: &Path, access: Access) -> io::Result<OwnedHandle> {
    let wide = to_wide(path);
    // SAFETY: `wide` is NUL-terminated and outlives the call. The security-attributes and template
    // arguments are null, which the API documents as "defaults, no template". The returned handle
    // is checked against `INVALID_HANDLE_VALUE` before it is adopted.
    let handle = unsafe {
        CreateFileW(
            wide.as_ptr(),
            access.desired(),
            FILE_SHARE_READ | FILE_SHARE_WRITE | FILE_SHARE_DELETE,
            ptr::null(),
            OPEN_EXISTING,
            FILE_FLAG_BACKUP_SEMANTICS | FILE_FLAG_OPEN_REPARSE_POINT,
            ptr::null_mut(),
        )
    };
    if handle == INVALID_HANDLE_VALUE {
        return Err(io::Error::last_os_error());
    }
    // SAFETY: `handle` is a valid, freshly opened handle this function exclusively owns, and it is
    // handed over exactly once. `OwnedHandle` closes it on drop.
    Ok(unsafe { OwnedHandle::from_raw_handle(handle.cast()) })
}

/// Reads the volume serial number and the 64-bit file index of an open entry.
///
/// Together these identify one entry on one volume, which is what ownership verification needs:
/// the same path can be a different entry a moment later, but the same identity cannot.
///
/// # Errors
///
/// Returns the operating-system error when the information is unavailable.
pub(super) fn file_identity(handle: &OwnedHandle) -> io::Result<(u64, u64)> {
    let mut information = BY_HANDLE_FILE_INFORMATION {
        dwFileAttributes: 0,
        ftCreationTime: FILETIME {
            dwLowDateTime: 0,
            dwHighDateTime: 0,
        },
        ftLastAccessTime: FILETIME {
            dwLowDateTime: 0,
            dwHighDateTime: 0,
        },
        ftLastWriteTime: FILETIME {
            dwLowDateTime: 0,
            dwHighDateTime: 0,
        },
        dwVolumeSerialNumber: 0,
        nFileSizeHigh: 0,
        nFileSizeLow: 0,
        nNumberOfLinks: 0,
        nFileIndexHigh: 0,
        nFileIndexLow: 0,
    };
    // SAFETY: the handle is live for the whole call because it is borrowed, and `information` is a
    // fully initialized value of exactly the type the API writes into.
    let succeeded = unsafe { GetFileInformationByHandle(raw(handle), &raw mut information) };
    if succeeded == 0 {
        return Err(io::Error::last_os_error());
    }
    let index =
        (u64::from(information.nFileIndexHigh) << 32) | u64::from(information.nFileIndexLow);
    Ok((u64::from(information.dwVolumeSerialNumber), index))
}

/// Reads the raw reparse buffer of an open entry.
///
/// # Errors
///
/// Returns the operating-system error, including `ERROR_NOT_A_REPARSE_POINT` for an ordinary
/// directory, which the caller treats as "this is not a link" rather than as a failure.
pub(super) fn read_reparse_point(handle: &OwnedHandle) -> io::Result<Vec<u8>> {
    let mut buffer = vec![0u8; MAXIMUM_REPARSE_DATA_BUFFER_SIZE];
    let capacity = u32::try_from(buffer.len()).map_err(|_| {
        io::Error::new(
            io::ErrorKind::InvalidInput,
            "the reparse buffer is larger than the API accepts",
        )
    })?;
    let mut returned: u32 = 0;
    // SAFETY: `buffer` is a live allocation of exactly `capacity` bytes and is not aliased. The
    // input arguments are null with a zero length, which this control code expects. `returned`
    // receives the byte count and is read only after the call reports success.
    let succeeded = unsafe {
        DeviceIoControl(
            raw(handle),
            FSCTL_GET_REPARSE_POINT,
            ptr::null(),
            0,
            buffer.as_mut_ptr().cast(),
            capacity,
            &raw mut returned,
            ptr::null_mut(),
        )
    };
    if succeeded == 0 {
        return Err(io::Error::last_os_error());
    }
    buffer.truncate(returned as usize);
    Ok(buffer)
}

/// Writes a reparse buffer, turning an empty directory into a junction.
///
/// # Errors
///
/// Returns the operating-system error when the entry cannot be turned into a reparse point.
pub(super) fn write_reparse_point(handle: &OwnedHandle, buffer: &[u8]) -> io::Result<()> {
    let length = u32::try_from(buffer.len()).map_err(|_| {
        io::Error::new(
            io::ErrorKind::InvalidInput,
            "the reparse buffer is larger than the API accepts",
        )
    })?;
    let mut returned: u32 = 0;
    // SAFETY: `buffer` is a live, immutable slice of exactly `length` bytes. The output arguments
    // are null with a zero length, which this control code expects; `returned` is still a valid
    // pointer because the API requires one even though it writes zero.
    let succeeded = unsafe {
        DeviceIoControl(
            raw(handle),
            FSCTL_SET_REPARSE_POINT,
            buffer.as_ptr().cast(),
            length,
            ptr::null_mut(),
            0,
            &raw mut returned,
            ptr::null_mut(),
        )
    };
    if succeeded == 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(())
}

/// Atomically renames `from` to `to` on the same volume, failing if `to` already exists.
///
/// `std::fs::rename` passes `MOVEFILE_REPLACE_EXISTING`, which is exactly the behavior placement
/// must not have. Omitting the flag makes Windows fail with `ERROR_ALREADY_EXISTS` instead.
///
/// # Errors
///
/// Returns the operating-system error, including [`io::ErrorKind::AlreadyExists`] when the
/// destination is occupied.
pub(super) fn rename_no_replace(from: &Path, to: &Path) -> io::Result<()> {
    let from = to_wide(from);
    let to = to_wide(to);
    // SAFETY: both buffers are NUL-terminated and outlive the call. Passing no flags is what makes
    // the operation refuse to replace an existing destination.
    let succeeded = unsafe { MoveFileExW(from.as_ptr(), to.as_ptr(), 0) };
    if succeeded == 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(())
}

/// Removes one directory entry, which for a reparse point unlinks the link and not its target.
///
/// This is the Windows half of the promise that `SkillMount` never deletes a Skill source.
/// `RemoveDirectoryW` on a junction detaches the reparse point; it does not descend.
///
/// # Errors
///
/// Returns the operating-system error, including `ERROR_DIR_NOT_EMPTY` when the entry is a real
/// directory with contents.
pub(super) fn remove_directory_entry(path: &Path) -> io::Result<()> {
    let wide = to_wide(path);
    // SAFETY: `wide` is NUL-terminated and outlives the call.
    let succeeded = unsafe { RemoveDirectoryW(wide.as_ptr()) };
    if succeeded == 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(())
}

/// Returns the raw handle for one call, without transferring ownership.
fn raw(handle: &OwnedHandle) -> HANDLE {
    handle.as_raw_handle().cast()
}

/// Encodes a path as the NUL-terminated extended-form wide string every call above expects.
fn to_wide(path: &Path) -> Vec<u16> {
    let encoded = path.as_os_str().encode_wide().collect::<Vec<_>>();
    let mut wide = winpath::to_extended(&encoded);
    wide.push(0);
    wide
}
