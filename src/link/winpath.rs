//! Windows path-namespace normalization, for comparison only.
//!
//! Windows names one directory several ways. `C:\Skills`, `c:/Skills/`, `\\?\C:\Skills`, and the
//! `\??\C:\Skills` form stored inside a junction's reparse buffer are the same place, and code
//! that compares them as strings gets the wrong answer. This module folds those spellings into one
//! key. It never rewrites a path an operator sees.
//!
//! The functions take and return wide units rather than [`std::path::Path`] on purpose. Windows
//! path semantics are not the host's path semantics, so a `Path`-based implementation would behave
//! differently when it is compiled for a Unix test host and could not be tested where it is being
//! written. Working on `&[u16]` makes the algorithm exact and testable everywhere.
//!
//! Case is folded for the drive letter only. NTFS has supported per-directory case sensitivity
//! since Windows 10 1803, so folding whole paths would make two entries that really are different
//! compare equal — which for a module that decides whether an entry is still ours would be a
//! correctness bug, not a convenience.

/// Path separator.
const SEP: u16 = b'\\' as u16;
/// Alternate separator that Windows accepts and this module folds away.
const ALT_SEP: u16 = b'/' as u16;
/// Drive-letter delimiter.
const COLON: u16 = b':' as u16;
/// Current-directory component.
const DOT: u16 = b'.' as u16;

/// The `\\?\` verbatim prefix.
const VERBATIM_PREFIX: [u16; 4] = [SEP, SEP, b'?' as u16, SEP];
/// The `\??\` NT object-namespace prefix stored inside reparse buffers.
const NT_PREFIX: [u16; 4] = [SEP, b'?' as u16, b'?' as u16, SEP];
/// The `UNC\` marker that follows a namespace prefix on a network path.
const UNC_MARKER: [u16; 4] = [b'U' as u16, b'N' as u16, b'C' as u16, SEP];

/// Returns the normalized comparison key for a Windows path.
///
/// Namespace prefixes are removed, separators are folded to `\`, `.` and `..` components are
/// resolved lexically, a trailing separator is dropped, and an ASCII drive letter is upper-cased.
pub(crate) fn comparison_key(path: &[u16]) -> Vec<u16> {
    let namespace_free = strip_namespace(&fold_separators(path));
    let (mut key, rest) = split_root(&namespace_free);
    // A prefix that ends in a separator is a real root, so `..` cannot climb above it and the
    // first component needs no separator of its own. A relative or drive-relative path keeps a
    // leading `..`, because dropping it would silently name a different directory.
    let rooted = key.ends_with(&[SEP]);

    let mut components: Vec<&[u16]> = Vec::new();
    for component in rest.split(|unit| *unit == SEP) {
        match component {
            [] | [DOT] => {}
            [DOT, DOT] => {
                if components.last().is_some_and(|last| !is_parent(last)) {
                    components.pop();
                } else if !rooted {
                    // There is nothing above a root, so `C:\..` stays `C:\`. A relative path has
                    // to keep the hop: dropping it would name a different directory.
                    components.push(component);
                }
            }
            other => components.push(other),
        }
    }

    for (index, component) in components.iter().enumerate() {
        if index > 0 {
            key.push(SEP);
        }
        key.extend_from_slice(component);
    }
    key
}

/// Returns whether the path names a UNC or network share.
///
/// Junction fallback refuses these: a junction stores a local NT device path and does not resolve
/// to a share, so one created against `\\server\share` would point at nothing usable.
pub(crate) fn is_unc(path: &[u16]) -> bool {
    let folded = strip_namespace(&fold_separators(path));
    folded.starts_with(&[SEP, SEP])
}

/// Returns whether the path is an absolute local drive path such as `C:\Skills`.
pub(crate) fn is_local_drive_absolute(path: &[u16]) -> bool {
    let folded = strip_namespace(&fold_separators(path));
    matches!(folded.as_slice(), [drive, COLON, SEP, ..] if is_ascii_alpha(*drive))
}

