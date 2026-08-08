//! A bounded, line-oriented codec that round-trips platform-native values exactly.
//!
//! No serialization crate is used here, and the reason is the crate's central invariant: paths and
//! forwarded arguments stay `OsString`/`PathBuf` end to end and are never forced through UTF-8. A
//! Unix path is an arbitrary byte string and a Windows path is an arbitrary UTF-16 sequence, so a
//! text format that assumes UTF-8 either rejects a legal path or silently rewrites it. Either
//! outcome would make a journal describe an entry other than the one on disk, which is precisely
//! the mistake ownership verification exists to prevent.
//!
//! Every value is therefore encoded as its platform-native bytes: verbatim when the bytes are
//! unambiguous ASCII, and `%`-prefixed hexadecimal otherwise. Because `%` is outside the verbatim
//! set, the two forms can never be confused. The header records which platform produced the bytes,
//! so a journal copied to the other platform is rejected instead of being misread.

use std::ffi::{OsStr, OsString};
use std::path::{Path, PathBuf};

use crate::native::{os_bytes as native_bytes, os_string as native_string};

/// Largest journal this codec will read.
///
/// A journal holds one line per Skill, so this is several hundred thousand Skills. The cap exists
/// because a journal is read during recovery, when the file may be truncated, corrupt, or not a
/// journal at all, and an unbounded read would turn that into an allocation failure.
pub(crate) const MAX_JOURNAL_BYTES: u64 = 4 * 1024 * 1024;

/// Largest number of body lines this codec will read.
pub(crate) const MAX_JOURNAL_LINES: usize = 65_536;

/// Platform tag written into the header.
pub(crate) const PLATFORM: &str = if cfg!(windows) { "windows" } else { "unix" };

/// Magic value that starts every journal.
pub(crate) const MAGIC: &str = "skillmount-journal";

/// Returns whether a byte may appear verbatim in an encoded value.
///
/// The set excludes whitespace and `=` so a line stays splittable, and `%` so the escape marker is
/// unambiguous. Everything else is either escaped or would be ambiguous to a reader.
const fn is_verbatim(byte: u8) -> bool {
    matches!(byte,
        b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'.' | b'_' | b'-' | b'/' | b'\\' | b':' | b'@' | b'+' | b',')
}

/// Encodes arbitrary bytes into one whitespace-free token.
pub(crate) fn encode_bytes(bytes: &[u8]) -> String {
    if !bytes.is_empty() && bytes.iter().copied().all(is_verbatim) {
        return String::from_utf8_lossy(bytes).into_owned();
    }
    let mut encoded = String::with_capacity(1 + bytes.len() * 2);
    encoded.push('%');
    for byte in bytes {
        encoded.push(nibble(byte >> 4));
        encoded.push(nibble(byte & 0x0f));
    }
    encoded
}

/// Decodes one token produced by [`encode_bytes`].
pub(crate) fn decode_bytes(token: &str) -> Option<Vec<u8>> {
    let Some(hex) = token.strip_prefix('%') else {
        return token
            .bytes()
            .all(is_verbatim)
            .then(|| token.as_bytes().to_vec());
    };
    if hex.len() % 2 != 0 {
        return None;
    }
    let mut bytes = Vec::with_capacity(hex.len() / 2);
    let raw = hex.as_bytes();
    for pair in raw.chunks_exact(2) {
        bytes.push((value(pair[0])? << 4) | value(pair[1])?);
    }
    Some(bytes)
}

/// Encodes a platform-native string.
pub(crate) fn encode_os(value: &OsStr) -> String {
    encode_bytes(&native_bytes(value))
}

/// Decodes a platform-native string.
pub(crate) fn decode_os(token: &str) -> Option<OsString> {
    decode_bytes(token).and_then(|bytes| native_string(&bytes))
}

/// Encodes a path.
pub(crate) fn encode_path(path: &Path) -> String {
    encode_os(path.as_os_str())
}

/// Decodes a path.
pub(crate) fn decode_path(token: &str) -> Option<PathBuf> {
    decode_os(token).map(PathBuf::from)
}

