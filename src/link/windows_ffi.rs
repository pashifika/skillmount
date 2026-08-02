//! The audited Windows system-call surface.
//!
//! This is the only Windows module allowed to contain `unsafe`, and it is deliberately the
//! smallest module in the crate that could do the job. Each function wraps exactly one Win32 call,
//! converts its result into an `io::Error` immediately, and returns owned Rust values. No
//! `windows_sys` type crosses back out of it, so a reviewer auditing the raw-pointer surface reads
//! this file and nothing else.
//!
//! Five things the standard library cannot do bring us here: opening a directory without following
//! its reparse point, reading and writing a reparse buffer, reading a stable file identity,
//! renaming without replacement, and replacing a durable journal with write-through semantics.
//! Everything else in the Windows backend uses safe `std::fs`.
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
    FILE_FLAG_OPEN_REPARSE_POINT, FILE_ID_128, FILE_ID_INFO, FILE_READ_ATTRIBUTES,
    FILE_SHARE_DELETE, FILE_SHARE_READ, FILE_SHARE_WRITE, FileIdInfo, GetFileInformationByHandle,
    GetFileInformationByHandleEx, MOVEFILE_REPLACE_EXISTING, MOVEFILE_WRITE_THROUGH, MoveFileExW,
    OPEN_EXISTING, RemoveDirectoryW,
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
    let wide = to_wide(path)?;
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

/// A stable identity for an open entry, in whichever form the volume supports.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum FileIdentity {
    /// The 128-bit `FILE_ID_INFO` identity, available since Windows 8.
    ///
    /// This is the form ownership verification wants. The legacy 64-bit index below is documented
    /// as not guaranteed unique on `ReFS` and as reusable after deletion, so on those volumes it is
    /// not a lifetime capability and cannot by itself authorize a removal.
    Wide {
        /// 64-bit volume serial number.
        volume: u64,
        /// 128-bit file identifier.
        id: [u8; 16],
    },
    /// The legacy volume-serial and 64-bit index pair, for a volume that reports nothing better.
    Legacy {
        /// 32-bit volume serial number.
        volume: u32,
        /// 64-bit file index.
        index: u64,
    },
}

/// Reads the strongest identity the volume reports for an open entry.
///
/// `FILE_ID_INFO` is tried first and the legacy pair is the fallback, because a filesystem or
/// filter driver that does not implement the newer class fails the call rather than degrading.
///
/// # Errors
///
/// Returns the operating-system error when neither form is available.
pub(super) fn file_identity(handle: &OwnedHandle) -> io::Result<FileIdentity> {
    if let Some(wide) = wide_file_identity(handle) {
        return Ok(wide);
    }
    let (volume, index) = legacy_file_identity(handle)?;
    Ok(FileIdentity::Legacy { volume, index })
}

/// Reads `FILE_ID_INFO`, or `None` when the volume does not report it.
fn wide_file_identity(handle: &OwnedHandle) -> Option<FileIdentity> {
    let mut information = FILE_ID_INFO {
        VolumeSerialNumber: 0,
        FileId: FILE_ID_128 {
            Identifier: [0; 16],
        },
    };
    let size = u32::try_from(size_of::<FILE_ID_INFO>()).ok()?;
    // SAFETY: the handle is live for the whole call because it is borrowed, `information` is a
    // fully initialized value of exactly the type `FileIdInfo` writes, and `size` is that type's
    // own size. A volume that does not implement the class fails the call and writes nothing.
    let succeeded = unsafe {
        GetFileInformationByHandleEx(raw(handle), FileIdInfo, (&raw mut information).cast(), size)
    };
    if succeeded == 0 {
        return None;
    }
    Some(FileIdentity::Wide {
        volume: information.VolumeSerialNumber,
        id: information.FileId.Identifier,
    })
}

fn legacy_file_identity(handle: &OwnedHandle) -> io::Result<(u32, u64)> {
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
    Ok((information.dwVolumeSerialNumber, index))
}

/// An 8-byte-aligned reparse buffer, matching what the standard library uses for the same call.
///
/// The alignment is defensive parity, not a requirement. Both reparse control codes are
/// `METHOD_BUFFERED` — the transfer method is the low two bits of the code, and
/// `FSCTL_GET_REPARSE_POINT` is `0x0009_00A8` while `FSCTL_SET_REPARSE_POINT` is `0x0009_00A4`, so
/// both are zero. The I/O manager therefore copies each buffer through a system allocation of its
/// own and no driver ever reads or writes this memory directly.
///
/// It is kept anyway because it costs nothing, because `Align8` in
/// `library/std/src/sys/fs/windows.rs` does the same for this control code, and because a reader
/// auditing raw-pointer code should not have to re-derive the transfer method to be satisfied. The
/// *input* buffer on the write side is deliberately left as the caller's slice: aligning it would
/// mean copying 16 KiB for a guarantee the kernel does not need either.
#[repr(C, align(8))]
struct AlignedReparseBuffer([u8; MAXIMUM_REPARSE_DATA_BUFFER_SIZE]);