/// Renders an absolute local path as the `\??\C:\...` substitute name a junction stores.
pub(crate) fn to_nt_substitute_name(path: &[u16]) -> Vec<u16> {
    let mut name = NT_PREFIX.to_vec();
    name.extend_from_slice(&comparison_key(path));
    name
}

/// Renders an absolute path in the `\\?\` extended form the file APIs are called with.
///
/// The extended form lifts the 260-character limit, which a Skill store nested under a deep
/// project path reaches in practice. A relative path has no extended form and is returned
/// normalized, which leaves it to be resolved against the current directory as usual.
pub(crate) fn to_extended(path: &[u16]) -> Vec<u16> {
    let key = comparison_key(path);
    if key.starts_with(&[SEP, SEP]) {
        let mut extended = VERBATIM_PREFIX.to_vec();
        extended.extend_from_slice(&UNC_MARKER);
        extended.extend_from_slice(&key[2..]);
        return extended;
    }
    if is_local_drive_absolute(&key) {
        let mut extended = VERBATIM_PREFIX.to_vec();
        extended.extend_from_slice(&key);
        return extended;
    }
    key
}

/// Replaces every `/` with `\`.
fn fold_separators(path: &[u16]) -> Vec<u16> {
    path.iter()
        .map(|unit| if *unit == ALT_SEP { SEP } else { *unit })
        .collect()
}

/// Removes a `\\?\`, `\??\`, `\\?\UNC\`, or `\??\UNC\` prefix.
///
/// The UNC forms collapse back to the `\\server\share` spelling rather than being dropped, so a
/// verbatim network path and the same path typed normally produce one key.
fn strip_namespace(path: &[u16]) -> Vec<u16> {
    let rest = if path.starts_with(&VERBATIM_PREFIX) || path.starts_with(&NT_PREFIX) {
        &path[VERBATIM_PREFIX.len()..]
    } else {
        return path.to_vec();
    };

    if starts_with_ascii_case_insensitive(rest, &UNC_MARKER) {
        let mut unc = vec![SEP, SEP];
        unc.extend_from_slice(&rest[UNC_MARKER.len()..]);
        return unc;
    }
    rest.to_vec()
}

/// Splits a namespace-free path into its root prefix and the components below it.
///
/// The prefix is returned already normalized, which is where the drive letter is upper-cased. An
/// empty prefix means the path is relative and `..` at its head has to be kept.
fn split_root(path: &[u16]) -> (Vec<u16>, &[u16]) {
    match path {
        [drive, COLON, SEP, rest @ ..] if is_ascii_alpha(*drive) => {
            (vec![to_ascii_upper(*drive), COLON, SEP], rest)
        }
        // `C:skills` is drive-relative. It is ambiguous and rejected before it reaches this
        // module, but it must still normalize to something stable rather than losing its drive.
        [drive, COLON, rest @ ..] if is_ascii_alpha(*drive) => {
            (vec![to_ascii_upper(*drive), COLON], rest)
        }
        [SEP, SEP, rest @ ..] => (vec![SEP, SEP], rest),
        [SEP, rest @ ..] => (vec![SEP], rest),
        rest => (Vec::new(), rest),
    }
}

fn starts_with_ascii_case_insensitive(path: &[u16], prefix: &[u16]) -> bool {
    path.len() >= prefix.len()
        && path
            .iter()
            .zip(prefix)
            .all(|(left, right)| to_ascii_upper(*left) == to_ascii_upper(*right))
}

fn is_parent(component: &[u16]) -> bool {
    component == &[DOT, DOT][..]
}

const fn is_ascii_alpha(unit: u16) -> bool {
    matches!(unit, 0x41..=0x5A | 0x61..=0x7A)
}

const fn to_ascii_upper(unit: u16) -> u16 {
    if matches!(unit, 0x61..=0x7A) {
        unit - 0x20
    } else {
        unit
    }
}

#[cfg(test)]
mod tests {
    use super::{
        comparison_key, is_local_drive_absolute, is_unc, to_extended, to_nt_substitute_name,
    };

    fn wide(value: &str) -> Vec<u16> {
        value.encode_utf16().collect()
    }

    fn key(value: &str) -> String {
        String::from_utf16(&comparison_key(&wide(value))).expect("keys stay valid UTF-16")
    }

