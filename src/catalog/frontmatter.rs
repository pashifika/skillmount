use std::fs::File;
use std::io::Read;
use std::path::Path;

use serde_yaml_ng::Value;

use crate::domain::SkillMetadata;

const MAX_SKILL_MD_BYTES: u64 = 1024 * 1024;
const MAX_FRONTMATTER_BYTES: usize = 64 * 1024;

#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) struct ParsedFrontmatter {
    pub(super) metadata: SkillMetadata,
    pub(super) raw: String,
}

pub(super) fn parse(path: &Path) -> Result<ParsedFrontmatter, String> {
    let mut file = File::open(path).map_err(|error| format!("cannot open SKILL.md: {error}"))?;
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

/// Parses the known metadata fields without exposing the validation-only envelope representation.
pub(crate) fn metadata(path: &Path) -> Result<SkillMetadata, String> {
    parse(path).map(|parsed| parsed.metadata)
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
    use super::parse;
    use std::fs;
    use std::path::PathBuf;
    use std::time::{SystemTime, UNIX_EPOCH};

    fn fixture(contents: &str) -> PathBuf {
        let nonce = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .expect("clock")
            .as_nanos();
        let path = std::env::temp_dir().join(format!(
            "skillmount-frontmatter-{}-{nonce}.md",
            std::process::id()
        ));
        fs::write(&path, contents).expect("fixture");
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
}
