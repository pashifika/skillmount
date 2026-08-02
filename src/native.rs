//! Lossless conversion between platform-native strings and bytes.
//!
//! Two places need a path as bytes: the journal codec, which must write a path and read back
//! exactly the same one, and lock-key derivation, which must hash two spellings of one resource to
//! the same digest. Both would be wrong if they went through UTF-8. A Unix path is an arbitrary
//! byte string, a Windows path is an arbitrary UTF-16 sequence including unpaired surrogates, and
//! lossy conversion turns two distinct paths into one value — which for a lock key means two
//! different resources sharing a lock, and for a journal means recorded ownership evidence that
//! points at the wrong entry.
//!
//! The byte encoding is platform-specific by design and is never exchanged between platforms.

use std::ffi::{OsStr, OsString};

/// Returns the platform-native byte encoding of `value`.
#[cfg(unix)]
pub(crate) fn os_bytes(value: &OsStr) -> Vec<u8> {
    use std::os::unix::ffi::OsStrExt;

    value.as_bytes().to_vec()
}

/// Rebuilds a platform-native string from [`os_bytes`].
///
/// The `Option` matches the Windows signature, where an odd byte count cannot be a UTF-16 sequence.
/// Every byte string is a legal Unix path, so this side never fails.
#[cfg(unix)]
#[allow(clippy::unnecessary_wraps)]
pub(crate) fn os_string(bytes: &[u8]) -> Option<OsString> {
    use std::os::unix::ffi::OsStringExt;

    Some(OsString::from_vec(bytes.to_vec()))
}

/// Returns the platform-native byte encoding of `value`.
///
/// UTF-16 code units are written little-endian, which round-trips an unpaired surrogate that no
/// UTF-8 encoding can represent.
#[cfg(windows)]
pub(crate) fn os_bytes(value: &OsStr) -> Vec<u8> {
    use std::os::windows::ffi::OsStrExt;

    let mut bytes = Vec::new();
    for unit in value.encode_wide() {
        bytes.extend_from_slice(&unit.to_le_bytes());
    }
    bytes
}

/// Rebuilds a platform-native string from [`os_bytes`].
#[cfg(windows)]
pub(crate) fn os_string(bytes: &[u8]) -> Option<OsString> {
    use std::os::windows::ffi::OsStringExt;

    if bytes.len() % 2 != 0 {
        return None;
    }
    let units = bytes
        .chunks_exact(2)
        .map(|pair| u16::from_le_bytes([pair[0], pair[1]]))
        .collect::<Vec<_>>();
    Some(OsString::from_wide(&units))
}
