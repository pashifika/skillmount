use std::fs::File;
#[cfg(unix)]
use std::fs::OpenOptions;
use std::io::Read;
use std::path::Path;

use serde_yaml_ng::Value;

use crate::domain::SkillMetadata;

const MAX_SKILL_MD_BYTES: u64 = 1024 * 1024;
const MAX_FRONTMATTER_BYTES: usize = 64 * 1024;
const CODEX_MAX_NAME_CHARS: usize = 64;

#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) struct ParsedFrontmatter {
    pub(super) metadata: SkillMetadata,
    pub(super) raw: String,
}

/// Result category for Codex-compatible metadata inspection.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum CodexMetadataError {
    /// Codex rejects this Skill too, so it can be retained as a warning and omitted.
    Rejected(String),
    /// `SkillMount` could not prove that its inventory is complete and must fail closed.
    Incomplete(String),
}

pub(super) fn parse(path: &Path) -> Result<ParsedFrontmatter, String> {
    let mut file = open_regular_skill_md(path)?;
    let mut bytes = Vec::new();
    file.by_ref()
        .take(MAX_SKILL_MD_BYTES + 1)
        .read_to_end(&mut bytes)
        .map_err(|error| format!("cannot read SKILL.md: {error}"))?;
    if u64::try_from(bytes.len()).unwrap_or(u64::MAX) > MAX_SKILL_MD_BYTES {
        return Err(format!("SKILL.md exceeds {MAX_SKILL_MD_BYTES} bytes"));
    }
    let content = String::from_utf8(bytes)
        .map_err(|_| "SKILL.md frontmatter is not valid UTF-8".to_owned())?;
    let raw = envelope(&content)?;
    let value: Value = serde_yaml_ng::from_str(raw)
        .map_err(|error| format!("invalid YAML frontmatter: {error}"))?;
    let mapping = value
        .as_mapping()
        .ok_or_else(|| "YAML frontmatter must be a mapping".to_owned())?;
    let metadata = SkillMetadata {
        name: known_string(mapping, "name")?,
        description: known_string(mapping, "description")?,
    };
    Ok(ParsedFrontmatter {
        metadata,
        raw: raw.to_owned(),
    })
}

/// Confirms that metadata can be opened without parsing it or blocking on a special file.
pub(crate) fn readable(path: &Path) -> Result<(), String> {
    open_regular_skill_md(path).map(drop)
}

fn open_regular_skill_md(path: &Path) -> Result<File, String> {
    open_regular_file(path, "SKILL.md")
}

/// Reads at most `max_bytes` from a regular file without blocking on a Unix FIFO or device.
///
/// The regular-file check happens after opening so a path swapped between an earlier metadata
/// probe and this read cannot bypass the boundary.
pub(crate) fn read_bounded_regular_file(
    path: &Path,
    label: &str,
    max_bytes: u64,
) -> Result<Vec<u8>, String> {
    let mut file = open_regular_file(path, label)?;
    let mut bytes = Vec::new();
    file.by_ref()
        .take(max_bytes.saturating_add(1))
        .read_to_end(&mut bytes)
        .map_err(|error| format!("cannot read {label}: {error}"))?;
    if u64::try_from(bytes.len()).unwrap_or(u64::MAX) > max_bytes {
        return Err(format!("{label} exceeds {max_bytes} bytes"));
    }
    Ok(bytes)
}

fn open_regular_file(path: &Path, label: &str) -> Result<File, String> {
    let file = open_file(path, label)?;
    if !file
        .metadata()
        .map_err(|error| format!("cannot inspect {label}: {error}"))?
        .is_file()
    {
        return Err(format!("{label} is not a regular file"));
    }
    Ok(file)
}

/// Opens metadata without allowing a FIFO or device path to block discovery indefinitely.
#[cfg(unix)]
fn open_file(path: &Path, label: &str) -> Result<File, String> {
    use std::os::unix::fs::OpenOptionsExt;

    OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_NONBLOCK)
        .open(path)
        .map_err(|error| format!("cannot open {label}: {error}"))
}

