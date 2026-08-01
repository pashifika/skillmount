use std::collections::BTreeMap;
use std::ffi::OsString;
use std::path::PathBuf;

use crate::catalog::CatalogRequest;
use crate::catalog::discover::{DiscoveredSource, NativeNameKey, RawCandidate};
use crate::catalog::validate::validate_candidate;
use crate::domain::{ResolvedSkill, ShadowReason, ShadowedSkill, SkillCatalog};
use crate::error::{AppError, CatalogError};

#[derive(Debug)]
pub(super) struct PendingResolution {
    pub(super) selected: RawCandidate,
    pub(super) shadowed: Vec<ShadowedSkill>,
}

pub(super) fn fold_sources(
    sources: Vec<DiscoveredSource>,
) -> Result<Vec<PendingResolution>, AppError> {
    let mut selected = BTreeMap::<NativeNameKey, PendingResolution>::new();

    for source in sources {
        let mut within_source = BTreeMap::<NativeNameKey, RawCandidate>::new();
        for candidate in source.candidates {
            if let Some(first) =
                within_source.insert(candidate.comparison_key.clone(), candidate.clone())
            {
                return Err(CatalogError::DuplicateLogicalName {
                    source_ordinal: source.source.ordinal,
                    first: first.origin.source_entry,
                    second: candidate.origin.source_entry,
                }
                .into());
            }
        }

        for (key, candidate) in within_source {
            let pending = if let Some(previous) = selected.remove(&key) {
                let reason = shadow_reason(&previous.selected, &candidate);
                let mut shadowed = previous.shadowed;
                shadowed.push(ShadowedSkill {
                    origin: previous.selected.origin,
                    reason,
                });
                PendingResolution {
                    selected: candidate,
                    shadowed,
                }
            } else {
                PendingResolution {
                    selected: candidate,
                    shadowed: Vec::new(),
                }
            };
            selected.insert(key, pending);
        }
    }

    Ok(selected.into_values().collect())
}

pub(super) fn validate_resolutions(
    pending: Vec<PendingResolution>,
    request: &CatalogRequest<'_>,
) -> Result<SkillCatalog, AppError> {
    reject_canonical_aliases(&pending)?;

    let mut warnings = Vec::new();
    let mut resolutions = Vec::with_capacity(pending.len());
    for pending in pending {
        let (selected, warning) = validate_candidate(&pending.selected, request)?;
        if let Some(warning) = warning {
            warnings.push(warning);
        }
        resolutions.push(ResolvedSkill {
            selected,
            shadowed: pending.shadowed,
        });
    }
    resolutions.sort_by_key(|resolution| resolution.selected.mount_name.comparison_key());
    Ok(SkillCatalog {
        resolutions,
        warnings,
    })
}

fn shadow_reason(previous: &RawCandidate, replacement: &RawCandidate) -> ShadowReason {
    if previous.canonical_valid
        && replacement.canonical_valid
        && previous.origin.source_canonical == replacement.origin.source_canonical
    {
        ShadowReason::RepeatedCanonicalSource
    } else {
        ShadowReason::DifferentSourceOverride
    }
}

fn reject_canonical_aliases(pending: &[PendingResolution]) -> Result<(), AppError> {
    let mut canonical_names = BTreeMap::<PathBuf, OsString>::new();
    for resolution in pending {
        if !resolution.selected.canonical_valid {
            continue;
        }
        let canonical = resolution.selected.origin.source_canonical.clone();
        if let Some(first_name) =
            canonical_names.insert(canonical.clone(), resolution.selected.raw_name.clone())
        {
            if first_name != resolution.selected.raw_name {
                return Err(CatalogError::CanonicalAlias {
                    canonical,
                    first_name,
                    second_name: resolution.selected.raw_name.clone(),
                }
                .into());
            }
        }
    }
    Ok(())
}
