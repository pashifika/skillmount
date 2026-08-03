//! Audited Windows Known Folder boundary for Codex discovery roots.

#![allow(unsafe_code)]

use std::ffi::{OsString, c_void};
use std::os::windows::ffi::OsStringExt as _;
use std::path::PathBuf;
use std::ptr;
use std::slice;

use windows_sys::Win32::Globalization::lstrlenW;
use windows_sys::Win32::System::Com::CoTaskMemFree;
use windows_sys::Win32::UI::Shell::{FOLDERID_Profile, FOLDERID_ProgramData, SHGetKnownFolderPath};
use windows_sys::core::{GUID, PWSTR};

/// Resolves the profile directory through the same Known Folder API as Codex 0.146.0.
pub(super) fn profile_directory() -> Option<PathBuf> {
    known_folder(FOLDERID_Profile)
}

/// Resolves the administrator configuration base used by Codex 0.146.0.
pub(super) fn program_data_directory() -> Option<PathBuf> {
    known_folder(FOLDERID_ProgramData)
}

fn known_folder(folder_id: GUID) -> Option<PathBuf> {
    let mut path_ptr: PWSTR = ptr::null_mut();
    // SAFETY: `folder_id` is one of the two static Known Folder GUID values above, the token is
    // null for the current user, and `path_ptr` is a valid out pointer. On success Windows returns
    // a null-terminated allocation owned by the COM task allocator. `lstrlenW` reads only through
    // that terminator, the slice is copied into an OsString before `CoTaskMemFree`, and the pointer
    // is freed exactly once on both success and failure. No raw value escapes this module.
    unsafe {
        let result =
            SHGetKnownFolderPath(&raw const folder_id, 0, ptr::null_mut(), &raw mut path_ptr);
        if result != 0 || path_ptr.is_null() {
            CoTaskMemFree(path_ptr.cast::<c_void>());
            return None;
        }

        let raw_length = lstrlenW(path_ptr);
        let Ok(length) = usize::try_from(raw_length) else {
            CoTaskMemFree(path_ptr.cast::<c_void>());
            return None;
        };
        let path = OsString::from_wide(slice::from_raw_parts(path_ptr, length));
        CoTaskMemFree(path_ptr.cast::<c_void>());
        Some(PathBuf::from(path))
    }
}