/// Encodes text that is already UTF-8, such as a diagnostic message.
pub(crate) fn encode_text(text: &str) -> String {
    encode_bytes(text.as_bytes())
}

/// Decodes text written by [`encode_text`].
pub(crate) fn decode_text(token: &str) -> Option<String> {
    decode_bytes(token).and_then(|bytes| String::from_utf8(bytes).ok())
}

/// One parsed body line: a record name and its fields in written order.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Line {
    /// Record name, such as `action`.
    pub(crate) record: String,
    /// Fields exactly as written, keys unescaped and values still encoded.
    fields: Vec<(String, String)>,
}

impl Line {
    /// Starts a line for `record`.
    pub(crate) fn new(record: &str) -> Self {
        Self {
            record: record.to_owned(),
            fields: Vec::new(),
        }
    }

    /// Appends a field whose value is already an encoded token.
    pub(crate) fn push(&mut self, key: &str, token: String) -> &mut Self {
        self.fields.push((key.to_owned(), token));
        self
    }

    /// Appends a field only when the value is present.
    ///
    /// An absent optional field is omitted rather than written empty, so "not recorded" and
    /// "recorded as empty" stay distinguishable. Ownership verification depends on that
    /// distinction: an action with no recorded identity is never removed.
    pub(crate) fn push_optional(&mut self, key: &str, token: Option<String>) -> &mut Self {
        if let Some(token) = token {
            self.push(key, token);
        }
        self
    }

    /// Returns the encoded token for `key`.
    pub(crate) fn field(&self, key: &str) -> Option<&str> {
        self.fields
            .iter()
            .find(|(name, _)| name == key)
            .map(|(_, value)| value.as_str())
    }

    #[cfg(test)]
    pub(crate) fn remove_field(&mut self, key: &str) {
        self.fields.retain(|(name, _)| name != key);
    }

    #[cfg(test)]
    pub(crate) fn set_field(&mut self, key: &str, token: String) {
        if let Some((_, value)) = self.fields.iter_mut().find(|(name, _)| name == key) {
            *value = token;
        } else {
            self.push(key, token);
        }
    }

    /// Renders the line without its trailing newline.
    pub(crate) fn render(&self) -> String {
        let mut rendered = self.record.clone();
        for (key, value) in &self.fields {
            rendered.push(' ');
            rendered.push_str(key);
            rendered.push('=');
            rendered.push_str(value);
        }
        rendered
    }

    /// Parses one body line, rejecting a field that has no `=`.
    fn parse(raw: &str) -> Option<Self> {
        let mut parts = raw.split(' ');
        let record = parts.next()?;
        if record.is_empty() {
            return None;
        }
        let mut line = Self::new(record);
        for part in parts {
            let (key, value) = part.split_once('=')?;
            if key.is_empty() {
                return None;
            }
            line.push(key, value.to_owned());
        }
        Some(line)
    }
}

/// Why a byte sequence is not a journal this build can act on.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum DecodeError {
    /// The header is missing, malformed, or not a journal at all.
    Malformed(String),
    /// The body checksum does not match, which a truncated write also produces.
    ChecksumMismatch,
    /// The schema version is not the one this build writes.
    UnsupportedVersion(String),
    /// The journal was written by the other platform's encoding.
    ForeignPlatform(String),
}

impl std::fmt::Display for DecodeError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Malformed(detail) => write!(formatter, "malformed journal: {detail}"),
            Self::ChecksumMismatch => formatter.write_str(
                "the body checksum does not match, so the journal is truncated or corrupt",
            ),
            Self::UnsupportedVersion(found) => write!(
                formatter,
                "schema version {found} is not supported by this build (expected {})",
                super::SCHEMA_VERSION
            ),
            Self::ForeignPlatform(found) => write!(
                formatter,
                "the journal was written on {found}, whose native path encoding this host cannot interpret"
            ),
        }
    }
}

/// Renders a header and body into the exact bytes a current journal file holds.
pub(crate) fn render_document(lines: &[Line]) -> Vec<u8> {
    render_document_with_schema(lines, super::SCHEMA_VERSION)
}

