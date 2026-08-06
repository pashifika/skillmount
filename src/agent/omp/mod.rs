//! OMP adapter: a new foreground session mounting into the launch CWD's `.omp/skills`.
//!
//! The discovery, settings, plugin, and argument contracts reproduce the tagged OMP 17.2.9 source
//! recorded in [ADR 0034](../../../docs/adr/0034-pin-the-omp-session-discovery-contract.md). This
//! module observes and describes only; the shared application and transaction layers own ordering,
//! mutation, child lifetime, and cleanup.

mod args;
mod discovery;
mod plugins;
mod settings;

use std::ffi::OsString;
use std::path::{Path, PathBuf};

use crate::agent::version::VersionSpec;
use crate::agent::{AgentAdapter, DiscoverySnapshot, discovery_indexes};
use crate::diagnostic::Diagnostic;
use crate::domain::{AgentId, CatalogPolicy, RunContext, SkillCatalog};
use crate::error::{AppError, CatalogError};
use crate::mount::plan::apply_conflict_policy;
use crate::mount::{
    ActionSequence, DiscoveryPlan, LaunchPlan, MountAction, MountPlan, PathPrecondition,
};

/// OMP banner attached to the adapter's last-tested discovery evidence.
const LAST_TESTED_OMP_BANNER: &str = "omp/17.2.9";
const OMP_VERSION_SPEC: VersionSpec =
    VersionSpec::new("OMP", LAST_TESTED_OMP_BANNER, "SKILLMOUNT_TEST_OMP_VERSION");

/// Environment variables that relocate every OMP root or inject a settings overlay.
const REJECTED_ENVIRONMENT: [(&str, &str); 3] = [
    (
        "OMP_PROFILE",
        "selects a named profile, which relocates every OMP configuration, session, and plugin root \
         away from the ones SkillMount inspected",
    ),
    (
        "PI_PROFILE",
        "is OMP's legacy profile selector and relocates every OMP root away from the ones \
         SkillMount inspected",
    ),
    (
        "PI_CONFIG_FILES",
        "injects extra settings overlays that can hide a selected Skill after SkillMount has fixed \
         its discovery contract",
    ),
];

/// Read-only OMP adapter.
#[derive(Debug, Clone, Copy, Default)]
pub struct OmpAdapter;

/// Returns the dated version evidence used by the shared advisory observer.
const fn version_spec() -> VersionSpec {
    OMP_VERSION_SPEC
}

/// Verifies the OMP launch invariants that remain mandatory for every observed release.
fn verify_launch_invariants(context: &RunContext) -> Result<(), AppError> {
    verify_environment()?;
    let passthrough = args::validate(&context.passthrough_args)?;
    verify_home_escape(context, passthrough.allows_home)?;
    verify_settled_configuration(context)
}

/// Rejects every environment overlay that would move the inspected namespace.
fn verify_environment() -> Result<(), AppError> {
    for (variable, reason) in REJECTED_ENVIRONMENT {
        if std::env::var_os(variable).is_some_and(|value| !value.is_empty()) {
            return Err(AppError::Usage(format!(
                "{variable} {reason}; unset it or run the agent directly"
            )));
        }
    }
    Ok(())
}

/// Requires the operator's own `--allow-home` when the launch CWD is the user home.
///
/// Without it OMP changes directory into a temporary directory before it loads any Skill, so the
/// plan would describe a namespace the child never reads.
fn verify_home_escape(context: &RunContext, allows_home: bool) -> Result<(), AppError> {
    let omp = context.agent.omp()?;
    if allows_home || context.launch_cwd != omp.user_home {
        return Ok(());
    }
    Err(AppError::Usage(format!(
        "OMP starts in {} only with its own --allow-home option; otherwise it moves to a temporary \
         directory before loading Skills, so the mounted namespace would not be the one it reads. \
         Pass --allow-home through to OMP, or start the session in a project directory",
        omp.user_home.display()
    )))
}