#[cfg(windows)]
fn open_file(path: &Path, label: &str) -> Result<File, String> {
    File::open(path).map_err(|error| format!("cannot open {label}: {error}"))
}

/// Parses the metadata contract used by the pinned Codex loader.
///
/// Unlike selected-catalog validation, an absent or blank `name` falls back to the containing
/// directory name, whitespace is collapsed, and a narrow repair accepts common unquoted prose
/// containing `: `. The local size bound is stricter than Codex's read path; crossing it therefore
/// makes the conflict inventory incomplete instead of pretending the Skill was rejected.
pub(crate) fn codex_metadata(
    path: &Path,
    containing_directory: &Path,
) -> Result<SkillMetadata, CodexMetadataError> {
    let mut file = open_regular_skill_md(path).map_err(CodexMetadataError::Incomplete)?;
    let mut bytes = Vec::new();
    file.by_ref()
        .take(MAX_SKILL_MD_BYTES + 1)
        .read_to_end(&mut bytes)
        .map_err(|error| {
            CodexMetadataError::Incomplete(format!("cannot read SKILL.md: {error}"))
        })?;
    if u64::try_from(bytes.len()).unwrap_or(u64::MAX) > MAX_SKILL_MD_BYTES {
        return Err(CodexMetadataError::Incomplete(format!(
            "SKILL.md exceeds SkillMount's {MAX_SKILL_MD_BYTES}-byte inventory bound"
        )));
    }
    let content = String::from_utf8(bytes)
        .map_err(|_| CodexMetadataError::Rejected("SKILL.md is not valid UTF-8".to_owned()))?;
    let raw = codex_envelope(&content).ok_or_else(|| {
        CodexMetadataError::Rejected("missing YAML frontmatter delimited by ---".to_owned())
    })?;
    let value = match serde_yaml_ng::from_str::<Value>(&raw) {
        Ok(value) => value,
        Err(original_error) => {
            let Some(repaired) = repair_codex_scalar_fields(&raw) else {
                return Err(CodexMetadataError::Rejected(format!(
                    "invalid YAML frontmatter: {original_error}"
                )));
            };
            serde_yaml_ng::from_str::<Value>(&repaired).map_err(|_| {
                CodexMetadataError::Rejected(format!("invalid YAML frontmatter: {original_error}"))
            })?
        }
    };
    let mapping = value.as_mapping().ok_or_else(|| {
        CodexMetadataError::Rejected("YAML frontmatter must be a mapping".to_owned())
    })?;
    validate_codex_metadata_shape(mapping)?;

    let name = codex_optional_string(mapping, "name")?
        .as_deref()
        .map(sanitize_single_line)
        .filter(|value| !value.is_empty())
        .unwrap_or_else(|| default_codex_name(containing_directory));
    if name.chars().count() > CODEX_MAX_NAME_CHARS {
        return Err(CodexMetadataError::Rejected(format!(
            "name exceeds maximum length of {CODEX_MAX_NAME_CHARS} characters"
        )));
    }
    let description = codex_optional_string(mapping, "description")?
        .as_deref()
        .map(sanitize_single_line)
        .unwrap_or_default();
    if description.is_empty() {
        return Err(CodexMetadataError::Rejected(
            "missing field `description`".to_owned(),
        ));
    }
    Ok(SkillMetadata {
        name: Some(name),
        description: Some(description),
    })
}

fn codex_envelope(content: &str) -> Option<String> {
    let mut lines = content.lines();
    if !matches!(lines.next(), Some(line) if line.trim() == "---") {
        return None;
    }
    let mut frontmatter = Vec::new();
    let mut found_closing = false;
    for line in lines {
        if line.trim() == "---" {
            found_closing = true;
            break;
        }
        frontmatter.push(line);
    }
    if frontmatter.is_empty() || !found_closing {
        return None;
    }
    Some(frontmatter.join("\n"))
}

