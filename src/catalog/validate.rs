use std::collections::BTreeSet;
use std::fs;
use std::path::{Path, PathBuf};

use crate::catalog::frontmatter;
use crate::catalog::{CatalogRequest, RawCandidate};
use crate::diagnostic::Diagnostic;
use crate::domain::{CatalogPolicy, Skill, SkillMetadata, SkillName, ValidationLevel};
use crate::error::{AppError, CatalogError};
use crate::paths::{canonical_anchor, lexical_normalize};

const MAX_LINK_DEPTH: usize = 40;

pub(super) fn validate_candidate(
    candidate: &RawCandidate,
    request: &CatalogRequest<'_>,
) -> Result<(Skill, Option<Diagnostic>), AppError> {
    let mount_name =
        SkillName::parse(&candidate.raw_name).map_err(|error| CatalogError::InvalidSkillName {
            path: candidate.origin.source_entry.clone(),
            reason: error.to_string(),
        })?;

    if request.policy.requires_exact_skill_md_entry {
        let display = request.agent.descriptor().display_name();
        let exact_entry = super::find_exact_entry(
            &candidate.origin.source_entry,
            std::ffi::OsStr::new("SKILL.md"),
        )
        .map_err(|error| CatalogError::InvalidSelectedSkill {
            path: candidate.origin.source_entry.clone(),
            reason: format!("cannot enumerate the Skill directory: {error}"),
        })?
        .ok_or_else(|| CatalogError::InvalidSelectedSkill {
            path: candidate.origin.source_entry.clone(),
            reason: format!("{display} requires an exact SKILL.md directory-entry name"),
        })?;
        if exact_entry.path() != candidate.skill_md {
            return Err(CatalogError::InvalidSelectedSkill {
                path: candidate.origin.source_entry.clone(),
                reason: "the exact SKILL.md directory entry changed during catalog validation"
                    .to_owned(),
            }
            .into());
        }
        let entry_metadata = fs::symlink_metadata(exact_entry.path()).map_err(|error| {
            CatalogError::InvalidSelectedSkill {
                path: candidate.origin.source_entry.clone(),
                reason: format!("cannot inspect SKILL.md entry: {error}"),
            }
        })?;
        if !entry_metadata.file_type().is_file() {
            return Err(CatalogError::InvalidSelectedSkill {
                path: candidate.origin.source_entry.clone(),
                reason: format!(
                    "{display} discovers only regular SKILL.md entries, not file links or other special files"
                ),
            }
            .into());
        }
    }

    let canonical_directory = validate_directory(candidate)?;
    let canonical_skill_md = resolve_terminal(&candidate.skill_md).map_err(|reason| {
        CatalogError::InvalidSelectedSkill {
            path: candidate.origin.source_entry.clone(),
            reason,
        }
    })?;
    let terminal_metadata =
        fs::metadata(&canonical_skill_md).map_err(|error| CatalogError::InvalidSelectedSkill {
            path: candidate.origin.source_entry.clone(),
            reason: format!("cannot inspect terminal SKILL.md: {error}"),
        })?;
    if !terminal_metadata.is_file() {
        return Err(CatalogError::InvalidSelectedSkill {
            path: candidate.origin.source_entry.clone(),
            reason: "SKILL.md terminal target is not a regular file".to_owned(),
        }
        .into());
    }
    if !canonical_skill_md.starts_with(&canonical_directory) {
        return Err(CatalogError::InvalidSelectedSkill {
            path: candidate.origin.source_entry.clone(),
            reason: format!(
                "SKILL.md resolves outside the Skill directory to {}",
                canonical_skill_md.display()
            ),
        }
        .into());
    }

    validate_destination_cycles(
        &candidate.input_canonical,
        &canonical_directory,
        request.destination_stores,
    )?;

    let (metadata, warning) = validate_metadata(
        &candidate.skill_md,
        &mount_name,
        request.policy,
        request.validation,
    )?;
    let mut origin = candidate.origin.clone();
    origin.source_canonical = canonical_directory;
    Ok((
        Skill {
            mount_name,
            origin,
            skill_md: candidate.skill_md.clone(),
            metadata,
        },
        warning,
    ))
}