#[cfg(test)]
pub(crate) fn render_document_for_schema(lines: &[Line], schema_version: u32) -> Vec<u8> {
    render_document_with_schema(lines, schema_version)
}

fn render_document_with_schema(lines: &[Line], schema_version: u32) -> Vec<u8> {
    let mut body = String::new();
    for line in lines {
        body.push_str(&line.render());
        body.push('\n');
    }
    let header = format!(
        "{MAGIC} {schema_version} {PLATFORM} {}\n",
        checksum(body.as_bytes())
    );
    let mut document = header.into_bytes();
    document.extend_from_slice(body.as_bytes());
    document
}

#[derive(Debug)]
/// A checksum-verified document and the schema needed to interpret its body.
pub(crate) struct ParsedDocument {
    pub(crate) schema_version: u32,
    pub(crate) lines: Vec<Line>,
}

/// Validates the header and checksum and returns the schema plus parsed body lines.
pub(crate) fn parse_document(document: &[u8]) -> Result<ParsedDocument, DecodeError> {
    let text = std::str::from_utf8(document)
        .map_err(|_| DecodeError::Malformed("the file is not valid UTF-8".to_owned()))?;
    let (header, body) = text
        .split_once('\n')
        .ok_or_else(|| DecodeError::Malformed("the header line is unterminated".to_owned()))?;

    let mut parts = header.split(' ');
    if parts.next() != Some(MAGIC) {
        return Err(DecodeError::Malformed(
            "the file does not start with a SkillMount journal header".to_owned(),
        ));
    }
    let version = parts
        .next()
        .ok_or_else(|| DecodeError::Malformed("the header has no schema version".to_owned()))?;
    let platform = parts
        .next()
        .ok_or_else(|| DecodeError::Malformed("the header has no platform tag".to_owned()))?;
    let recorded = parts
        .next()
        .ok_or_else(|| DecodeError::Malformed("the header has no checksum".to_owned()))?;
    if parts.next().is_some() {
        return Err(DecodeError::Malformed(
            "the header has unexpected trailing fields".to_owned(),
        ));
    }

    // The version is checked before the checksum so a future journal reports the reason an
    // operator can act on rather than a checksum whose algorithm may itself have changed.
    let schema_version = if version == super::SCHEMA_VERSION.to_string() {
        super::SCHEMA_VERSION
    } else if version == super::LEGACY_SCHEMA_VERSION.to_string() {
        super::LEGACY_SCHEMA_VERSION
    } else {
        return Err(DecodeError::UnsupportedVersion(version.to_owned()));
    };
    if platform != PLATFORM {
        return Err(DecodeError::ForeignPlatform(platform.to_owned()));
    }
    if recorded != checksum(body.as_bytes()) {
        return Err(DecodeError::ChecksumMismatch);
    }

    let mut lines = Vec::new();
    for raw in body.lines() {
        if raw.is_empty() {
            continue;
        }
        if lines.len() == MAX_JOURNAL_LINES {
            return Err(DecodeError::Malformed(format!(
                "the journal has more than {MAX_JOURNAL_LINES} records"
            )));
        }
        lines.push(Line::parse(raw).ok_or_else(|| {
            DecodeError::Malformed("a record line is not `name key=value ...`".to_owned())
        })?);
    }
    Ok(ParsedDocument {
        schema_version,
        lines,
    })
}

/// Returns the lowercase hexadecimal SHA-256 digest of `bytes`.
fn checksum(bytes: &[u8]) -> String {
    use sha2::{Digest, Sha256};

    let digest = Sha256::digest(bytes);
    let mut rendered = String::with_capacity(digest.len() * 2);
    for byte in digest {
        rendered.push(nibble(byte >> 4));
        rendered.push(nibble(byte & 0x0f));
    }
    rendered
}

const fn nibble(value: u8) -> char {
    (if value < 10 {
        b'0' + value
    } else {
        b'a' + (value - 10)
    }) as char
}

const fn value(byte: u8) -> Option<u8> {
    match byte {
        b'0'..=b'9' => Some(byte - b'0'),
        b'a'..=b'f' => Some(byte - b'a' + 10),
        _ => None,
    }
}