    #[test]
    fn every_namespace_spelling_of_one_directory_shares_a_key() {
        let expected = "C:\\Skills\\rust";
        for spelling in [
            "C:\\Skills\\rust",
            "c:\\Skills\\rust",
            "C:/Skills/rust",
            "C:\\Skills\\rust\\",
            "C:\\Skills\\\\rust",
            "C:\\Skills\\.\\rust",
            "C:\\Skills\\other\\..\\rust",
            "\\\\?\\C:\\Skills\\rust",
            "\\??\\C:\\Skills\\rust",
        ] {
            assert_eq!(key(spelling), expected, "spelling {spelling}");
        }
    }

    #[test]
    fn only_the_drive_letter_is_case_folded() {
        assert_eq!(key("c:\\Skills"), "C:\\Skills");
        assert_ne!(
            key("C:\\SKILLS"),
            key("C:\\skills"),
            "a case-sensitive NTFS directory really can hold both"
        );
    }

    #[test]
    fn a_root_keeps_its_separator_and_a_leaf_loses_its_trailing_one() {
        assert_eq!(key("C:\\"), "C:\\");
        assert_eq!(key("C:\\Skills\\"), "C:\\Skills");
        assert_eq!(key("\\\\?\\C:\\"), "C:\\");
    }

    #[test]
    fn verbatim_and_plain_unc_paths_share_a_key() {
        assert_eq!(
            key("\\\\?\\UNC\\server\\share\\skills"),
            "\\\\server\\share\\skills"
        );
        assert_eq!(
            key("\\\\server\\share\\skills"),
            "\\\\server\\share\\skills"
        );
        assert_eq!(key("\\??\\unc\\server\\share"), "\\\\server\\share");
    }

    #[test]
    fn parent_components_never_escape_a_root_but_survive_a_relative_path() {
        assert_eq!(key("C:\\..\\..\\Skills"), "C:\\Skills");
        assert_eq!(key("..\\..\\skills"), "..\\..\\skills");
        assert_eq!(key("skills\\..\\..\\other"), "..\\other");
    }

    #[test]
    fn junction_eligibility_separates_local_drives_from_network_paths() {
        assert!(is_local_drive_absolute(&wide("C:\\Skills")));
        assert!(is_local_drive_absolute(&wide("\\\\?\\c:\\Skills")));
        assert!(!is_local_drive_absolute(&wide("\\\\server\\share")));
        assert!(!is_local_drive_absolute(&wide("skills")));

        assert!(is_unc(&wide("\\\\server\\share")));
        assert!(is_unc(&wide("\\\\?\\UNC\\server\\share")));
        assert!(!is_unc(&wide("C:\\Skills")));
    }

    #[test]
    fn a_substitute_name_carries_the_nt_prefix_a_junction_stores() {
        assert_eq!(
            String::from_utf16(&to_nt_substitute_name(&wide("c:/Skills/rust/"))).unwrap(),
            "\\??\\C:\\Skills\\rust"
        );
    }

    #[test]
    fn an_extended_form_is_produced_for_absolute_paths_only() {
        let extended = |value: &str| {
            String::from_utf16(&to_extended(&wide(value))).expect("stays valid UTF-16")
        };

        assert_eq!(extended("c:/Skills/rust"), "\\\\?\\C:\\Skills\\rust");
        assert_eq!(extended("\\\\?\\C:\\Skills"), "\\\\?\\C:\\Skills");
        assert_eq!(
            extended("\\\\server\\share\\skills"),
            "\\\\?\\UNC\\server\\share\\skills"
        );
        assert_eq!(
            extended("skills\\rust"),
            "skills\\rust",
            "a relative path has no extended form and must not gain a bogus root"
        );
    }

    #[test]
    fn japanese_spaces_and_long_paths_survive_normalization() {
        assert_eq!(
            key("\\\\?\\C:\\Program Files\\スキル 集\\rust"),
            "C:\\Program Files\\スキル 集\\rust"
        );
        let deep = format!("C:\\{}", ["長い名前"; 40].join("\\"));
        assert_eq!(key(&deep), deep);
    }
}
