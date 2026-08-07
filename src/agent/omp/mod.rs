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

use std::collections::BTreeMap;
use std::ffi::OsString;
use std::fs;
use std::path::{Path, PathBuf};

use crate::agent::version::VersionSpec;
use crate::agent::{AgentAdapter, DiscoverySnapshot, VisibleSkill, discovery_indexes};
use crate::diagnostic::Diagnostic;
use crate::domain::{AgentId, CatalogPolicy, RunContext, SkillCatalog, SkillNameKey};
use crate::error::{AppError, CatalogError};
use crate::link::resolve::ComparablePath;
use crate::mount::plan::apply_conflict_policy;
use crate::mount::{
    ActionSequence, DiscoveryPlan, LaunchPlan, MountAction, MountPlan, PathPrecondition,
};

/// OMP banner attached to the adapter's last-tested discovery evidence.
const LAST_TESTED_OMP_BANNER: &str = "omp/17.2.9";
const OMP_VERSION_SPEC: VersionSpec =
    VersionSpec::new(LAST_TESTED_OMP_BANNER, "SKILLMOUNT_TEST_OMP_VERSION");

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
///
/// Both operands are normalized the way OMP normalizes them. `startup-cwd.ts:16-20` compares
/// `normalizePathForComparison` of both sides, which is `path.resolve` then `fs.realpathSync` then
/// a lowercase fold on Windows (`dirs.ts:149-161`). `launch_cwd` is already canonical, but
/// `user_home` is the raw environment value, so comparing them directly would miss the guard
/// entirely on Windows - where canonicalization yields the verbatim `\\?\C:\...` prefix that never
/// equals `C:\...` - and on any platform where the home directory is reached through a symlink.
/// A match on either the raw or the resolved form fails closed, because OMP's escape only has to
/// agree once.
fn verify_home_escape(context: &RunContext, allows_home: bool) -> Result<(), AppError> {
    let omp = context.agent.omp()?;
    if allows_home || !names_launch_cwd(&context.launch_cwd, &omp.user_home) {
        return Ok(());
    }
    Err(AppError::Usage(format!(
        "OMP starts in {} only with its own --allow-home option; otherwise it moves to a temporary \
         directory before loading Skills, so the mounted namespace would not be the one it reads. \
         Pass --allow-home through to OMP, or start the session in a project directory",
        omp.user_home.display()
    )))
}

/// Returns whether `home` names the launch CWD, under OMP's own comparison rules.
///
/// `ComparablePath` folds the Windows namespace prefix, the case difference OMP's lowercase fold
/// absorbs, and lexical `.`/`..` components. The extra `canonicalize` pass adds the `realpathSync`
/// step, which is what makes a symlinked home directory compare equal. Canonicalization failure is
/// not fatal: the raw comparison already ran, and a home directory that cannot be resolved cannot
/// be the canonical launch CWD either.
fn names_launch_cwd(launch_cwd: &Path, home: &Path) -> bool {
    let launch = ComparablePath::new(launch_cwd);
    if launch.names_same_path(&ComparablePath::new(home)) {
        return true;
    }
    fs::canonicalize(home)
        .is_ok_and(|resolved| launch.names_same_path(&ComparablePath::new(&resolved)))
}

