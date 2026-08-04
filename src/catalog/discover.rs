use std::ffi::{OsStr, OsString};
use std::fs;
use std::path::{Path, PathBuf};

use crate::domain::{SkillNameKey, SkillOrigin, SkillSource, SourceOccurrence};
use crate::error::{AppError, CatalogError};

#[derive(Debug, Clone)]
pub(crate) struct RawCandidate {
    pub(super) raw_name: OsString,
    pub(super) comparison_key: SkillNameKey,
    pub(super) origin: SkillOrigin,
    pub(super) canonical_valid: bool,
    pub(super) input_canonical: PathBuf,
    pub(super) skill_md: PathBuf,
}

#[derive(Debug)]
pub(super) struct DiscoveredSource {
    pub(super) source: SkillSource,
    pub(super) candidates: Vec<RawCandidate>,
}

pub(super) fn discover_sources(
    occurrences: &[SourceOccurrence],
) -> Result<Vec<DiscoveredSource>, AppError> {
    // Accessibility is checked for every occurrence before any source is classified. This
    // prevents an earlier empty catalog from hiding a later missing input.
    let sources = occurrences
        .iter()
        .map(prepare_source)
        .collect::<Result<Vec<_>, _>>()?;

    sources
        .into_iter()
        .map(|source| {
            let candidates = scan_source(&source)?;
            if candidates.is_empty() {
                return Err(CatalogError::EmptyCatalog {
                    source_ordinal: source.ordinal,
                    path: source.resolved_path.clone(),
                }
                .into());
            }
            Ok(DiscoveredSource { source, candidates })
        })
        .collect()
}

fn prepare_source(occurrence: &SourceOccurrence) -> Result<SkillSource, AppError> {
    let path = &occurrence.resolved_path;
    let metadata = fs::metadata(path).map_err(|error| AppError::MissingInput {
        path: path.clone(),
        reason: error.to_string(),
    })?;
    if !metadata.is_dir() {
        return Err(AppError::MissingInput {
            path: path.clone(),
            reason: "expected a directory or directory link".to_owned(),
        });
    }
    fs::read_dir(path).map_err(|error| AppError::MissingInput {
        path: path.clone(),
        reason: error.to_string(),
    })?;
    let canonical_path = fs::canonicalize(path).map_err(|error| AppError::MissingInput {
        path: path.clone(),
        reason: error.to_string(),
    })?;
    Ok(SkillSource {
        ordinal: occurrence.ordinal,
        input_path: occurrence.input_path.clone(),
        resolved_path: path.clone(),
        canonical_path,
    })
}

fn scan_source(source: &SkillSource) -> Result<Vec<RawCandidate>, AppError> {
    match super::find_exact_entry(&source.resolved_path, OsStr::new("SKILL.md")).map_err(
        |error| AppError::MissingInput {
            path: source.resolved_path.clone(),
            reason: error.to_string(),
        },
    )? {
        Some(entry) => {
            let name = source
                .resolved_path
                .file_name()
                .unwrap_or_else(|| OsStr::new(""));
            Ok(vec![candidate(
                source,
                &source.resolved_path,
                name,
                entry.path(),
            )])
        }
        None => scan_catalog(source),
    }
}

fn scan_catalog(source: &SkillSource) -> Result<Vec<RawCandidate>, AppError> {
    let entries = fs::read_dir(&source.resolved_path).map_err(|error| AppError::MissingInput {
        path: source.resolved_path.clone(),
        reason: error.to_string(),
    })?;
    let mut candidates = Vec::new();

    for entry in entries {
        let entry = entry.map_err(|error| AppError::MissingInput {
            path: source.resolved_path.clone(),
            reason: error.to_string(),
        })?;
        let file_type = entry.file_type().map_err(|error| AppError::MissingInput {
            path: entry.path(),
            reason: error.to_string(),
        })?;
        if !file_type.is_dir() && !directory_link_target_is_directory(&entry, file_type)? {
            continue;
        }

        let entry_path = entry.path();
        if let Some(skill_md) = super::find_exact_entry(&entry_path, OsStr::new("SKILL.md"))
            .map_err(|error| AppError::MissingInput {
                path: entry_path.clone(),
                reason: error.to_string(),
            })?
        {
            candidates.push(candidate(
                source,
                &entry_path,
                &entry.file_name(),
                skill_md.path(),
            ));
        }
    }

    candidates.sort_by(|left, right| {
        left.comparison_key
            .cmp(&right.comparison_key)
            .then_with(|| left.raw_name.cmp(&right.raw_name))
            .then_with(|| left.origin.source_entry.cmp(&right.origin.source_entry))
    });
    Ok(candidates)
}

fn directory_link_target_is_directory(
    entry: &fs::DirEntry,
    file_type: fs::FileType,
) -> Result<bool, AppError> {
    if !file_type.is_symlink() {
        return Ok(false);
    }
    match fs::metadata(entry.path()) {
        Ok(metadata) => Ok(metadata.is_dir()),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(false),
        Err(error) => Err(AppError::MissingInput {
            path: entry.path(),
            reason: error.to_string(),
        }),
    }
}

fn candidate(source: &SkillSource, entry: &Path, name: &OsStr, skill_md: PathBuf) -> RawCandidate {
    let canonical = fs::canonicalize(entry);
    let (source_canonical, canonical_valid) = match canonical {
        Ok(path) => (path, true),
        Err(_) => (entry.to_path_buf(), false),
    };
    RawCandidate {
        raw_name: name.to_os_string(),
        comparison_key: SkillNameKey::new(name),
        origin: SkillOrigin {
            source_ordinal: source.ordinal,
            source_entry: entry.to_path_buf(),
            source_canonical,
        },
        canonical_valid,
        input_canonical: source.canonical_path.clone(),
        skill_md,
    }
}