fn codex_optional_string(
    mapping: &serde_yaml_ng::Mapping,
    key: &str,
) -> Result<Option<String>, CodexMetadataError> {
    let Some(value) = mapping.get(Value::String(key.to_owned())) else {
        return Ok(None);
    };
    if value.is_null() {
        return Ok(None);
    }
    value
        .as_str()
        .map(|value| Some(value.to_owned()))
        .ok_or_else(|| {
            CodexMetadataError::Rejected(format!("frontmatter field {key:?} must be a string"))
        })
}

fn validate_codex_metadata_shape(
    mapping: &serde_yaml_ng::Mapping,
) -> Result<(), CodexMetadataError> {
    let Some(metadata) = mapping.get(Value::String("metadata".to_owned())) else {
        return Ok(());
    };
    let Some(metadata) = metadata.as_mapping() else {
        return Err(CodexMetadataError::Rejected(
            "frontmatter field \"metadata\" must be a mapping".to_owned(),
        ));
    };
    let Some(short_description) = metadata.get(Value::String("short-description".to_owned()))
    else {
        return Ok(());
    };
    if short_description.is_null() || short_description.as_str().is_some() {
        Ok(())
    } else {
        Err(CodexMetadataError::Rejected(
            "frontmatter field \"metadata.short-description\" must be a string".to_owned(),
        ))
    }
}

fn default_codex_name(directory: &Path) -> String {
    directory
        .file_name()
        .and_then(|name| name.to_str())
        .map(sanitize_single_line)
        .filter(|name| !name.is_empty())
        .unwrap_or_else(|| "skill".to_owned())
}

fn sanitize_single_line(raw: &str) -> String {
    raw.split_whitespace().collect::<Vec<_>>().join(" ")
}

fn repair_codex_scalar_fields(frontmatter: &str) -> Option<String> {
    let mut changed = false;
    let mut block_scalar_indent: Option<usize> = None;
    let mut repaired_lines = Vec::new();
    for line in frontmatter.lines() {
        let indent = line
            .chars()
            .take_while(|character| *character == ' ')
            .count();
        if let Some(block_indent) = block_scalar_indent {
            if line.trim().is_empty() || indent > block_indent {
                repaired_lines.push(line.to_owned());
                continue;
            }
            block_scalar_indent = None;
        }

        let Some((key, value)) = line.split_once(':') else {
            repaired_lines.push(line.to_owned());
            continue;
        };
        if key.trim().is_empty() || !value.chars().next().is_none_or(char::is_whitespace) {
            repaired_lines.push(line.to_owned());
            continue;
        }

        let trimmed_start = value.trim_start();
        let leading_whitespace = &value[..value.len() - trimmed_start.len()];
        let mut scalar = trimmed_start;
        let mut comment = "";
        for (index, character) in trimmed_start.char_indices() {
            if character == '#'
                && (index == 0
                    || trimmed_start[..index]
                        .chars()
                        .next_back()
                        .is_some_and(char::is_whitespace))
            {
                let comment_start = trimmed_start[..index].trim_end().len();
                scalar = &trimmed_start[..comment_start];
                comment = &trimmed_start[comment_start..];
                break;
            }
        }

        let scalar = scalar.trim_end();
        let Some(first_char) = scalar.chars().next() else {
            repaired_lines.push(line.to_owned());
            continue;
        };
        if matches!(first_char, '|' | '>') {
            block_scalar_indent = Some(indent);
            repaired_lines.push(line.to_owned());
            continue;
        }
        if matches!(first_char, '\'' | '"') {
            repaired_lines.push(line.to_owned());
            continue;
        }
        let mut has_colon_separator = false;
        let mut characters = scalar.chars().peekable();
        while let Some(character) = characters.next() {
            if character == ':' && matches!(characters.peek(), Some(next) if next.is_whitespace()) {
                has_colon_separator = true;
                break;
            }
        }
        let invalid_flow_like_scalar = matches!(first_char, '[' | '{' | '@' | '`')
            && serde_yaml_ng::from_str::<Value>(scalar).is_err();
        if !has_colon_separator && !invalid_flow_like_scalar {
            repaired_lines.push(line.to_owned());
            continue;
        }

        let quoted_scalar = format!("'{}'", scalar.replace('\'', "''"));
        repaired_lines.push(format!(
            "{key}:{leading_whitespace}{quoted_scalar}{comment}"
        ));
        changed = true;
    }
    changed.then(|| repaired_lines.join("\n"))
}