/// Reads the raw reparse buffer of an open entry.
///
/// # Errors
///
/// Returns the operating-system error, including `ERROR_NOT_A_REPARSE_POINT` for an ordinary
/// directory, which the caller treats as "this is not a link" rather than as a failure.
pub(super) fn read_reparse_point(handle: &OwnedHandle) -> io::Result<Vec<u8>> {
    let mut buffer = AlignedReparseBuffer([0u8; MAXIMUM_REPARSE_DATA_BUFFER_SIZE]);
    let capacity = u32::try_from(buffer.0.len()).map_err(|_| {
        io::Error::new(
            io::ErrorKind::InvalidInput,
            "the reparse buffer is larger than the API accepts",
        )
    })?;
    let mut returned: u32 = 0;
    // SAFETY: `buffer` is a live, exclusively borrowed value of exactly `capacity` bytes. The input
    // arguments are null with a zero length, which this control code expects. `returned` receives
    // the byte count and is read only after the call reports success.
    let succeeded = unsafe {
        DeviceIoControl(
            raw(handle),
            FSCTL_GET_REPARSE_POINT,
            ptr::null(),
            0,
            buffer.0.as_mut_ptr().cast(),
            capacity,
            &raw mut returned,
            ptr::null_mut(),
        )
    };
    if succeeded == 0 {
        return Err(io::Error::last_os_error());
    }
    // The decoder reads the payload byte-wise, so the alignment matters only for the call above.
    let written = usize::try_from(returned).unwrap_or(buffer.0.len());
    Ok(buffer.0.get(..written).unwrap_or(&buffer.0).to_vec())
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
    let from = to_wide(from)?;
    let to = to_wide(to)?;
    // SAFETY: both buffers are NUL-terminated and outlive the call. Passing no flags is what makes
    // the operation refuse to replace an existing destination.
    let succeeded = unsafe { MoveFileExW(from.as_ptr(), to.as_ptr(), 0) };
    if succeeded == 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(())
}

/// Atomically replaces `to` with `from` and waits for the move to reach the disk.
///
/// Microsoft documents `MOVEFILE_WRITE_THROUGH` as not returning until the file is actually moved
/// on disk. Pairing it with `MOVEFILE_REPLACE_EXISTING` supplies the journal semantics that
/// `std::fs::rename` lacks on Windows: replacement plus a durable namespace update before success.
/// No cross-volume copy fallback is enabled; the journal temporary is always a sibling of `to`.
///
/// # Errors
///
/// Returns the operating-system error when replacement or its write-through completion fails.
pub(super) fn replace_file_write_through(from: &Path, to: &Path) -> io::Result<()> {
    let from = to_wide(from)?;
    let to = to_wide(to)?;
    // SAFETY: both buffers are NUL-terminated and outlive the call. The source and destination are
    // siblings, so no copy-across-volumes flag is needed. `MOVEFILE_WRITE_THROUGH` makes successful
    // return the durability boundary instead of merely queueing the namespace update.
    let succeeded = unsafe {
        MoveFileExW(
            from.as_ptr(),
            to.as_ptr(),
            MOVEFILE_REPLACE_EXISTING | MOVEFILE_WRITE_THROUGH,
        )
    };
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
    let wide = to_wide(path)?;
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
///
/// An interior NUL is refused rather than encoded. Every Win32 call here reads up to the first NUL,
/// so a path containing one would silently address a shorter, different path — and two of these
/// calls move and delete entries. The Unix module rejects the same input for the same reason, by
/// way of `CString::new`; doing it here keeps the two boundaries honest about the same hazard
/// instead of one of them relying on the input never occurring.
///
/// # Errors
///
/// Returns [`io::ErrorKind::InvalidInput`] when the path contains an interior NUL.
fn to_wide(path: &Path) -> io::Result<Vec<u16>> {
    let encoded = path.as_os_str().encode_wide().collect::<Vec<_>>();
    if encoded.contains(&0) {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            format!("path {} contains an interior NUL", path.display()),
        ));
    }
    let mut wide = winpath::to_extended(&encoded);
    wide.push(0);
    Ok(wide)
}
