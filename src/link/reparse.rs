//! Encoding and decoding of the Windows `REPARSE_DATA_BUFFER` payload.
//!
//! This is deliberately a pure byte-slice codec with no Windows types in it. Getting a reparse
//! buffer wrong points a junction at the wrong directory, and a bug that can only be reproduced on
//! a Windows CI runner is a bug that gets found late. Keeping the layout arithmetic here means
//! every bound, every offset, and every rejection is exercised on any host.
//!
//! Only the two tags `SkillMount` can own are decoded. A deduplication reparse point, an
//! `OneDrive` placeholder, and an application execution alias are all reparse points too, and
//! treating one of them as a link `SkillMount` created would be exactly the ownership confusion the
//! removal contract exists to prevent.

use std::fmt;

/// Tag identifying a mount point, which is what a junction is.
pub(crate) const IO_REPARSE_TAG_MOUNT_POINT: u32 = 0xA000_0003;
/// Tag identifying a symbolic link.
pub(crate) const IO_REPARSE_TAG_SYMLINK: u32 = 0xA000_000C;
/// Largest reparse payload Windows accepts, from `winnt.h`.
pub(crate) const MAXIMUM_REPARSE_DATA_BUFFER_SIZE: usize = 16 * 1024;

/// Bytes before `ReparseDataLength` takes effect: tag, data length, and reserved.
const HEADER_LEN: usize = 8;
/// The four `USHORT` name offset/length fields shared by both supported tags.
const NAME_FIELDS_LEN: usize = 8;
/// A symbolic link adds a `ULONG` flags field before its path buffer.
const SYMLINK_FLAGS_LEN: usize = 4;

/// A decoded reparse point.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct ReparsePoint {
    /// Reparse tag, one of the two supported constants.
    pub(crate) tag: u32,
    /// Substitute name: the target the object manager resolves, such as `\??\C:\Skills`.
    pub(crate) substitute_name: Vec<u16>,
    /// Print name: the display form, which may be empty.
    pub(crate) print_name: Vec<u16>,
}

/// Why a reparse buffer could not be decoded or encoded.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum ReparseError {
    /// The buffer is shorter than the fixed fields it must contain.
    TooShort,
    /// The tag is not one this backend owns.
    UnsupportedTag(u32),
    /// `ReparseDataLength` disagrees with how many bytes are present.
    LengthMismatch,
    /// A name offset or length reaches outside the path buffer.
    NameOutOfBounds,
    /// A name length is not a whole number of wide characters.
    OddNameLength,
    /// The encoded buffer would exceed the maximum Windows accepts.
    TooLarge,
    /// A name contains an interior NUL, which would truncate the stored target.
    InteriorNul,
}

impl fmt::Display for ReparseError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::TooShort => formatter.write_str("the reparse buffer is truncated"),
            Self::UnsupportedTag(tag) => {
                write!(formatter, "reparse tag {tag:#010x} is not a directory link")
            }
            Self::LengthMismatch => {
                formatter.write_str("the reparse buffer declares a length it does not have")
            }
            Self::NameOutOfBounds => {
                formatter.write_str("a reparse name reaches outside its path buffer")
            }
            Self::OddNameLength => {
                formatter.write_str("a reparse name length is not a whole number of characters")
            }
            Self::TooLarge => formatter.write_str("the reparse buffer exceeds the maximum size"),
            Self::InteriorNul => formatter.write_str("a reparse name contains an interior NUL"),
        }
    }
}

/// Decodes a reparse buffer read from `FSCTL_GET_REPARSE_POINT`.
pub(crate) fn parse(buffer: &[u8]) -> Result<ReparsePoint, ReparseError> {
    if buffer.len() < HEADER_LEN + NAME_FIELDS_LEN {
        return Err(ReparseError::TooShort);
    }
    let tag = u32::from_le_bytes([buffer[0], buffer[1], buffer[2], buffer[3]]);
    let path_offset = match tag {
        IO_REPARSE_TAG_MOUNT_POINT => HEADER_LEN + NAME_FIELDS_LEN,
        IO_REPARSE_TAG_SYMLINK => HEADER_LEN + NAME_FIELDS_LEN + SYMLINK_FLAGS_LEN,
        other => return Err(ReparseError::UnsupportedTag(other)),
    };

    let declared_end = HEADER_LEN + usize::from(read_u16(buffer, 4));
    if declared_end > buffer.len() || declared_end < path_offset {
        return Err(ReparseError::LengthMismatch);
    }

    let path_buffer = &buffer[path_offset..declared_end];
    Ok(ReparsePoint {
        tag,
        substitute_name: read_name(path_buffer, read_u16(buffer, 8), read_u16(buffer, 10))?,
        print_name: read_name(path_buffer, read_u16(buffer, 12), read_u16(buffer, 14))?,
    })
}