fn envelope(content: &str) -> Result<&str, String> {
    let Some(after_start) = content
        .strip_prefix("---\n")
        .or_else(|| content.strip_prefix("---\r\n"))
    else {
        return Err("SKILL.md must begin with a YAML frontmatter envelope".to_owned());
    };
    let mut offset = 0;
    for line in after_start.split_inclusive('\n') {
        let line_text = line.trim_end_matches(['\r', '\n']);
        if line_text == "---" {
            if offset > MAX_FRONTMATTER_BYTES {
                return Err(format!("frontmatter exceeds {MAX_FRONTMATTER_BYTES} bytes"));
            }
            return Ok(&after_start[..offset]);
        }
        offset += line.len();
        if offset > MAX_FRONTMATTER_BYTES {
            return Err(format!("frontmatter exceeds {MAX_FRONTMATTER_BYTES} bytes"));
        }
    }
    Err("SKILL.md frontmatter is missing its closing delimiter".to_owned())
}

fn known_string(mapping: &serde_yaml_ng::Mapping, key: &str) -> Result<Option<String>, String> {
    let Some(value) = mapping.get(Value::String(key.to_owned())) else {
        return Ok(None);
    };
    value
        .as_str()
        .map(|value| Some(value.to_owned()))
        .ok_or_else(|| format!("frontmatter field {key:?} must be a string"))
}

#[cfg(test)]
mod tests {
    use super::{codex_metadata, parse};
    #[cfg(unix)]
    use super::{read_bounded_regular_file, readable};
    use std::fs;
    use std::path::{Path, PathBuf};
    use std::sync::atomic::{AtomicU64, Ordering};
    use std::time::{SystemTime, UNIX_EPOCH};

    static NEXT_FIXTURE: AtomicU64 = AtomicU64::new(0);

    fn fixture(contents: &str) -> PathBuf {
        let nonce = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .expect("clock")
            .as_nanos();
        let path = std::env::temp_dir().join(format!(
            "skillmount-frontmatter-{}-{nonce}-{}.md",
            std::process::id(),
            NEXT_FIXTURE.fetch_add(1, Ordering::Relaxed)
        ));
        fs::write(&path, contents).expect("fixture");
        path
    }

    fn codex_fixture(directory_name: &str, contents: &str) -> PathBuf {
        let nonce = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .expect("clock")
            .as_nanos();
        let root = std::env::temp_dir().join(format!(
            "skillmount-codex-frontmatter-{}-{nonce}-{}",
            std::process::id(),
            NEXT_FIXTURE.fetch_add(1, Ordering::Relaxed)
        ));
        let directory = root.join(directory_name);
        fs::create_dir_all(&directory).expect("Codex Skill fixture");
        let path = directory.join("SKILL.md");
        fs::write(&path, contents).expect("Codex Skill metadata");
        path
    }

    #[test]
    fn extracts_known_fields_and_keeps_unknown_yaml_unchanged() {
        let path = fixture(
            "---\nname: demo\ndescription: |\n  first\n  second\nunknown: [a, b]\n---\nbody\n",
        );
        let parsed = parse(&path).expect("frontmatter should parse");
        fs::remove_file(path).expect("fixture cleanup");

        assert_eq!(parsed.metadata.name.as_deref(), Some("demo"));
        assert_eq!(
            parsed.metadata.description.as_deref(),
            Some("first\nsecond\n")
        );
        assert!(parsed.raw.contains("unknown: [a, b]"));
    }