fn validate_directory(candidate: &RawCandidate) -> Result<PathBuf, AppError> {
    let metadata = fs::metadata(&candidate.origin.source_entry).map_err(|error| {
        CatalogError::InvalidSelectedSkill {
            path: candidate.origin.source_entry.clone(),
            reason: format!("Skill directory has no valid terminal target: {error}"),
        }
    })?;
    if !metadata.is_dir() {
        return Err(CatalogError::InvalidSelectedSkill {
            path: candidate.origin.source_entry.clone(),
            reason: "Skill entry is not a directory".to_owned(),
        }
        .into());
    }
    fs::read_dir(&candidate.origin.source_entry).map_err(|error| {
        CatalogError::InvalidSelectedSkill {
            path: candidate.origin.source_entry.clone(),
            reason: format!("Skill directory is not readable: {error}"),
        }
    })?;
    fs::canonicalize(&candidate.origin.source_entry).map_err(|error| {
        CatalogError::InvalidSelectedSkill {
            path: candidate.origin.source_entry.clone(),
            reason: format!("Skill directory cannot be canonicalized: {error}"),
        }
        .into()
    })
}

fn resolve_terminal(path: &Path) -> Result<PathBuf, String> {
    let mut current = path.to_path_buf();
    let mut visited = BTreeSet::new();
    for _ in 0..MAX_LINK_DEPTH {
        current = lexical_normalize(&current);
        if !visited.insert(current.clone()) {
            return Err("SKILL.md link cycle detected".to_owned());
        }
        let metadata = fs::symlink_metadata(&current)
            .map_err(|error| format!("SKILL.md entry is missing or broken: {error}"))?;
        if !metadata.file_type().is_symlink() {
            return fs::canonicalize(&current)
                .map_err(|error| format!("cannot canonicalize SKILL.md: {error}"));
        }
        let target = fs::read_link(&current)
            .map_err(|error| format!("cannot read SKILL.md link: {error}"))?;
        current = if target.is_absolute() {
            target
        } else {
            current
                .parent()
                .unwrap_or_else(|| Path::new(""))
                .join(target)
        };
    }
    Err(format!("SKILL.md link depth exceeds {MAX_LINK_DEPTH}"))
}

fn validate_destination_cycles(
    input_source: &Path,
    selected_source: &Path,
    destinations: &[PathBuf],
) -> Result<(), AppError> {
    for destination in destinations {
        let destination = canonical_anchor(destination);
        for source in [input_source, selected_source] {
            if destination.starts_with(source) || source.starts_with(&destination) {
                return Err(CatalogError::SourceDestinationCycle {
                    source: source.to_path_buf(),
                    destination,
                }
                .into());
            }
        }
    }
    Ok(())
}

fn validate_metadata(
    skill_md: &Path,
    mount_name: &SkillName,
    policy: CatalogPolicy,
    level: ValidationLevel,
) -> Result<(SkillMetadata, Option<Diagnostic>), AppError> {
    if level == ValidationLevel::None && !policy.always_parses_metadata {
        frontmatter::readable(skill_md).map_err(|reason| CatalogError::InvalidSelectedSkill {
            path: skill_md.to_path_buf(),
            reason: format!("SKILL.md is not readable: {reason}"),
        })?;
        return Ok((
            SkillMetadata::default(),
            Some(Diagnostic::warning(
                "metadata validation is disabled; adapter compatibility is not guaranteed",
                skill_md,
            )),
        ));
    }

    let parsed =
        frontmatter::parse(skill_md).map_err(|reason| CatalogError::InvalidSelectedSkill {
            path: skill_md.to_path_buf(),
            reason,
        })?;
    let metadata = parsed.metadata;
    let requires_name = level == ValidationLevel::Strict || policy.requires_name;
    if requires_name && metadata.name.as_deref().is_none_or(blank) {
        return Err(CatalogError::InvalidSelectedSkill {
            path: skill_md.to_path_buf(),
            reason: "SKILL.md is missing non-empty field \"name\"".to_owned(),
        }
        .into());
    }
    if policy.requires_description && metadata.description.as_deref().is_none_or(blank) {
        return Err(CatalogError::InvalidSelectedSkill {
            path: skill_md.to_path_buf(),
            reason: "SKILL.md is missing non-empty field \"description\"".to_owned(),
        }
        .into());
    }
    if let Some(name) = &metadata.name {
        if policy.requires_matching_name && name != mount_name.as_str() {
            return Err(CatalogError::InvalidSelectedSkill {
                path: skill_md.to_path_buf(),
                reason: format!(
                    "frontmatter name {name:?} does not match directory {:?}",
                    mount_name.as_str()
                ),
            }
            .into());
        }
        if level == ValidationLevel::Strict {
            SkillName::parse(name.as_ref()).map_err(|error| {
                CatalogError::InvalidSelectedSkill {
                    path: skill_md.to_path_buf(),
                    reason: format!("frontmatter name is not portable: {error}"),
                }
            })?;
        }
    }
    Ok((metadata, None))
}

fn blank(value: &str) -> bool {
    value.trim().is_empty()
}
