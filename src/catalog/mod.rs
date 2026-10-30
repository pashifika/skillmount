//! Side-effect-free Skill source discovery, overlay resolution, and validation.

mod discover;
pub(crate) mod frontmatter;
mod resolve;
mod validate;

#[cfg(test)]
mod tests;

use std::ffi::OsStr;
use std::fs::{self, DirEntry};
use std::io;
use std::path::{Path, PathBuf};

use crate::domain::{AgentId, CatalogPolicy, SkillCatalog, SourceOccurrence, ValidationLevel};
use crate::error::AppError;

pub(crate) use discover::RawCandidate;

/// Finds a child by its exact directory-entry name instead of a case-folding path lookup.
pub(crate) fn find_exact_entry(directory: &Path, name: &OsStr) -> io::Result<Option<DirEntry>> {
    for entry in fs::read_dir(directory)? {
        let entry = entry?;
        if entry.file_name() == name {
            return Ok(Some(entry));
        }
    }
    Ok(None)
}

/// Read-only inputs that control catalog validation.
#[derive(Debug, Clone)]
pub struct CatalogRequest<'a> {
    /// Adapter whose metadata contract is active; used only to name it in a diagnostic.
    pub agent: AgentId,
    /// Declarative Agent requirements contributed by the selected adapter.
    ///
    /// A policy may only strengthen compatibility requirements. Structural, canonicalization,
    /// destination-cycle, portable-name, selection-order, and no-fallback checks stay
    /// unconditional.
    pub policy: CatalogPolicy,
    /// Metadata validation policy.
    pub validation: ValidationLevel,
    /// Future destination stores used only for cycle detection.
    pub destination_stores: &'a [PathBuf],
}

/// Resolves an ordered set of source occurrences into a validated immutable catalog.
///
/// This function performs reads only. It never creates directories, links, locks, journals,
/// or child processes.
///
/// # Errors
///
/// Returns a typed input error when a source is unavailable, or a catalog error when discovery,
/// overlay, structural validation, metadata validation, alias checking, or cycle checking fails.
pub fn resolve_catalog(
    occurrences: &[SourceOccurrence],
    request: &CatalogRequest<'_>,
) -> Result<SkillCatalog, AppError> {
    let sources = discover::discover_sources(occurrences)?;
    let pending = resolve::fold_sources(sources)?;
    resolve::validate_resolutions(pending, request)
}