/// Encodes the mount-point buffer that creates a junction.
///
/// The substitute name is what the object manager follows and must be an `\??\`-prefixed absolute
/// path. The print name is what Explorer and `dir` show. Both are stored NUL-terminated, which
/// Windows expects even though the lengths exclude the terminators.
pub(crate) fn build_mount_point(
    substitute_name: &[u16],
    print_name: &[u16],
) -> Result<Vec<u8>, ReparseError> {
    if substitute_name.contains(&0) || print_name.contains(&0) {
        return Err(ReparseError::InteriorNul);
    }

    let substitute_bytes = wide_byte_length(substitute_name)?;
    let print_bytes = wide_byte_length(print_name)?;
    // Each name is followed by a NUL that its declared length excludes.
    let path_bytes = substitute_bytes
        .checked_add(print_bytes)
        .and_then(|total| total.checked_add(4))
        .ok_or(ReparseError::TooLarge)?;
    let data_length = NAME_FIELDS_LEN + path_bytes;
    let total = HEADER_LEN + data_length;
    if total > MAXIMUM_REPARSE_DATA_BUFFER_SIZE {
        return Err(ReparseError::TooLarge);
    }

    let mut buffer = Vec::with_capacity(total);
    buffer.extend_from_slice(&IO_REPARSE_TAG_MOUNT_POINT.to_le_bytes());
    buffer.extend_from_slice(
        &u16::try_from(data_length)
            .map_err(|_| ReparseError::TooLarge)?
            .to_le_bytes(),
    );
    buffer.extend_from_slice(&0u16.to_le_bytes());
    buffer.extend_from_slice(&0u16.to_le_bytes());
    buffer.extend_from_slice(
        &u16::try_from(substitute_bytes)
            .map_err(|_| ReparseError::TooLarge)?
            .to_le_bytes(),
    );
    buffer.extend_from_slice(
        &u16::try_from(substitute_bytes + 2)
            .map_err(|_| ReparseError::TooLarge)?
            .to_le_bytes(),
    );
    buffer.extend_from_slice(
        &u16::try_from(print_bytes)
            .map_err(|_| ReparseError::TooLarge)?
            .to_le_bytes(),
    );
    push_wide(&mut buffer, substitute_name);
    push_wide(&mut buffer, &[0]);
    push_wide(&mut buffer, print_name);
    push_wide(&mut buffer, &[0]);
    Ok(buffer)
}

fn read_u16(buffer: &[u8], offset: usize) -> u16 {
    u16::from_le_bytes([buffer[offset], buffer[offset + 1]])
}

fn read_name(path_buffer: &[u8], offset: u16, length: u16) -> Result<Vec<u16>, ReparseError> {
    if length % 2 != 0 || offset % 2 != 0 {
        return Err(ReparseError::OddNameLength);
    }
    let start = usize::from(offset);
    let end = start
        .checked_add(usize::from(length))
        .ok_or(ReparseError::NameOutOfBounds)?;
    let bytes = path_buffer
        .get(start..end)
        .ok_or(ReparseError::NameOutOfBounds)?;
    Ok(bytes
        .chunks_exact(2)
        .map(|pair| u16::from_le_bytes([pair[0], pair[1]]))
        .collect())
}

fn wide_byte_length(name: &[u16]) -> Result<usize, ReparseError> {
    name.len().checked_mul(2).ok_or(ReparseError::TooLarge)
}

fn push_wide(buffer: &mut Vec<u8>, name: &[u16]) {
    for unit in name {
        buffer.extend_from_slice(&unit.to_le_bytes());
    }
}

#[cfg(test)]
mod tests {
    use super::{
        IO_REPARSE_TAG_MOUNT_POINT, IO_REPARSE_TAG_SYMLINK, MAXIMUM_REPARSE_DATA_BUFFER_SIZE,
        ReparseError, build_mount_point, parse,
    };

    fn wide(value: &str) -> Vec<u16> {
        value.encode_utf16().collect()
    }

    fn text(value: &[u16]) -> String {
        String::from_utf16(value).expect("test names stay valid UTF-16")
    }

    #[test]
    fn a_built_junction_buffer_decodes_to_the_names_it_was_given() {
        let substitute = wide("\\??\\C:\\Skills\\スキル 集");
        let print = wide("C:\\Skills\\スキル 集");
        let buffer = build_mount_point(&substitute, &print).expect("the buffer fits");

        let decoded = parse(&buffer).expect("a buffer this module built must decode");

        assert_eq!(decoded.tag, IO_REPARSE_TAG_MOUNT_POINT);
        assert_eq!(
            text(&decoded.substitute_name),
            "\\??\\C:\\Skills\\スキル 集"
        );
        assert_eq!(text(&decoded.print_name), "C:\\Skills\\スキル 集");
    }