/// Refuses to plan against OMP global state whose effective settings are not yet in a YAML file.
///
/// OMP migrates a legacy `settings.json` and the `settings` table of `agent.db` into `config.yml`
/// on its next persisting start. Until then the values it will use are visible in no YAML file, so
/// reading an empty configuration would silently model the wrong namespace. The database is never
/// opened.
fn verify_settled_configuration(context: &RunContext) -> Result<(), AppError> {
    let omp = context.agent.omp()?;
    if ["config.yml", "config.yaml"]
        .iter()
        .any(|name| omp.agent_dir.join(name).is_file())
    {
        return Ok(());
    }
    for legacy in ["settings.json", "agent.db"] {
        let path = omp.agent_dir.join(legacy);
        if path.exists() {
            return Err(AppError::Usage(format!(
                "OMP has not yet migrated {} into config.yml, so the Skill settings it will use are \
                 not visible in any configuration file and SkillMount cannot prove the selected \
                 Skills stay visible; run omp once directly to settle that state, then retry",
                path.display()
            )));
        }
    }
    Ok(())
}

impl AgentAdapter for OmpAdapter {
    fn version_spec(&self) -> VersionSpec {
        version_spec()
    }

    fn catalog_policy(&self) -> CatalogPolicy {
        // OMP's own native provider requires a readable non-empty description and would otherwise
        // silently drop the selected Skill, so that requirement holds even when generic metadata
        // validation is disabled. OMP accepts a frontmatter name that differs from the directory
        // name and indexes by the frontmatter name; requiring agreement is stricter on purpose,
        // because otherwise a mount would load under a name SkillMount never planned.
        CatalogPolicy {
            requires_exact_skill_md_entry: false,
            always_parses_metadata: true,
            requires_name: false,
            requires_description: true,
            requires_matching_name: true,
        }
    }

    fn destination_stores(&self, context: &RunContext) -> Vec<PathBuf> {
        vec![context.launch_cwd.join(discovery::DESTINATION_SUFFIX)]
    }

    fn validate_passthrough_args(&self, args: &[OsString]) -> Result<Vec<Diagnostic>, AppError> {
        args::validate(args)?;
        Ok(Vec::new())
    }

    fn validate_launch_invariants(&self, context: &RunContext) -> Result<(), AppError> {
        verify_launch_invariants(context)
    }

    fn inspect_discovery(&self, context: &RunContext) -> Result<DiscoverySnapshot, AppError> {
        // A profile selector or settings overlay moves every root, so even a read-only inspection
        // would describe a namespace OMP never reads. Refusing here keeps `inspect` and `--dry-run`
        // honest instead of printing a plan for the wrong roots.
        verify_environment()?;
        let inspection = discovery::inspect(context)?;
        let (visible_skills, mount_entries) =
            discovery_indexes(&inspection.scopes, &inspection.destination);
        Ok(DiscoverySnapshot {
            agent: AgentId::Omp,
            scopes: inspection.scopes,
            visible_skills,
            mount_entries,
            discovery_entry: inspection.destination.clone(),
            backing_store: inspection.destination,
            backing_store_state: inspection.destination_state.kind,
            lock_resources: inspection.lock_resources,
            warnings: inspection.warnings,
        })
    }

