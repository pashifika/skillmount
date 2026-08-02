//! The one Unix operation the standard library does not expose: an atomic no-replace rename.
//!
//! `std::fs::rename` maps to `rename(2)`, which silently replaces an existing destination. That is
//! precisely the behavior placement must not have: between planning and applying, another process
//! may have created the destination, and overwriting it would destroy state `SkillMount` does not
//! own. No safe wrapper exists for the flag that forbids replacement, so this module does.
//!
//! This is the only Unix module allowed to contain `unsafe`, and it contains one call per
//! platform. Every path is converted to a NUL-terminated C string first, both pointers stay alive
//! across the call, and the return value becomes an `io::Error` immediately.

#![allow(unsafe_code)]

use std::ffi::CString;
use std::io;
use std::os::unix::ffi::OsStrExt;
use std::path::Path;

/// Atomically renames `from` to `to` on the same filesystem, failing if `to` already exists.
///
/// # Errors
///
/// Returns [`io::ErrorKind::AlreadyExists`] when the destination is occupied,
/// [`io::ErrorKind::InvalidInput`] when a path contains an interior NUL, and an
/// unsupported-operation error when the host filesystem cannot promise no-replace semantics. The
/// caller must fail rather than fall back to a check-then-rename sequence.
pub(super) fn rename_no_replace(from: &Path, to: &Path) -> io::Result<()> {
    rename_excl(&c_path(from)?, &c_path(to)?)
}

fn c_path(path: &Path) -> io::Result<CString> {
    CString::new(path.as_os_str().as_bytes()).map_err(|_| {
        io::Error::new(
            io::ErrorKind::InvalidInput,
            format!("path {} contains an interior NUL", path.display()),
        )
    })
}

/// macOS reaches no-replace renaming through `renameatx_np`, available since 10.12.
///
/// `RENAME_EXCL` is honoured by APFS and HFS+. A filesystem without it returns `ENOTSUP`, which
/// the caller reports rather than emulates.
#[cfg(target_os = "macos")]
fn rename_excl(from: &CString, to: &CString) -> io::Result<()> {
    // SAFETY: both pointers come from `CString` values the caller owns for the whole call, and
    // each is NUL-terminated by construction. `AT_FDCWD` resolves a relative path against the
    // current directory, the same base `std::fs::rename` uses. The call renames one directory
    // entry and has no other effect; its result is inspected immediately below.
    let result = unsafe {
        libc::renameatx_np(
            libc::AT_FDCWD,
            from.as_ptr(),
            libc::AT_FDCWD,
            to.as_ptr(),
            libc::RENAME_EXCL,
        )
    };
    os_result(result)
}

/// Linux reaches the same guarantee through `renameat2`, available since kernel 3.15.
///
/// Linux is not a supported release target. This branch exists so the shared suite runs on the CI
/// quality runner rather than skipping every placement test there, which would leave the behavior
/// unverified on the host that runs the most checks.
#[cfg(any(target_os = "linux", target_os = "android"))]
fn rename_excl(from: &CString, to: &CString) -> io::Result<()> {
    // SAFETY: identical to the macOS branch. Both pointers outlive the call, and
    // `RENAME_NOREPLACE` only narrows what the syscall is permitted to do.
    let result = unsafe {
        libc::renameat2(
            libc::AT_FDCWD,
            from.as_ptr(),
            libc::AT_FDCWD,
            to.as_ptr(),
            libc::RENAME_NOREPLACE,
        )
    };
    os_result(result)
}

/// Every other Unix fails closed.
///
/// Falling back to `rename(2)` here would replace a destination that appeared after planning,
/// which is the one outcome placement exists to prevent.
#[cfg(not(any(target_os = "macos", target_os = "linux", target_os = "android")))]
fn rename_excl(_from: &CString, _to: &CString) -> io::Result<()> {
    Err(io::Error::new(
        io::ErrorKind::Unsupported,
        "this platform exposes no atomic no-replace rename",
    ))
}

#[cfg(any(target_os = "macos", target_os = "linux", target_os = "android"))]
fn os_result(result: libc::c_int) -> io::Result<()> {
    if result == 0 {
        Ok(())
    } else {
        Err(io::Error::last_os_error())
    }
}