/// Refuses to plan against OMP global state whose effective settings are not yet in a YAML file.
///
/// OMP migrates a legacy `settings.json` into `config.yml` on its next persisting start. Until
/// then the values it will use are visible in no YAML file, so reading an empty configuration
/// would silently model the wrong namespace.
///
/// `agent.db` is deliberately *not* a trigger. Its mere existence is not evidence of unmigrated
/// settings: `AgentStorage.getSettings` returns null for an empty `settings` table
/// (`session/agent-storage.ts:415-418`), so `#migrateFromLegacy` leaves `migrated` false and
/// `config.yml` is never written (`config/settings.ts:1323`). Every OMP start creates `agent.db`
/// for sessions and usage, and 17.2.9 has no live write path into that table - the only two
/// `INSERT INTO settings` sites are schema migrations moving pre-existing rows. Treating the file
/// as a trigger therefore refused every install that had simply never customized a global setting,
/// and the refusal was permanent because no OMP run could clear it. The database is never opened.
fn verify_settled_configuration(context: &RunContext) -> Result<(), AppError> {
    let omp = context.agent.omp()?;
    if ["config.yml", "config.yaml"]
        .iter()
        .any(|name| omp.agent_dir.join(name).is_file())
    {
        return Ok(());
    }
    let legacy = omp.agent_dir.join("settings.json");
    if legacy.exists() {
        return Err(AppError::Usage(format!(
            "OMP has not yet migrated {} into config.yml, so the Skill settings it will use are \
             not visible in any configuration file and SkillMount cannot prove the selected \
             Skills stay visible; run omp once directly to settle that state, then retry",
            legacy.display()
        )));
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
            backing_store_canonical: crate::agent::canonical_backing(
                &inspection.destination,
                &inspection.destination_state,
            ),
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
        // The plan is derived from `discovery` alone. That snapshot is the one observation whose
        // `lock_resources` the stabilization loop verified are held, so re-inspecting the namespace
        // here and planning from the result could mount into a root this run holds no lock on, and
        // could take conflict decisions from a different filesystem observation than the directory
        // chain. Only the settings are re-read, because the visibility gate needs them and every
        // settings input is already inside the held lock set.
        verify_selected_visibility(catalog, &discovery::load_settings(context)?, context)?;
        let mut actions = ActionSequence::default();
        // Dependency order: the `.omp` scope, then its `skills` directory, then Skills.
        for directory in
            discovery::missing_destination_chain(&context.launch_cwd, discovery.backing_store_state)
        {
            actions.push(
                MountAction::CreateDirectory { path: directory },
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
        catalog: &SkillCatalog,
        discovery: &DiscoverySnapshot,
        plan: &MountPlan,
    ) -> Result<(), AppError> {
        verify_launch_invariants(context)?;
        // Re-read the settings rather than trust the pre-apply read. A configuration edit that
        // hides a selected Skill leaves the non-owned namespace byte-identical, so the evidence
        // comparison below cannot see it, and launching would produce exactly the silent success
        // `verify_selected_visibility` exists to prevent.
        //
        // At plan time the same refusal is a data error: the operator's own configuration hides the
        // Skill and a retry cannot help. Here it means an external writer moved the configuration
        // after the plan was applied, which is transient — the same category its two sibling
        // rechecks already report, so a caller sees one consistent status for "the ground moved
        // before spawn".
        verify_selected_visibility(catalog, &discovery::load_settings(context)?, context)
            .map_err(|error| spawn_boundary_drift(&error))?;
        verify_owned_entries_resolve(plan)?;
        verify_non_owned_evidence(context, discovery, plan)
    }
}

/// Re-reports a plan-time refusal as the transient drift it is at the child boundary.
fn spawn_boundary_drift(error: &AppError) -> AppError {
    AppError::Temporary(format!(
        "the OMP configuration that decided this plan changed after it was applied, so the child \
         would load a namespace SkillMount did not inspect; nothing was launched and the \
         transaction was released: {error}"
    ))
}

/// Requires every mount link this run created to still resolve to the source the plan recorded.
///
/// The non-owned fingerprint cannot cover this. Its owned-entry filter matches on the visible
/// destination path, so an entry planted at a planned path is excluded from the comparison exactly
/// as the genuine mount would be - which is what a retargeted `.omp/skills` produces. Comparing the
/// resolved terminal against the planned source instead is decisive: after a retarget the
/// destination resolves into a tree the plan never named, and no child is spawned.
fn verify_owned_entries_resolve(plan: &MountPlan) -> Result<(), AppError> {
    for action in &plan.actions {
        let MountAction::CreateDirectoryLink {
            source,
            destination,
            ..
        } = &action.operation
        else {
            continue;
        };
        let resolved = crate::mount::resolve::classify(destination)?;
        let terminal = resolved.terminal.as_deref();
        if terminal != Some(source.as_path()) {
            return Err(AppError::Temporary(format!(
                "the OMP mount at {} no longer resolves to the Skill this run linked, so the child \
                 would load a Skill SkillMount did not select; nothing was launched and the \
                 transaction was released",
                destination.display()
            )));
        }
    }
    Ok(())
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
    let before = non_owned_evidence(&discovery.visible_skills, &owned);
    let rebuilt = discovery::inspect(context)?;
    let (visible, _) = discovery_indexes(&rebuilt.scopes, &rebuilt.destination);
    let after = non_owned_evidence(&visible, &owned);

    if before == after && discovery.backing_store == rebuilt.destination {
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
    // `disabledProviders` drops the provider from OMP's capability registry before any root is
    // scanned, so the destination becomes unreadable while every per-level toggle still reads as
    // enabled. Naming it explicitly keeps the diagnostic honest about which setting is at fault.
    if settings.provider_disabled("native") {
        return Err(reject(
            "OMP setting disabledProviders lists the native provider, which serves the project \
             scope this session mounts into, so a mounted Skill would never be loaded; remove it \
             in OMP or run the agent directly"
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
        // The mount links the source directory in, so the child reads the source's own frontmatter.
        if discovery::selected_is_disabled(&resolution.selected.skill_md)? {
            return Err(reject(format!(
                "selected Skill {name} sets enabled: false in its own SKILL.md, so OMP would load \
                 nothing under that name and mounting it would have no effect; remove that key or \
                 deselect the Skill"
            )));
        }
    }
    Ok(())
}

/// Returns the destination entry paths this plan owns.
///
/// Only link destinations qualify. The plan's `CreateDirectory` actions are the `.omp` and
/// `.omp/skills` containers, never Skill entries — but `claude-plugins` roots are scanned with
/// `includes_self`, so a registry naming the destination directory can turn it into a visible
/// entry. Excluding a container from the recheck would hide exactly that entry.
fn owned_destination_entries(plan: &MountPlan) -> Vec<PathBuf> {
    plan.actions
        .iter()
        .filter_map(|action| match &action.operation {
            MountAction::CreateDirectoryLink { destination, .. } => Some(destination.clone()),
            MountAction::CreateDirectory { .. } | MountAction::ReuseExistingLink { .. } => None,
        })
        .collect()
}

/// Returns every non-owned observation, keyed and ordered for an exact structural comparison.
///
/// The values are compared as [`OsStr`]-backed structures rather than rendered into one delimited
/// string. `Path::display` substitutes U+FFFD for every non-Unicode byte, so two different
/// symlink targets could render identically, and U+001F is itself a legal byte in a filename and
/// in a frontmatter name, so no delimiter could separate the fields safely either. Both defects
/// made the recheck return "unchanged" for a namespace an external writer had moved.
fn non_owned_evidence(
    visible: &BTreeMap<SkillNameKey, Vec<VisibleSkill>>,
    owned: &[PathBuf],
) -> BTreeMap<SkillNameKey, Vec<VisibleSkill>> {
    visible
        .iter()
        .filter_map(|(key, occupants)| {
            let retained = occupants
                .iter()
                .filter(|occupant| !owned.contains(&occupant.skill.entry))
                .cloned()
                .collect::<Vec<_>>();
            (!retained.is_empty()).then(|| (key.clone(), retained))
        })
        .collect()
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

    #[test]
    fn non_owned_evidence_separates_paths_a_lossy_render_would_merge() {
        use crate::agent::{ExistingSkill, ScopeKind, VisibleSkill};
        use crate::domain::SkillNameKey;
        use crate::mount::resolve::PathKind;
        use std::collections::BTreeMap;
        use std::path::PathBuf;

        // Two distinct byte sequences that `Path::display` renders identically: every invalid
        // UTF-8 byte becomes U+FFFD, so a delimited `Display` string could not tell a retargeted
        // symlink from the original.
        let target = |byte: u8| -> PathBuf {
            #[cfg(unix)]
            {
                use std::os::unix::ffi::OsStringExt;
                PathBuf::from(std::ffi::OsString::from_vec(
                    [b"/tmp/z/".as_slice(), &[byte], b"/s".as_slice()].concat(),
                ))
            }
            #[cfg(not(unix))]
            {
                PathBuf::from(format!("C:/tmp/z/{byte}/s"))
            }
        };
        #[cfg(unix)]
        assert_eq!(
            target(0x80).display().to_string(),
            target(0x81).display().to_string(),
            "the two targets must be indistinguishable once rendered"
        );

        let visible = |canonical: PathBuf| {
            let key = SkillNameKey::new(std::ffi::OsStr::new("tool"));
            let mut map = BTreeMap::new();
            map.insert(
                key.clone(),
                vec![VisibleSkill {
                    scope: ScopeKind::OmpCompatibility,
                    skill: ExistingSkill {
                        comparison_key: key,
                        raw_name: "tool".into(),
                        entry: PathBuf::from("/p/.claude/skills/tool"),
                        kind: PathKind::DirectoryLink,
                        source_canonical: Some(canonical),
                    },
                }],
            );
            map
        };

        let before = super::non_owned_evidence(&visible(target(0x80)), &[]);
        let after = super::non_owned_evidence(&visible(target(0x81)), &[]);
        assert_ne!(
            before, after,
            "a retargeted non-owned entry must be observable before spawn"
        );
        assert_eq!(
            before,
            super::non_owned_evidence(&visible(target(0x80)), &[])
        );

        // An owned destination entry is the one authorized difference and drops out entirely.
        let owned = [PathBuf::from("/p/.claude/skills/tool")];
        assert!(
            super::non_owned_evidence(&visible(target(0x80)), &owned).is_empty(),
            "an owned entry must not contribute evidence"
        );
    }
}