    fn build_mount_plan(
        &self,
        context: &RunContext,
        catalog: &SkillCatalog,
        discovery: &DiscoverySnapshot,
    ) -> Result<MountPlan, AppError> {
        let inspection = discovery::inspect(context)?;
        verify_selected_visibility(catalog, &inspection.settings, context)?;
        let mut actions = ActionSequence::default();
        // Dependency order: the `.omp` scope, then its `skills` directory, then Skills.
        for directory in &inspection.missing_directories {
            actions.push(
                MountAction::CreateDirectory {
                    path: directory.clone(),
                },
                PathPrecondition::Missing,
            );
        }
        let mut preserved = Vec::new();
        apply_conflict_policy(context, catalog, discovery, &mut actions, &mut preserved)?;

        Ok(MountPlan {
            agent: AgentId::Omp,
            discovery: DiscoveryPlan {
                entry: discovery.discovery_entry.clone(),
                backing_store: discovery.backing_store.clone(),
            },
            actions: actions.into_actions(),
            preserved,
            launch: LaunchPlan {
                // OMP receives no injected argument at all: every root, profile, config, and Skill
                // control is either rejected or left to the operator, and the mount is discovered
                // through the launch CWD's own highest-priority provider scope.
                executable: context.executable().to_path_buf(),
                cwd: context.launch_cwd.clone(),
                injected_args: Vec::new(),
                passthrough_args: context.passthrough_args.clone(),
                environment_overrides: Vec::new(),
            },
        })
    }

    fn validate_spawn_boundary(
        &self,
        context: &RunContext,
        _catalog: &SkillCatalog,
        discovery: &DiscoverySnapshot,
        plan: &MountPlan,
    ) -> Result<(), AppError> {
        verify_launch_invariants(context)?;
        verify_non_owned_evidence(context, discovery, plan)
    }
}

/// Rechecks the non-owned part of the inspected namespace immediately before spawn.
///
/// The comparison deliberately ignores exactly the transaction-owned destination entries this run
/// just created: those are the only difference the plan authorized. Anything else that moved — a
/// settings layer, a provider root, an enabled package — means the child would load a namespace
/// `SkillMount` never planned, so no child is spawned and the active transaction is released through
/// the normal evidence-checked cleanup path.
fn verify_non_owned_evidence(
    context: &RunContext,
    discovery: &DiscoverySnapshot,
    plan: &MountPlan,
) -> Result<(), AppError> {
    let owned = owned_destination_entries(plan);
    let before = non_owned_fingerprint(discovery, &owned, &discovery.backing_store);
    let rebuilt = discovery::inspect(context)?;
    let (visible, _) = discovery_indexes(&rebuilt.scopes, &rebuilt.destination);
    let after = non_owned_fingerprint_from_parts(&visible, &rebuilt, &owned);

    if before == after {
        return Ok(());
    }
    Err(AppError::Temporary(
        "the OMP settings, provider, or extension-package state that decided this plan changed \
         after it was applied, so the child would load a namespace SkillMount did not inspect; \
         nothing was launched and the transaction was released"
            .to_owned(),
    ))
}

/// Refuses a plan whose selected Skills the operator's own OMP configuration would hide.
///
/// `SkillMount` never weakens that configuration to make a mount work. A hidden winner means the
/// mount would be applied and then ignored, which is exactly the silent success this check exists
/// to prevent.
fn verify_selected_visibility(
    catalog: &SkillCatalog,
    settings: &settings::SkillSettings,
    context: &RunContext,
) -> Result<(), AppError> {
    let destination = context.launch_cwd.join(discovery::DESTINATION_SUFFIX);
    let reject = |reason: String| -> AppError {
        AppError::Catalog(CatalogError::InvalidSelectedSkill {
            path: destination.clone(),
            reason,
        })
    };
    if !settings.enabled {
        return Err(reject(
            "OMP setting skills.enabled is false, so a mounted Skill would never be loaded; enable \
             Skill discovery in OMP or run the agent directly"
                .to_owned(),
        ));
    }
    // The destination is the project level of OMP's own `native` provider.
    if !settings.source_enabled("native", true) {
        return Err(reject(
            "OMP setting skills.enablePiProject is false, so the project scope this session mounts \
             into is not read; enable it in OMP or run the agent directly"
                .to_owned(),
        ));
    }
    for resolution in &catalog.resolutions {
        let name = resolution.selected.mount_name.as_str();
        if !settings.name_visible(name) {
            return Err(reject(format!(
                "OMP configuration hides selected Skill {name} through disabledExtensions, \
                 skills.ignoredSkills, or skills.includeSkills, so mounting it would have no \
                 effect; adjust that configuration in OMP or deselect the Skill"
            )));
        }
    }
    Ok(())
}