    #[test]
    fn an_empty_print_name_is_valid() {
        let buffer = build_mount_point(&wide("\\??\\C:\\Skills"), &[]).expect("the buffer fits");
        let decoded = parse(&buffer).expect("decodes");
        assert_eq!(text(&decoded.substitute_name), "\\??\\C:\\Skills");
        assert!(decoded.print_name.is_empty());
    }

    /// Builds the symbolic-link layout, which puts a `ULONG` flags field before its path buffer.
    fn symlink_buffer(substitute: &str, print: &str) -> Vec<u8> {
        let substitute = wide(substitute);
        let print = wide(print);
        let substitute_bytes = u16::try_from(substitute.len() * 2).unwrap();
        let print_bytes = u16::try_from(print.len() * 2).unwrap();
        let data_length = 8 + 4 + u32::from(substitute_bytes) + u32::from(print_bytes) + 4;

        let mut buffer = Vec::new();
        buffer.extend_from_slice(&IO_REPARSE_TAG_SYMLINK.to_le_bytes());
        buffer.extend_from_slice(&u16::try_from(data_length).unwrap().to_le_bytes());
        buffer.extend_from_slice(&0u16.to_le_bytes());
        buffer.extend_from_slice(&0u16.to_le_bytes());
        buffer.extend_from_slice(&substitute_bytes.to_le_bytes());
        buffer.extend_from_slice(&(substitute_bytes + 2).to_le_bytes());
        buffer.extend_from_slice(&print_bytes.to_le_bytes());
        buffer.extend_from_slice(&1u32.to_le_bytes());
        for unit in substitute.iter().chain(&[0]).chain(&print).chain(&[0]) {
            buffer.extend_from_slice(&unit.to_le_bytes());
        }
        buffer
    }

    #[test]
    fn a_symbolic_link_buffer_decodes_past_its_flags_field() {
        let buffer = symlink_buffer("\\??\\C:\\Skills", "C:\\Skills");
        let decoded = parse(&buffer).expect("decodes");

        assert_eq!(decoded.tag, IO_REPARSE_TAG_SYMLINK);
        assert_eq!(
            text(&decoded.substitute_name),
            "\\??\\C:\\Skills",
            "reading a symbolic link at the mount-point offset would return shifted text"
        );
        assert_eq!(text(&decoded.print_name), "C:\\Skills");
    }

    #[test]
    fn a_reparse_tag_this_backend_does_not_own_is_rejected() {
        let mut buffer = build_mount_point(&wide("\\??\\C:\\Skills"), &[]).expect("fits");
        // 0xA000_0019 is IO_REPARSE_TAG_APPEXECLINK, which a store application leaves behind.
        buffer[0..4].copy_from_slice(&0xA000_0019_u32.to_le_bytes());

        assert_eq!(
            parse(&buffer),
            Err(ReparseError::UnsupportedTag(0xA000_0019))
        );
    }

    #[test]
    fn truncated_and_over_declared_buffers_are_rejected_rather_than_read() {
        assert_eq!(parse(&[]), Err(ReparseError::TooShort));
        assert_eq!(parse(&[0; 15]), Err(ReparseError::TooShort));

        let mut buffer = build_mount_point(&wide("\\??\\C:\\Skills"), &[]).expect("fits");
        let declared = u16::from_le_bytes([buffer[4], buffer[5]]);
        buffer[4..6].copy_from_slice(&(declared + 64).to_le_bytes());
        assert_eq!(parse(&buffer), Err(ReparseError::LengthMismatch));
    }

    #[test]
    fn a_name_that_reaches_outside_the_path_buffer_is_rejected() {
        let mut buffer = build_mount_point(&wide("\\??\\C:\\Skills"), &[]).expect("fits");
        let overrun = u16::try_from(buffer.len() + 32).unwrap();
        buffer[10..12].copy_from_slice(&overrun.to_le_bytes());

        assert_eq!(parse(&buffer), Err(ReparseError::NameOutOfBounds));
    }

    #[test]
    fn an_odd_name_length_is_rejected_rather_than_rounded() {
        let mut buffer = build_mount_point(&wide("\\??\\C:\\Skills"), &[]).expect("fits");
        let declared = u16::from_le_bytes([buffer[10], buffer[11]]);
        buffer[10..12].copy_from_slice(&(declared - 1).to_le_bytes());

        assert_eq!(parse(&buffer), Err(ReparseError::OddNameLength));
    }

    #[test]
    fn names_that_would_overflow_or_truncate_the_buffer_are_refused() {
        let too_long = vec![u16::from(b'a'); MAXIMUM_REPARSE_DATA_BUFFER_SIZE];
        assert_eq!(
            build_mount_point(&too_long, &[]),
            Err(ReparseError::TooLarge)
        );
        assert_eq!(
            build_mount_point(&wide("\\??\\C:\\Sk\u{0}ills"), &[]),
            Err(ReparseError::InteriorNul)
        );
    }
}