    #[test]
    fn rejects_unbounded_or_missing_envelopes() {
        let path = fixture("name: demo\n");
        assert!(parse(&path).is_err());
        fs::remove_file(path).expect("fixture cleanup");
    }

    #[test]
    fn codex_missing_or_blank_name_falls_back_to_the_directory() {
        for contents in [
            "---\ndescription: fallback fixture\n---\n",
            "---\nname:\ndescription: fallback fixture\n---\n",
            "  ---  \nname: '  '\ndescription: fallback fixture\n --- \n",
        ] {
            let path = codex_fixture("fallback-skill", contents);
            let metadata = codex_metadata(&path, path.parent().expect("Skill directory"))
                .expect("Codex-compatible metadata");
            fs::remove_dir_all(path.parent().and_then(Path::parent).expect("fixture root"))
                .expect("fixture cleanup");

            assert_eq!(metadata.name.as_deref(), Some("fallback-skill"));
        }
    }

    #[test]
    fn codex_repairs_colon_prose_and_collapses_metadata_whitespace() {
        let path = codex_fixture(
            "repair",
            "---\nname:  team   deploy\ndescription: Build for AWS: ECS and Lambda\n---\n",
        );

        let metadata = codex_metadata(&path, path.parent().expect("Skill directory"))
            .expect("the pinned Codex repair accepts the prose");
        fs::remove_dir_all(path.parent().and_then(Path::parent).expect("fixture root"))
            .expect("fixture cleanup");

        assert_eq!(metadata.name.as_deref(), Some("team deploy"));
        assert_eq!(
            metadata.description.as_deref(),
            Some("Build for AWS: ECS and Lambda")
        );
    }

    #[cfg(unix)]
    #[test]
    fn rejects_a_fifo_without_waiting_for_a_writer() {
        use nix::sys::stat::Mode;
        use nix::unistd::mkfifo;

        let path = fixture("placeholder");
        fs::remove_file(&path).expect("replace regular fixture");
        mkfifo(&path, Mode::S_IRUSR | Mode::S_IWUSR).expect("FIFO fixture");

        let error = parse(&path).expect_err("a FIFO is not Skill metadata");

        fs::remove_file(path).expect("fixture cleanup");
        assert!(error.contains("not a regular file"), "{error}");
    }

    #[cfg(unix)]
    #[test]
    fn readability_only_validation_rejects_a_fifo_without_waiting_for_a_writer() {
        use nix::sys::stat::Mode;
        use nix::unistd::mkfifo;

        let path = fixture("placeholder");
        fs::remove_file(&path).expect("replace regular fixture");
        mkfifo(&path, Mode::S_IRUSR | Mode::S_IWUSR).expect("FIFO fixture");

        let error = readable(&path).expect_err("a FIFO is not readable Skill metadata");

        fs::remove_file(path).expect("fixture cleanup");
        assert!(error.contains("not a regular file"), "{error}");
    }

    #[cfg(unix)]
    #[test]
    fn bounded_regular_file_reader_rejects_a_fifo_without_waiting_for_a_writer() {
        use nix::sys::stat::Mode;
        use nix::unistd::mkfifo;

        let path = fixture("placeholder");
        fs::remove_file(&path).expect("replace regular fixture");
        mkfifo(&path, Mode::S_IRUSR | Mode::S_IWUSR).expect("FIFO fixture");

        let error = read_bounded_regular_file(&path, "manifest", 1024)
            .expect_err("a FIFO is not a bounded regular file");

        fs::remove_file(path).expect("fixture cleanup");
        assert!(error.contains("not a regular file"), "{error}");
    }
}