/// Returns the destination entry paths this plan owns.
fn owned_destination_entries(plan: &MountPlan) -> Vec<PathBuf> {
    plan.actions
        .iter()
        .filter_map(|action| match &action.operation {
            MountAction::CreateDirectoryLink { destination, .. } => Some(destination.clone()),
            MountAction::CreateDirectory { path } => Some(path.clone()),
            MountAction::ReuseExistingLink { .. } => None,
        })
        .collect()
}

/// Builds a bounded, deterministic description of every non-owned observation.
fn non_owned_fingerprint(
    discovery: &DiscoverySnapshot,
    owned: &[PathBuf],
    destination: &Path,
) -> Vec<String> {
    let mut lines = Vec::new();
    for (key, occupants) in &discovery.visible_skills {
        for occupant in occupants {
            if owned.contains(&occupant.skill.entry) {
                continue;
            }
            lines.push(format!(
                "{}\u{1f}{}\u{1f}{}\u{1f}{}",
                key,
                occupant.scope.label(),
                occupant.skill.entry.display(),
                occupant
                    .skill
                    .source_canonical
                    .as_ref()
                    .map_or_else(String::new, |path| path.display().to_string())
            ));
        }
    }
    lines.push(format!("destination\u{1f}{}", destination.display()));
    lines.sort();
    lines
}

fn non_owned_fingerprint_from_parts(
    visible: &std::collections::BTreeMap<
        crate::domain::SkillNameKey,
        Vec<crate::agent::VisibleSkill>,
    >,
    rebuilt: &discovery::Inspection,
    owned: &[PathBuf],
) -> Vec<String> {
    let mut lines = Vec::new();
    for (key, occupants) in visible {
        for occupant in occupants {
            if owned.contains(&occupant.skill.entry) {
                continue;
            }
            lines.push(format!(
                "{}\u{1f}{}\u{1f}{}\u{1f}{}",
                key,
                occupant.scope.label(),
                occupant.skill.entry.display(),
                occupant
                    .skill
                    .source_canonical
                    .as_ref()
                    .map_or_else(String::new, |path| path.display().to_string())
            ));
        }
    }
    lines.push(format!(
        "destination\u{1f}{}",
        rebuilt.destination.display()
    ));
    lines.sort();
    lines
}

#[cfg(test)]
mod tests {
    use super::{LAST_TESTED_OMP_BANNER, OmpAdapter, version_spec};
    use crate::agent::AgentAdapter;
    use crate::domain::AgentId;

    #[test]
    fn version_spec_names_the_last_tested_omp_evidence() {
        assert_eq!(version_spec().last_tested_banner(), LAST_TESTED_OMP_BANNER);
        assert_eq!(
            OmpAdapter.version_spec().last_tested_banner(),
            "omp/17.2.9",
            "the recorded banner is the one the pinned binary reports"
        );
    }

    #[test]
    fn the_catalog_policy_keeps_omps_own_description_requirement() {
        let policy = OmpAdapter.catalog_policy();
        assert!(
            policy.always_parses_metadata && policy.requires_description,
            "OMP's native provider drops a Skill without a description"
        );
        assert!(
            !policy.requires_exact_skill_md_entry && !policy.requires_name,
            "OMP follows a linked entry and falls back to the directory name"
        );
        assert!(policy.requires_matching_name);
    }

    #[test]
    fn omp_is_registered_and_mounts_into_the_project_scope() {
        let descriptor = AgentId::Omp.descriptor();
        assert_eq!(descriptor.label(), "omp");
        assert_eq!(descriptor.executable_name(), "omp");
        assert_eq!(
            descriptor.default_mount_mode(),
            crate::domain::MountMode::Project
        );
        assert!(
            !descriptor.supports_explicit_mount_mode(crate::domain::MountMode::Staging),
            "OMP has no isolated staging namespace"
        );
    }
}
