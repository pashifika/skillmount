//! The shared destination-conflict table, applied to selected candidates only.

use std::ffi::OsString;

use crate::agent::{DiscoveryScope, DiscoverySnapshot, ExistingSkill};
use crate::domain::{ConflictPolicy, RunContext, SkillCatalog};
use crate::error::{AppError, PlanError};
use crate::mount::resolve::PathKind;
use crate::mount::{ActionSequence, MountAction, PathPrecondition, PreservedSkill};

/// How an already-visible entry relates to the Skill that wants its logical name.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Occupancy {
    /// A directory link that already points at the selected canonical source.
    SameSource,
    /// A directory link pointing somewhere else.
    DifferentSource,
    /// A regular directory, treated as a project-owned Skill.
    ProjectDirectory,
    /// A broken, cyclic, over-deep, or non-directory entry.
    Unsupported,
}

impl Occupancy {
    /// Returns whether `--conflict=skip` may preserve this entry.
    ///
    /// `skip` means "the existing Skill is good enough". That claim cannot be made about an entry
    /// whose target is unknown, so an unsupported entry fails under both policies.
    const fn is_skippable(self) -> bool {
        matches!(self, Self::DifferentSource | Self::ProjectDirectory)
    }
}

/// Applies the conflict table to every selected Skill and appends the resulting actions.
///
/// Only the final selected candidate of each logical name is considered. A shadowed candidate is
/// never inspected here, so skipping a winner can never reveal a lower-precedence source.
///
/// # Errors
///
/// Returns [`AppError::Plan`] on the first unresolvable conflict. Planning is still read-only at
/// that point, so no earlier candidate has been applied.
pub(crate) fn apply_conflict_policy(
    context: &RunContext,
    catalog: &SkillCatalog,
    discovery: &DiscoverySnapshot,
    actions: &mut ActionSequence,
    preserved: &mut Vec<PreservedSkill>,
) -> Result<(), AppError> {
    let policy = context.options.conflict;
    let mount_scope = discovery.mount_scope();

    for resolution in &catalog.resolutions {
        let skill = &resolution.selected;
        let key = skill.mount_name.comparison_key();
        let source = &skill.origin.source_canonical;
        let destination = discovery.backing_store.join(skill.mount_name.as_str());

        let visible = visible_occupant(discovery, &key, source);
        if let Some((scope, existing, other)) =
            visible.filter(|(_, _, occupancy)| *occupancy != Occupancy::SameSource)
        {
            if other.is_skippable() && policy == ConflictPolicy::Skip {
                preserved.push(preserve(scope, existing, source));
                continue;
            }
            return Err(conflict(scope, existing, skill, source).into());
        }

        let direct = mount_scope.and_then(|scope| {
            most_restrictive(scope.direct_occupants(&key), source)
                .map(|(existing, occupancy)| (scope, existing, occupancy))
        });
        if let Some((scope, existing, other)) =
            direct.filter(|(_, _, occupancy)| *occupancy != Occupancy::SameSource)
        {
            if other.is_skippable() && policy == ConflictPolicy::Skip {
                preserved.push(preserve(scope, existing, source));
                continue;
            }
            return Err(conflict(scope, existing, skill, source).into());
        }

        let reusable = direct
            .or(visible)
            .filter(|(_, _, occupancy)| *occupancy == Occupancy::SameSource);
        if let Some((_scope, existing, _)) = reusable {
            // The child already sees this exact source. A second mount would add another entry
            // whose duplicate-name behavior SkillMount does not control.
            actions.push(
                MountAction::ReuseExistingLink {
                    source: source.clone(),
                    destination: existing.entry.clone(),
                },
                PathPrecondition::ExistingLinkToSource,
            );
            continue;
        }

        actions.push(
            MountAction::CreateDirectoryLink {
                source: source.clone(),
                destination,
                mode: context.options.link_mode,
            },
            PathPrecondition::Missing,
        );
    }

    Ok(())
}

/// Classifies an existing entry against the canonical source that wants its name.
fn occupancy(existing: &ExistingSkill, source: &std::path::Path) -> Occupancy {
    match existing.kind {
        PathKind::DirectoryLink if existing.source_canonical.as_deref() == Some(source) => {
            Occupancy::SameSource
        }
        PathKind::DirectoryLink => Occupancy::DifferentSource,
        PathKind::Directory => Occupancy::ProjectDirectory,
        _ => Occupancy::Unsupported,
    }
}

/// Finds the most restrictive visible occupant of `key` across every discovery scope.
///
/// A different or unknown source anywhere the child can see outranks a matching one: the agent
/// picks between duplicates by rules `SkillMount` does not control, so the presence of any foreign
/// Skill under this name has to drive the decision.
fn visible_occupant<'a>(
    discovery: &'a DiscoverySnapshot,
    key: &crate::domain::SkillNameKey,
    source: &std::path::Path,
) -> Option<(&'a DiscoveryScope, &'a ExistingSkill, Occupancy)> {
    let mut matching = None;
    for scope in &discovery.scopes {
        if let Some((existing, occupancy)) = most_restrictive(scope.occupants(key), source) {
            if occupancy == Occupancy::SameSource {
                matching.get_or_insert((scope, existing, occupancy));
            } else {
                return Some((scope, existing, occupancy));
            }
        }
    }
    matching
}

fn most_restrictive<'a>(
    occupants: &'a [ExistingSkill],
    source: &std::path::Path,
) -> Option<(&'a ExistingSkill, Occupancy)> {
    let mut matching = None;
    let mut skippable = None;
    for existing in occupants {
        let occupancy = occupancy(existing, source);
        match occupancy {
            Occupancy::Unsupported => return Some((existing, occupancy)),
            Occupancy::SameSource => {
                matching.get_or_insert((existing, occupancy));
            }
            Occupancy::DifferentSource | Occupancy::ProjectDirectory => {
                skippable.get_or_insert((existing, occupancy));
            }
        }
    }
    skippable.or(matching)
}

fn preserve(
    scope: &DiscoveryScope,
    existing: &ExistingSkill,
    source: &std::path::Path,
) -> PreservedSkill {
    PreservedSkill {
        comparison_key: existing.comparison_key.clone(),
        existing: existing.entry.clone(),
        existing_kind: existing.kind,
        scope: scope.kind,
        omitted_source: source.to_path_buf(),
    }
}

fn conflict(
    scope: &DiscoveryScope,
    existing: &ExistingSkill,
    skill: &crate::domain::Skill,
    source: &std::path::Path,
) -> PlanError {
    PlanError::DestinationConflict {
        name: OsString::from(skill.mount_name.as_str()),
        scope: scope.kind.label(),
        existing: existing.entry.clone(),
        existing_state: existing.kind.label(),
        selected: source.to_path_buf(),
    }
}
