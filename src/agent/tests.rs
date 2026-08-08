use std::ffi::OsString;
use std::path::{Path, PathBuf};

use super::claude::ClaudeAdapter;
use super::codex::{CodexAdapter, resolve_destination};
use super::omp::OmpAdapter;
use super::{AgentAdapter, DiscoverySnapshot, ScopeKind, inspect_scope};
use crate::catalog::{CatalogRequest, resolve_catalog};
use crate::diagnostic::DiagnosticKind;
use crate::domain::{
    AgentId, ConflictPolicy, LinkMode, MountMode, RunContext, RunOptions, SkillCatalog,
    SkillNameKey, SourceOccurrence, ValidationLevel,
};
use crate::error::ExitCategory;
use crate::mount::resolve::{PathKind, classify};
use crate::mount::{MountAction, MountPlan};
use crate::test_support::{
    TestDir, assert_no_side_effects, remove_directory_link, resolved_agent, symlink_dir_or_skip,
    symlink_file_or_skip,
};

const PREFERRED: &str = ".agents/skills";
const LEGACY: &str = ".codex/skills";

/// A project fixture whose paths are all canonical, so anchors and scopes compare correctly.
struct Project {
    _dir: TestDir,
    root: PathBuf,
    sources: PathBuf,
}

impl Project {
    fn new(label: &str) -> Self {
        let dir = TestDir::new(label);
        let root = std::fs::canonicalize(dir.path()).expect("canonical fixture root");
        let sources = root.join("sources");
        std::fs::create_dir_all(&sources).expect("source directory");
        Self {
            _dir: dir,
            root,
            sources,
        }
    }

    fn preferred(&self) -> PathBuf {
        self.root.join(PREFERRED)
    }

    fn legacy(&self) -> PathBuf {
        self.root.join(LEGACY)
    }

    fn make_dir(&self, relative: &str) -> PathBuf {
        let path = self.root.join(relative);
        std::fs::create_dir_all(&path).expect("fixture directory");
        path
    }

    /// Creates a Skill under `sources/` that passes basic validation.
    fn source_skill(&self, name: &str) -> PathBuf {
        let path = self.sources.join(name);
        std::fs::create_dir_all(&path).expect("skill directory");
        std::fs::write(
            path.join("SKILL.md"),
            format!("---\nname: {name}\ndescription: {name} description\n---\n"),
        )
        .expect("SKILL.md");
        path
    }

    fn context(
        &self,
        agent: AgentId,
        mount_mode: MountMode,
        conflict: ConflictPolicy,
    ) -> RunContext {
        RunContext {
            agent: resolved_agent(agent, &self.root),
            invocation_cwd: self.root.clone(),
            launch_cwd: self.root.clone(),
            project_root: self.root.clone(),
            skill_sources: Vec::new(),
            session_id: None,
            passthrough_args: Vec::new(),
            options: RunOptions {
                link_mode: LinkMode::Auto,
                mount_mode,
                conflict,
                validation: ValidationLevel::Basic,
                dry_run: true,
                keep_mounts: false,
                no_recover: false,
                verbosity: 0,
            },
        }
    }

    fn codex_context(&self, conflict: ConflictPolicy) -> RunContext {
        self.context(AgentId::Codex, MountMode::Project, conflict)
    }

    fn catalog(&self, agent: AgentId) -> SkillCatalog {
        let occurrences = vec![SourceOccurrence {
            ordinal: 0,
            input_path: self.sources.clone(),
            resolved_path: self.sources.clone(),
        }];
        resolve_catalog(
            &occurrences,
            &CatalogRequest {
                agent,
                policy: crate::agent::adapter(agent).catalog_policy(),
                validation: ValidationLevel::Basic,
                destination_stores: &[],
            },
        )
        .expect("fixture catalog resolves")
    }
}

fn plan_codex(
    project: &Project,
    context: &RunContext,
) -> Result<MountPlan, crate::error::AppError> {
    let catalog = project.catalog(AgentId::Codex);
    let snapshot = CodexAdapter.inspect_discovery(context)?;
    CodexAdapter.build_mount_plan(context, &catalog, &snapshot)
}

fn plan_claude(
    project: &Project,
    context: &RunContext,
) -> Result<MountPlan, crate::error::AppError> {
    let catalog = project.catalog(AgentId::Claude);
    let snapshot = ClaudeAdapter.inspect_discovery(context)?;
    ClaudeAdapter.build_mount_plan(context, &catalog, &snapshot)
}

fn assert_dual_discovery_keys(
    snapshot: &DiscoverySnapshot,
    path: &Path,
    access: crate::lock::LockAccess,
    legacy: &crate::lock::LockResource,
) {
    let emitted = snapshot
        .lock_resources
        .iter()
        .filter(|resource| {
            resource.kind == crate::lock::LockResourceKind::DiscoveryEntry
                && resource.access == access
                && resource.path == path
        })
        .map(|resource| resource.lock_keys()[0].clone())
        .collect::<std::collections::BTreeSet<_>>();
    let shared = crate::lock::LockResource::describe_shared(
        crate::lock::LockResourceKind::DiscoveryEntry,
        access,
        path,
    )
    .expect("shared discovery identity");

    assert!(
        emitted.contains(&shared.lock_keys()[0]),
        "the shared volume-root key is emitted for {}",
        path.display()
    );
    assert!(
        emitted.contains(&legacy.lock_keys()[0]),
        "the origin/dev/0.3.x key is emitted for {}",
        path.display()
    );
    assert_ne!(
        shared.lock_keys()[0],
        legacy.lock_keys()[0],
        "the fixture must exercise two distinct logical identities"
    );
}

#[test]
fn codex_permission_diagnostics_are_typed_and_only_cover_external_skills() {
    let project = Project::new("codex-permission-diagnostic");
    project.source_skill("inside");
    let context = project.codex_context(ConflictPolicy::Error);
    assert!(
        CodexAdapter
            .catalog_diagnostics(
                &context,
                &project.catalog(AgentId::Codex),
                &plan_codex(&project, &context).expect("permission fixture plan"),
            )
            .is_empty(),
        "project-contained Skills need no permission-separation warning"
    );

    let external = TestDir::new("codex-permission-external");
    let external_source = external.dir("source");
    let external_skill = external.dir("source/outside");
    std::fs::write(
        external_skill.join("SKILL.md"),
        "---\nname: outside\ndescription: external fixture\n---\n",
    )
    .expect("external Skill metadata");
    let catalog = resolve_catalog(
        &[SourceOccurrence {
            ordinal: 0,
            input_path: external_source.clone(),
            resolved_path: external_source,
        }],
        &CatalogRequest {
            agent: AgentId::Codex,
            policy: crate::agent::adapter(AgentId::Codex).catalog_policy(),
            validation: ValidationLevel::Basic,
            destination_stores: &[],
        },
    )
    .expect("external catalog");

    let snapshot = CodexAdapter
        .inspect_discovery(&context)
        .expect("permission snapshot");
    let plan = CodexAdapter
        .build_mount_plan(&context, &catalog, &snapshot)
        .expect("permission fixture plan");
    let diagnostics = CodexAdapter.catalog_diagnostics(&context, &catalog, &plan);
    assert_eq!(diagnostics.len(), 1);
    assert_eq!(
        diagnostics[0].kind,
        DiagnosticKind::CodexPermissionSeparation
    );
    assert_eq!(diagnostics[0].source_ordinal, Some(0));
    assert!(
        diagnostics[0]
            .message
            .contains("does not grant sandbox access")
    );
}

#[test]
fn a_skipped_external_skill_does_not_claim_that_codex_will_follow_a_new_link() {
    let project = Project::new("codex-permission-skipped");
    let existing = project.make_dir(".agents/skills/outside");
    std::fs::write(
        existing.join("SKILL.md"),
        "---\nname: outside\ndescription: project fixture\n---\n",
    )
    .expect("project Skill metadata");
    let external = TestDir::new("codex-permission-skipped-source");
    let source_root = external.dir("source");
    let source = external.dir("source/outside");
    std::fs::write(
        source.join("SKILL.md"),
        "---\nname: outside\ndescription: external fixture\n---\n",
    )
    .expect("external Skill metadata");
    let catalog = resolve_catalog(
        &[SourceOccurrence {
            ordinal: 0,
            input_path: source_root.clone(),
            resolved_path: source_root,
        }],
        &CatalogRequest {
            agent: AgentId::Codex,
            policy: crate::agent::adapter(AgentId::Codex).catalog_policy(),
            validation: ValidationLevel::Basic,
            destination_stores: &[],
        },
    )
    .expect("external catalog");
    let context = project.codex_context(ConflictPolicy::Skip);
    let snapshot = CodexAdapter
        .inspect_discovery(&context)
        .expect("permission snapshot");
    let plan = CodexAdapter
        .build_mount_plan(&context, &catalog, &snapshot)
        .expect("skip preserves the project Skill");

    assert_eq!(plan.preserved.len(), 1);
    assert!(
        CodexAdapter
            .catalog_diagnostics(&context, &catalog, &plan)
            .is_empty(),
        "no new external link is planned, so no sandbox-access warning applies"
    );
}

/// Returns the per-Skill links in a mount plan.
fn link_destinations(plan: &MountPlan) -> Vec<&Path> {
    plan.actions
        .iter()
        .filter_map(|action| match &action.operation {
            MountAction::CreateDirectoryLink { destination, .. } => Some(destination.as_path()),
            _ => None,
        })
        .collect()
}

fn reuse_destinations(plan: &MountPlan) -> Vec<&Path> {
    plan.actions
        .iter()
        .filter_map(|action| match &action.operation {
            MountAction::ReuseExistingLink { destination, .. } => Some(destination.as_path()),
            _ => None,
        })
        .collect()
}

fn created_directories(plan: &MountPlan) -> Vec<&Path> {
    plan.actions
        .iter()
        .filter_map(|action| match &action.operation {
            MountAction::CreateDirectory { path } => Some(path.as_path()),
            _ => None,
        })
        .collect()
}

// ---------------------------------------------------------------------------
// Preferred `.agents/skills` placement, independent from legacy discovery.
// ---------------------------------------------------------------------------

#[test]
fn an_existing_agents_link_is_respected_as_the_logical_destination() {
    let project = Project::new("codex-agents-link");
    let terminal = project.make_dir("shared-store");
    project.make_dir(".agents");
    if !symlink_dir_or_skip(&terminal, &project.preferred()) {
        return;
    }

    let destination = resolve_destination(&project.root, &classify(&project.preferred()).unwrap())
        .expect("an existing preferred link resolves");

    assert_eq!(destination.entry, project.preferred());
    assert_eq!(destination.entry_state, PathKind::DirectoryLink);
    assert!(destination.create_directories.is_empty());
}

#[test]
fn a_regular_agents_directory_needs_no_helper_action() {
    let project = Project::new("codex-agents-directory");
    let preferred = project.make_dir(PREFERRED);

    let destination = resolve_destination(&project.root, &classify(&preferred).unwrap()).unwrap();

    assert_eq!(destination.entry, preferred);
    assert_eq!(destination.entry_state, PathKind::Directory);
    assert!(destination.create_directories.is_empty());
}

#[test]
fn a_missing_agents_entry_creates_only_the_preferred_regular_directory_chain() {
    let project = Project::new("codex-agents-missing");
    project.make_dir(LEGACY);

    let destination =
        resolve_destination(&project.root, &classify(&project.preferred()).unwrap()).unwrap();

    assert_eq!(destination.entry, project.preferred());
    assert_eq!(
        destination.create_directories,
        [project.root.join(".agents"), project.preferred()],
        "the legacy root is visible but never selected as a destination"
    );
}

#[test]
fn an_existing_agents_parent_only_adds_its_skills_child() {
    let project = Project::new("codex-agents-parent");
    project.make_dir(".agents");

    let destination =
        resolve_destination(&project.root, &classify(&project.preferred()).unwrap()).unwrap();

    assert_eq!(destination.create_directories, [project.preferred()]);
}

#[test]
fn a_linked_agents_parent_can_receive_the_regular_skills_child() {
    let project = Project::new("codex-linked-agents-parent");
    let terminal = project.make_dir("shared-agents");
    if !symlink_dir_or_skip(&terminal, &project.root.join(".agents")) {
        return;
    }

    let destination =
        resolve_destination(&project.root, &classify(&project.preferred()).unwrap()).unwrap();

    assert_eq!(destination.create_directories, [project.preferred()]);
}

#[test]
fn an_unresolvable_preferred_entry_fails_before_any_action_exists() {
    let project = Project::new("codex-row-unresolvable");
    project.make_dir(".agents");

    // Broken link.
    if !symlink_dir_or_skip(&project.root.join("absent"), &project.preferred()) {
        return;
    }
    let error = resolve_destination(&project.root, &classify(&project.preferred()).unwrap())
        .expect_err("a broken preferred entry has no safe destination");
    assert_eq!(error.category(), ExitCategory::Filesystem);
    remove_directory_link(&project.preferred());

    // Non-directory.
    std::fs::write(project.preferred(), "not a namespace").expect("file fixture");
    let error = resolve_destination(&project.root, &classify(&project.preferred()).unwrap())
        .expect_err("a file cannot hold Skills");
    assert_eq!(error.category(), ExitCategory::Filesystem);
    std::fs::remove_file(project.preferred()).expect("reset fixture");

    // Link cycle.
    let other = project.root.join(".agents/other");
    assert!(symlink_dir_or_skip(&other, &project.preferred()));
    assert!(symlink_dir_or_skip(&project.preferred(), &other));
    let resolved = classify(&project.preferred()).unwrap();
    assert_eq!(resolved.kind, PathKind::CyclicLink);
    let error = resolve_destination(&project.root, &resolved)
        .expect_err("a cycle has no terminal directory");
    assert_eq!(error.category(), ExitCategory::Filesystem);
}

#[test]
fn a_missing_preferred_entry_beneath_a_broken_parent_fails_closed() {
    let project = Project::new("codex-broken-agents-parent");
    if !symlink_dir_or_skip(&project.root.join("absent"), &project.root.join(".agents")) {
        return;
    }

    let error = resolve_destination(&project.root, &classify(&project.preferred()).unwrap())
        .expect_err("a broken parent cannot hold the preferred regular directory");

    assert_eq!(error.category(), ExitCategory::Filesystem);
}

// ---------------------------------------------------------------------------
// Scope inspection, including the ADR-010 behaviour for non-portable names.
// ---------------------------------------------------------------------------

#[test]
fn an_entry_that_is_not_a_portable_name_still_occupies_its_logical_key() {
    let project = Project::new("scope-non-portable");
    let store = project.make_dir(LEGACY);
    std::fs::create_dir_all(store.join("My_Skill")).expect("uppercase entry");
    std::fs::create_dir_all(store.join("rust--review")).expect("double-hyphen entry");

    let scope = inspect_scope(ScopeKind::CodexProjectLegacy, &store).unwrap();

    let occupant = scope
        .occupant(&SkillNameKey::new(std::ffi::OsStr::new("my_skill")))
        .expect("a non-portable entry must still be visible to conflict detection");
    assert_eq!(occupant.raw_name, OsString::from("My_Skill"));
    assert_eq!(occupant.kind, PathKind::Directory);
    assert!(
        scope
            .occupant(&SkillNameKey::new(std::ffi::OsStr::new("rust--review")))
            .is_some()
    );
}

#[test]
fn scope_enumeration_is_independent_of_host_ordering() {
    // Two stores hold the same logical entries created in opposite order. Creation order is the
    // lever a caller has over what `read_dir` returns, so building both and comparing is what
    // actually demonstrates the result does not depend on it. Reading one store twice would only
    // show that the host is self-consistent.
    let project = Project::new("scope-deterministic");
    let forward = project.make_dir(".codex/forward");
    let reverse = project.make_dir(".codex/reverse");
    let names = ["zeta", "alpha", "Middle", "beta"];
    for name in names {
        std::fs::create_dir_all(forward.join(name)).expect("entry");
    }
    for name in names.iter().rev() {
        std::fs::create_dir_all(reverse.join(name)).expect("entry");
    }

    let first = inspect_scope(ScopeKind::CodexProjectLegacy, &forward).unwrap();
    let second = inspect_scope(ScopeKind::CodexProjectLegacy, &reverse).unwrap();

    let keys = |scope: &super::DiscoveryScope| {
        scope
            .existing_skills
            .keys()
            .map(ToString::to_string)
            .collect::<Vec<_>>()
    };
    let raw_names = |scope: &super::DiscoveryScope| {
        scope
            .existing_skills
            .values()
            .flatten()
            .map(|existing| existing.raw_name.clone())
            .collect::<Vec<_>>()
    };

    assert_eq!(keys(&first), ["alpha", "beta", "middle", "zeta"]);
    assert_eq!(keys(&first), keys(&second));
    assert_eq!(raw_names(&first), raw_names(&second));
    assert_eq!(
        inspect_scope(ScopeKind::CodexProjectLegacy, &forward).unwrap(),
        first,
        "repeating one inspection must also be stable"
    );
}

#[test]
fn a_missing_scope_reports_no_occupants_instead_of_failing() {
    let project = Project::new("scope-missing");

    let scope = inspect_scope(ScopeKind::CodexProjectAgents, &project.preferred()).unwrap();

    assert_eq!(scope.state.kind, PathKind::Missing);
    assert!(scope.existing_skills.is_empty());
}

#[test]
fn a_file_link_named_skill_md_is_not_a_visible_codex_skill() {
    let project = Project::new("codex-file-linked-metadata");
    let skill = project.make_dir(".agents/skills/linked-metadata");
    let metadata = skill.join("metadata.md");
    std::fs::write(
        &metadata,
        "---\nname: linked-metadata\ndescription: linked fixture\n---\n",
    )
    .expect("metadata fixture");
    if !symlink_file_or_skip(&metadata, &skill.join("SKILL.md")) {
        return;
    }

    let snapshot = CodexAdapter
        .inspect_discovery(&project.codex_context(ConflictPolicy::Error))
        .expect("file-linked metadata is ignored, not followed");

    assert!(
        !snapshot
            .visible_skills
            .contains_key(&SkillNameKey::new(std::ffi::OsStr::new("linked-metadata")))
    );
    assert!(snapshot.warnings.iter().any(|warning| {
        warning
            .message
            .contains("directory entry is not a regular file")
    }));
}

// ---------------------------------------------------------------------------
// Section 15.6 conflict table.
// ---------------------------------------------------------------------------

#[test]
fn a_missing_destination_plans_a_link_under_both_policies() {
    for policy in [ConflictPolicy::Error, ConflictPolicy::Skip] {
        let project = Project::new("conflict-missing");
        let source = project.source_skill("alpha");
        project.make_dir(LEGACY);

        let plan = plan_codex(&project, &project.codex_context(policy)).expect("plan builds");

        assert_eq!(
            link_destinations(&plan),
            [project.preferred().join("alpha")]
        );
        assert_eq!(
            plan.actions
                .iter()
                .filter_map(|action| match &action.operation {
                    MountAction::CreateDirectoryLink { source, .. } => Some(source.as_path()),
                    _ => None,
                })
                .collect::<Vec<_>>(),
            [source.as_path()]
        );
    }
}

#[test]
fn a_link_to_the_selected_source_is_reused_and_never_owned() {
    let project = Project::new("conflict-same-source");
    let source = project.source_skill("alpha");
    let store = project.make_dir(LEGACY);
    if !symlink_dir_or_skip(&source, &store.join("alpha")) {
        return;
    }

    let plan = plan_codex(&project, &project.codex_context(ConflictPolicy::Error))
        .expect("an existing mount of the same source is reusable");

    assert_eq!(reuse_destinations(&plan), [store.join("alpha")]);
    assert!(
        link_destinations(&plan).is_empty(),
        "an entry that already points at the selected source is not recreated"
    );
    assert!(
        plan.created_actions()
            .all(|action| match &action.operation {
                MountAction::CreateDirectory { path } => path != &store.join("alpha"),
                MountAction::CreateDirectoryLink { destination, .. } =>
                    destination != &store.join("alpha"),
                MountAction::ReuseExistingLink { .. } => false,
            }),
        "a reused entry must never be owned, so cleanup can never remove it"
    );
}

#[test]
fn a_link_to_a_different_source_fails_under_error_and_is_preserved_under_skip() {
    let project = Project::new("conflict-different-source");
    project.source_skill("alpha");
    let other = project.make_dir("other-alpha");
    let store = project.make_dir(PREFERRED);
    if !symlink_dir_or_skip(&other, &store.join("alpha")) {
        return;
    }

    let error = plan_codex(&project, &project.codex_context(ConflictPolicy::Error))
        .expect_err("a foreign link is a destination conflict");
    assert_eq!(error.category(), ExitCategory::Filesystem);

    let plan = plan_codex(&project, &project.codex_context(ConflictPolicy::Skip))
        .expect("skip preserves the existing link");
    assert!(link_destinations(&plan).is_empty());
    assert_eq!(plan.preserved.len(), 1);
    assert_eq!(plan.preserved[0].existing, store.join("alpha"));
}

#[test]
fn a_regular_project_directory_is_never_replaced() {
    let project = Project::new("conflict-project-directory");
    project.source_skill("alpha");
    let store = project.make_dir(PREFERRED);
    std::fs::create_dir_all(store.join("alpha")).expect("project-owned skill");

    let error = plan_codex(&project, &project.codex_context(ConflictPolicy::Error))
        .expect_err("a project-owned Skill is a conflict");
    assert_eq!(error.category(), ExitCategory::Filesystem);

    let plan = plan_codex(&project, &project.codex_context(ConflictPolicy::Skip))
        .expect("skip preserves the project Skill");
    assert!(link_destinations(&plan).is_empty());
    assert_eq!(plan.preserved[0].existing_kind, PathKind::Directory);
}

#[test]
fn an_unsupported_destination_fails_under_both_policies() {
    let project = Project::new("conflict-unsupported");
    project.source_skill("alpha");
    let store = project.make_dir(PREFERRED);
    if !symlink_dir_or_skip(&project.root.join("absent"), &store.join("alpha")) {
        return;
    }

    for policy in [ConflictPolicy::Error, ConflictPolicy::Skip] {
        let error = plan_codex(&project, &project.codex_context(policy)).expect_err(
            "skip cannot claim a broken entry is a usable Skill, so it fails under both policies",
        );
        assert_eq!(error.category(), ExitCategory::Filesystem);
    }
}

#[test]
fn a_case_variant_destination_is_detected_even_though_the_exact_path_is_absent() {
    let project = Project::new("conflict-case-variant");
    project.source_skill("alpha");
    let store = project.make_dir(PREFERRED);
    std::fs::create_dir_all(store.join("Alpha")).expect("case-variant entry");

    let error = plan_codex(&project, &project.codex_context(ConflictPolicy::Error))
        .expect_err("an exact path being absent is not enough when the logical key is taken");

    assert_eq!(error.category(), ExitCategory::Filesystem);
}

#[test]
fn skipping_a_winner_never_reveals_a_shadowed_source() {
    let project = Project::new("conflict-no-shadow-reveal");
    let first = project.root.join("first");
    let second = project.root.join("second");
    for (root, marker) in [(&first, "first"), (&second, "second")] {
        let path = root.join("alpha");
        std::fs::create_dir_all(&path).expect("source");
        std::fs::write(
            path.join("SKILL.md"),
            format!("---\nname: alpha\ndescription: from {marker}\n---\n"),
        )
        .expect("SKILL.md");
    }
    let store = project.make_dir(PREFERRED);
    std::fs::create_dir_all(store.join("alpha")).expect("project-owned skill");

    let occurrences = [&first, &second]
        .iter()
        .enumerate()
        .map(|(ordinal, path)| SourceOccurrence {
            ordinal,
            input_path: (*path).clone(),
            resolved_path: (*path).clone(),
        })
        .collect::<Vec<_>>();
    let catalog = resolve_catalog(
        &occurrences,
        &CatalogRequest {
            agent: AgentId::Codex,
            policy: crate::agent::adapter(AgentId::Codex).catalog_policy(),
            validation: ValidationLevel::Basic,
            destination_stores: &[],
        },
    )
    .expect("overlay resolves");
    assert_eq!(catalog.resolutions[0].shadowed.len(), 1);

    let context = project.codex_context(ConflictPolicy::Skip);
    let snapshot = CodexAdapter.inspect_discovery(&context).unwrap();
    let plan = CodexAdapter
        .build_mount_plan(&context, &catalog, &snapshot)
        .expect("skip preserves the project Skill");

    assert!(
        link_destinations(&plan).is_empty(),
        "omitting the winner must not promote the shadowed candidate"
    );
    assert_eq!(plan.preserved.len(), 1);
    assert_eq!(plan.preserved[0].omitted_source, second.join("alpha"));
}

// ---------------------------------------------------------------------------
// Section 15.7 cross-scope preflight.
// ---------------------------------------------------------------------------

#[test]
fn a_same_key_skill_in_an_ancestor_scope_blocks_a_mount_whose_destination_is_free() {
    let project = Project::new("cross-scope-ancestor");
    project.source_skill("alpha");
    project.make_dir(LEGACY);
    let nested = project.make_dir("nested");
    let ancestor_store = project.make_dir("nested/.agents/skills");
    let ancestor_skill = ancestor_store.join("alpha");
    std::fs::create_dir_all(&ancestor_skill).expect("ancestor skill");
    std::fs::write(
        ancestor_skill.join("SKILL.md"),
        "---\nname: alpha\ndescription: ancestor alpha\n---\n",
    )
    .expect("ancestor Skill metadata");

    let mut context = project.codex_context(ConflictPolicy::Error);
    context.launch_cwd = nested;

    let error = plan_codex(&project, &context)
        .expect_err("a Skill already visible to the child must not be silently duplicated");

    assert_eq!(error.category(), ExitCategory::Filesystem);
}

#[test]
fn a_same_key_skill_in_an_ancestor_codex_scope_blocks_a_mount() {
    let project = Project::new("cross-scope-ancestor-codex");
    project.source_skill("alpha");
    project.make_dir(LEGACY);
    let nested = project.make_dir("nested");
    let existing = project.make_dir("nested/.codex/skills/alpha");
    std::fs::write(
        existing.join("SKILL.md"),
        "---\nname: alpha\ndescription: legacy alpha\n---\n",
    )
    .expect("existing Skill metadata");

    let mut context = project.codex_context(ConflictPolicy::Error);
    context.launch_cwd = nested;

    let error = plan_codex(&project, &context)
        .expect_err("a legacy Skill visible to Codex must veto a duplicate mount");

    assert_eq!(error.category(), ExitCategory::Filesystem);
}

#[test]
fn a_recursive_frontmatter_name_conflicts_when_directory_names_differ() {
    let project = Project::new("cross-scope-recursive-frontmatter");
    project.source_skill("alpha");
    let existing = project.make_dir(".agents/skills/group/legacy-alpha");
    std::fs::write(
        existing.join("SKILL.md"),
        "---\nname: alpha\ndescription: nested alpha\n---\n",
    )
    .expect("existing Skill metadata");

    let error = plan_codex(&project, &project.codex_context(ConflictPolicy::Error))
        .expect_err("Codex uses recursive frontmatter names rather than direct directory names");

    assert_eq!(error.category(), ExitCategory::Filesystem);
}

#[test]
fn codex_directory_name_fallback_is_included_in_the_conflict_index() {
    let project = Project::new("cross-scope-frontmatter-name-fallback");
    project.source_skill("alpha");
    let existing = project.make_dir(".agents/skills/group/alpha");
    std::fs::write(
        existing.join("SKILL.md"),
        "---\ndescription: fallback alpha\n---\n",
    )
    .expect("existing Skill metadata");

    let error = plan_codex(&project, &project.codex_context(ConflictPolicy::Error))
        .expect_err("Codex falls back to the containing directory when name is absent");

    assert_eq!(error.category(), ExitCategory::Filesystem);
}

#[test]
fn codex_directory_name_fallback_uses_a_link_targets_directory_name() {
    let project = Project::new("cross-scope-frontmatter-linked-name-fallback");
    project.source_skill("canonical-alpha");
    let store = project.make_dir(PREFERRED);
    let existing = project.make_dir("foreign/canonical-alpha");
    std::fs::write(
        existing.join("SKILL.md"),
        "---\ndescription: linked fallback alpha\n---\n",
    )
    .expect("existing Skill metadata");
    if !symlink_dir_or_skip(&existing, &store.join("alias-alpha")) {
        return;
    }

    let error = plan_codex(&project, &project.codex_context(ConflictPolicy::Error))
        .expect_err("Codex canonicalizes the Skill before applying its directory-name fallback");

    assert_eq!(error.category(), ExitCategory::Filesystem);
}

#[test]
fn codex_scalar_repair_keeps_a_real_existing_skill_in_the_conflict_index() {
    let project = Project::new("cross-scope-frontmatter-scalar-repair");
    project.source_skill("alpha");
    let existing = project.make_dir(".agents/skills/foreign");
    std::fs::write(
        existing.join("SKILL.md"),
        "---\nname: alpha\ndescription: Build for AWS: ECS and Lambda\n---\n",
    )
    .expect("existing Skill metadata");

    let error = plan_codex(&project, &project.codex_context(ConflictPolicy::Error))
        .expect_err("the pinned Codex loader repairs this unquoted prose");

    assert_eq!(error.category(), ExitCategory::Filesystem);
}

#[test]
fn codex_existing_discovery_ignores_a_wrong_case_skill_filename() {
    let project = Project::new("cross-scope-exact-skill-filename");
    project.source_skill("alpha");
    let existing = project.make_dir(".agents/skills/foreign");
    std::fs::write(
        existing.join("skill.md"),
        "---\nname: alpha\ndescription: wrong-case filename\n---\n",
    )
    .expect("wrong-case Skill metadata");

    let plan = plan_codex(&project, &project.codex_context(ConflictPolicy::Error))
        .expect("the pinned loader does not treat skill.md as SKILL.md");

    assert_eq!(
        link_destinations(&plan),
        [project.preferred().join("alpha")]
    );
}

#[test]
fn recursive_discovery_includes_depth_six_but_does_not_descend_beyond_it() {
    let project = Project::new("recursive-discovery-depth-boundary");
    let store = project.make_dir(PREFERRED);
    let mut directory = store;
    for component in ["one", "two", "three", "four", "five", "six"] {
        directory = directory.join(component);
        std::fs::create_dir(&directory).expect("nested discovery directory");
    }
    std::fs::write(
        directory.join("SKILL.md"),
        "---\nname: at-boundary\ndescription: depth-six fixture\n---\n",
    )
    .expect("depth-six Skill metadata");
    let beyond = directory.join("seven");
    std::fs::create_dir(&beyond).expect("beyond-boundary directory");
    std::fs::write(
        beyond.join("SKILL.md"),
        "---\nname: beyond-boundary\ndescription: depth-seven fixture\n---\n",
    )
    .expect("depth-seven Skill metadata");

    let snapshot = CodexAdapter
        .inspect_discovery(&project.codex_context(ConflictPolicy::Error))
        .expect("bounded recursive discovery");

    assert!(
        snapshot
            .visible_skills
            .contains_key(&SkillNameKey::new(std::ffi::OsStr::new("at-boundary")))
    );
    assert!(
        !snapshot
            .visible_skills
            .contains_key(&SkillNameKey::new(std::ffi::OsStr::new("beyond-boundary")))
    );
}

#[test]
fn recursive_discovery_does_not_descend_into_hidden_collections() {
    let project = Project::new("recursive-discovery-hidden-collection");
    let hidden = project.make_dir(".agents/skills/.hidden/foreign");
    std::fs::write(
        hidden.join("SKILL.md"),
        "---\nname: hidden-skill\ndescription: hidden fixture\n---\n",
    )
    .expect("hidden Skill metadata");

    let snapshot = CodexAdapter
        .inspect_discovery(&project.codex_context(ConflictPolicy::Error))
        .expect("hidden collection discovery");

    assert!(
        !snapshot
            .visible_skills
            .contains_key(&SkillNameKey::new(std::ffi::OsStr::new("hidden-skill")))
    );
}

#[test]
fn a_symlinked_collection_is_discovered_once_even_when_it_links_back() {
    let project = Project::new("cross-scope-symlinked-collection");
    project.source_skill("alpha");
    let store = project.make_dir(PREFERRED);
    let collection = project.make_dir("collection/deep/foreign");
    std::fs::write(
        collection.join("SKILL.md"),
        "---\nname: alpha\ndescription: linked collection alpha\n---\n",
    )
    .expect("linked collection Skill metadata");
    if !symlink_dir_or_skip(&project.root.join("collection"), &store.join("collection")) {
        return;
    }
    if !symlink_dir_or_skip(&store, &project.root.join("collection/loop")) {
        return;
    }

    let error = plan_codex(&project, &project.codex_context(ConflictPolicy::Error))
        .expect_err("Codex follows linked collections, and terminal identity must stop the cycle");

    assert_eq!(error.category(), ExitCategory::Filesystem);
}

#[test]
fn duplicate_frontmatter_names_retain_a_foreign_conflict() {
    let project = Project::new("cross-scope-duplicate-frontmatter");
    let source = project.source_skill("alpha");
    let store = project.make_dir(PREFERRED);
    if !symlink_dir_or_skip(&source, &store.join("one")) {
        return;
    }
    let foreign = project.make_dir(".agents/skills/two");
    std::fs::write(
        foreign.join("SKILL.md"),
        "---\nname: alpha\ndescription: foreign alpha\n---\n",
    )
    .expect("foreign Skill metadata");

    let error = plan_codex(&project, &project.codex_context(ConflictPolicy::Error))
        .expect_err("one matching duplicate must not hide a foreign duplicate");

    assert_eq!(error.category(), ExitCategory::Filesystem);
}

#[test]
fn agents_and_legacy_names_are_merged_without_dropping_a_foreign_duplicate() {
    let project = Project::new("cross-scope-agents-legacy-merge");
    let source = project.source_skill("alpha");
    let agents = project.make_dir(PREFERRED);
    if !symlink_dir_or_skip(&source, &agents.join("matching")) {
        return;
    }
    let foreign = project.make_dir(".codex/skills/foreign");
    std::fs::write(
        foreign.join("SKILL.md"),
        "---\nname: alpha\ndescription: foreign legacy alpha\n---\n",
    )
    .expect("foreign legacy Skill metadata");

    let context = project.codex_context(ConflictPolicy::Error);
    let snapshot = CodexAdapter
        .inspect_discovery(&context)
        .expect("merged discovery snapshot");
    let visible = snapshot
        .visible_skills
        .get(&SkillNameKey::new(std::ffi::OsStr::new("alpha")))
        .expect("both alpha declarations remain indexed");

    assert_eq!(visible.len(), 2);
    assert!(
        visible
            .iter()
            .any(|entry| entry.scope == ScopeKind::CodexProjectAgents)
    );
    assert!(
        visible
            .iter()
            .any(|entry| entry.scope == ScopeKind::CodexProjectLegacy)
    );
    let error = CodexAdapter
        .build_mount_plan(&context, &project.catalog(AgentId::Codex), &snapshot)
        .expect_err("a matching declaration must not hide the legacy foreign duplicate");
    assert_eq!(error.category(), ExitCategory::Filesystem);
}

#[test]
fn every_global_codex_scope_participates_in_the_merged_conflict_index() {
    for (label, relative, expected_scope) in [
        (
            "user-agents",
            "home/.agents/skills/foreign",
            ScopeKind::CodexUserAgents,
        ),
        (
            "user-legacy",
            "codex-home/skills/foreign",
            ScopeKind::CodexUserLegacy,
        ),
        (
            "bundled-system",
            "codex-home/skills/.system/foreign",
            ScopeKind::CodexSystem,
        ),
        (
            "administrator",
            "admin/skills/foreign",
            ScopeKind::CodexAdmin,
        ),
    ] {
        let project = Project::new(&format!("cross-scope-global-{label}"));
        project.source_skill("alpha");
        let existing = project.make_dir(relative);
        std::fs::write(
            existing.join("SKILL.md"),
            "---\nname: alpha\ndescription: global alpha\n---\n",
        )
        .expect("global Skill metadata");
        let context = project.codex_context(ConflictPolicy::Error);
        let snapshot = CodexAdapter
            .inspect_discovery(&context)
            .expect("global discovery snapshot");
        let visible = snapshot
            .visible_skills
            .get(&SkillNameKey::new(std::ffi::OsStr::new("alpha")))
            .expect("global alpha is indexed");

        assert!(
            visible.iter().any(|entry| entry.scope == expected_scope),
            "{label} was absent from {visible:?}"
        );
        let error = CodexAdapter
            .build_mount_plan(&context, &project.catalog(AgentId::Codex), &snapshot)
            .expect_err("a global duplicate must block a new mount");
        assert_eq!(error.category(), ExitCategory::Filesystem, "{label}");
    }
}

#[test]
fn embedded_system_names_are_reserved_before_codex_installs_its_cache() {
    for name in [
        "imagegen",
        "openai-docs",
        "plugin-creator",
        "review-agent",
        "skill-creator",
        "skill-installer",
    ] {
        for policy in [ConflictPolicy::Error, ConflictPolicy::Skip] {
            let project = Project::new(&format!(
                "cross-scope-system-cache-install-{name}-{policy:?}"
            ));
            project.source_skill(name);

            let error = plan_codex(&project, &project.codex_context(policy))
                .expect_err("Codex owns the embedded cache across discovery and launch");

            assert_eq!(error.category(), ExitCategory::Filesystem, "{name}");
            assert!(error.to_string().contains("codex system"), "{name}");
        }
    }
}

#[test]
fn system_and_admin_scopes_sharing_a_terminal_keep_their_traversal_policies() {
    let project = Project::new("cross-scope-system-admin-shared-terminal");
    project.source_skill("alpha");
    let system = project.make_dir("codex-home/skills/.system");
    let foreign = project.make_dir("foreign-admin-skill");
    std::fs::write(
        foreign.join("SKILL.md"),
        "---\nname: alpha\ndescription: linked administrator alpha\n---\n",
    )
    .expect("linked Skill metadata");
    if !symlink_dir_or_skip(&foreign, &system.join("linked")) {
        return;
    }
    let mut context = project.codex_context(ConflictPolicy::Error);
    context.agent.codex_mut().admin_skills = Some(system);

    let snapshot = CodexAdapter
        .inspect_discovery(&context)
        .expect("shared-terminal discovery");
    let visible = snapshot
        .visible_skills
        .get(&SkillNameKey::new(std::ffi::OsStr::new("alpha")))
        .expect("administrator traversal retains the linked Skill");
    assert!(
        visible
            .iter()
            .any(|entry| entry.scope == ScopeKind::CodexAdmin)
    );

    let error = CodexAdapter
        .build_mount_plan(&context, &project.catalog(AgentId::Codex), &snapshot)
        .expect_err("system's no-link scan must not erase the administrator result");
    assert_eq!(error.category(), ExitCategory::Filesystem);
}

#[test]
fn bundled_cache_entries_are_never_reused_as_stable_selected_sources() {
    let project = Project::new("codex-system-cache-reuse");
    let source = project.make_dir("codex-home/skills/.system/alpha");
    std::fs::write(
        source.join("SKILL.md"),
        "---\nname: alpha\ndescription: bundled cache fixture\n---\n",
    )
    .expect("system Skill metadata");
    let occurrences = vec![SourceOccurrence {
        ordinal: 0,
        input_path: source.clone(),
        resolved_path: source,
    }];
    let catalog = resolve_catalog(
        &occurrences,
        &CatalogRequest {
            agent: AgentId::Codex,
            policy: crate::agent::adapter(AgentId::Codex).catalog_policy(),
            validation: ValidationLevel::Basic,
            destination_stores: &[],
        },
    )
    .expect("system source catalog");

    let error_context = project.codex_context(ConflictPolicy::Error);
    let snapshot = CodexAdapter
        .inspect_discovery(&error_context)
        .expect("system cache discovery");
    let error = CodexAdapter
        .build_mount_plan(&error_context, &catalog, &snapshot)
        .expect_err("Codex may delete or replace an exact-source cache entry before loading it");
    assert_eq!(error.category(), ExitCategory::Filesystem);

    let skip_context = project.codex_context(ConflictPolicy::Skip);
    let error = CodexAdapter
        .build_mount_plan(&skip_context, &catalog, &snapshot)
        .expect_err("skip cannot promise that Codex will preserve its mutable cache entry");
    assert_eq!(error.category(), ExitCategory::Filesystem);
}

#[test]
fn bundled_system_discovery_does_not_follow_nested_directory_links() {
    let project = Project::new("cross-scope-system-link-policy");
    project.source_skill("alpha");
    let system = project.make_dir("codex-home/skills/.system");
    let foreign = project.make_dir("foreign-system-skill");
    std::fs::write(
        foreign.join("SKILL.md"),
        "---\nname: alpha\ndescription: linked system alpha\n---\n",
    )
    .expect("linked system Skill metadata");
    if !symlink_dir_or_skip(&foreign, &system.join("linked")) {
        return;
    }

    let plan = plan_codex(&project, &project.codex_context(ConflictPolicy::Error))
        .expect("Codex ignores nested links in its bundled system root");

    assert_eq!(
        link_destinations(&plan),
        [project.preferred().join("alpha")]
    );
}

#[test]
fn bundled_system_discovery_follows_a_linked_root_before_applying_its_link_policy() {
    let project = Project::new("cross-scope-system-root-link-policy");
    project.source_skill("alpha");
    project.make_dir("codex-home/skills");
    let foreign_root = project.make_dir("foreign-system-root");
    let foreign = project.make_dir("foreign-system-root/foreign");
    std::fs::write(
        foreign.join("SKILL.md"),
        "---\nname: alpha\ndescription: linked system root alpha\n---\n",
    )
    .expect("linked system Skill metadata");
    if !symlink_dir_or_skip(
        &foreign_root,
        &project.root.join("codex-home/skills/.system"),
    ) {
        return;
    }

    let error = plan_codex(&project, &project.codex_context(ConflictPolicy::Error))
        .expect_err("Codex canonicalizes the bundled-system root before walking it");

    assert_eq!(error.category(), ExitCategory::Filesystem);
}

#[cfg(target_os = "linux")]
#[test]
fn an_unsupported_case_variant_outranks_a_skippable_destination() {
    let project = Project::new("cross-scope-duplicate-unsupported");
    project.source_skill("alpha");
    let store = project.make_dir(PREFERRED);
    std::fs::create_dir(store.join("ALPHA")).expect("skippable case-variant directory");
    if !symlink_dir_or_skip(Path::new("missing-target"), &store.join("alpha")) {
        return;
    }

    let error = plan_codex(&project, &project.codex_context(ConflictPolicy::Skip))
        .expect_err("skip must not hide an unsupported occupant under the same key");

    assert_eq!(error.category(), ExitCategory::Filesystem);
}

#[test]
fn every_codex_discovery_root_and_backing_store_is_locked() {
    let project = Project::new("codex-complete-lock-set");
    project.source_skill("alpha");
    project.make_dir(LEGACY);
    let nested = project.make_dir("nested");
    project.make_dir("nested/.agents/skills");
    project.make_dir("nested/.codex/skills");

    let mut context = project.codex_context(ConflictPolicy::Error);
    context.launch_cwd = nested.clone();
    let snapshot = CodexAdapter
        .inspect_discovery(&context)
        .expect("discovery snapshot");

    let preferred = project.preferred();
    let preferred_state = classify(&preferred).expect("missing Codex destination");
    let legacy_writer = crate::lock::LockResource::describe_entry(
        crate::lock::LockResourceKind::DiscoveryEntry,
        crate::lock::LockAccess::Mutate,
        &project.root,
        &preferred_state,
    )
    .expect("origin/dev/0.3.x Codex writer identity");
    assert_dual_discovery_keys(
        &snapshot,
        &preferred,
        crate::lock::LockAccess::Mutate,
        &legacy_writer,
    );

    let nested_legacy = nested.join(LEGACY);
    let legacy_observer = crate::lock::LockResource::describe_entry(
        crate::lock::LockResourceKind::DiscoveryEntry,
        crate::lock::LockAccess::Observe,
        &project.root,
        &classify(&nested_legacy).expect("existing Codex ancestor scope"),
    )
    .expect("origin/dev/0.3.x Codex observer identity");
    assert_dual_discovery_keys(
        &snapshot,
        &nested_legacy,
        crate::lock::LockAccess::Observe,
        &legacy_observer,
    );

    let resources = snapshot
        .lock_resources
        .iter()
        .map(|resource| (resource.kind, resource.access, resource.path.clone()))
        .collect::<std::collections::BTreeSet<_>>();
    let expected = [
        (
            crate::lock::LockResourceKind::DiscoveryEntry,
            crate::lock::LockAccess::Mutate,
            project.preferred(),
        ),
        (
            crate::lock::LockResourceKind::DiscoveryEntry,
            crate::lock::LockAccess::Mutate,
            project.root.join(".agents"),
        ),
        (
            crate::lock::LockResourceKind::DiscoveryEntry,
            crate::lock::LockAccess::Observe,
            project.legacy(),
        ),
        (
            crate::lock::LockResourceKind::BackingStore,
            crate::lock::LockAccess::Mutate,
            project.preferred(),
        ),
        (
            crate::lock::LockResourceKind::DiscoveryEntry,
            crate::lock::LockAccess::Observe,
            nested.join(PREFERRED),
        ),
        (
            crate::lock::LockResourceKind::DiscoveryEntry,
            crate::lock::LockAccess::Observe,
            nested.join(LEGACY),
        ),
        (
            crate::lock::LockResourceKind::DiscoveryEntry,
            crate::lock::LockAccess::Observe,
            project.root.join("home/.agents/skills"),
        ),
        (
            crate::lock::LockResourceKind::DiscoveryEntry,
            crate::lock::LockAccess::Observe,
            project.root.join("codex-home/skills"),
        ),
        (
            crate::lock::LockResourceKind::DiscoveryEntry,
            crate::lock::LockAccess::Observe,
            project.root.join("codex-home/skills/.system"),
        ),
        (
            crate::lock::LockResourceKind::DiscoveryEntry,
            crate::lock::LockAccess::Observe,
            project.root.join("admin/skills"),
        ),
    ]
    .into_iter()
    .collect::<std::collections::BTreeSet<_>>();

    assert_eq!(resources, expected);
}

#[test]
fn omp_emits_shared_and_legacy_keys_for_observation_and_mutation() {
    let project = Project::new("omp-dual-lock-identities");
    let context = project.context(AgentId::Omp, MountMode::Project, ConflictPolicy::Error);
    let snapshot = OmpAdapter
        .inspect_discovery(&context)
        .expect("OMP discovery snapshot");

    let destination = project.root.join(".omp/skills");
    let legacy_writer = crate::lock::LockResource::describe_entry(
        crate::lock::LockResourceKind::DiscoveryEntry,
        crate::lock::LockAccess::Mutate,
        &project.root,
        &classify(&destination).expect("missing OMP destination"),
    )
    .expect("origin/dev/0.3.x OMP writer identity");
    assert_dual_discovery_keys(
        &snapshot,
        &destination,
        crate::lock::LockAccess::Mutate,
        &legacy_writer,
    );

    let observed = project.root.join("home/.omp/agent/skills");
    let legacy_observer = crate::lock::LockResource::describe_entry(
        crate::lock::LockResourceKind::DiscoveryEntry,
        crate::lock::LockAccess::Observe,
        &project.root,
        &classify(&observed).expect("missing OMP user scope"),
    )
    .expect("origin/dev/0.3.x OMP observer identity");
    assert_dual_discovery_keys(
        &snapshot,
        &observed,
        crate::lock::LockAccess::Observe,
        &legacy_observer,
    );
}

#[test]
fn claude_emits_shared_and_legacy_keys_for_observation_and_project_mutation() {
    let project = Project::new("claude-dual-lock-identities");
    let context = project.context(AgentId::Claude, MountMode::Project, ConflictPolicy::Error);
    let snapshot = ClaudeAdapter
        .inspect_discovery(&context)
        .expect("Claude discovery snapshot");

    let legacy_writer = crate::lock::LockResource::describe(
        crate::lock::LockResourceKind::DiscoveryEntry,
        crate::lock::LockAccess::Mutate,
        &project.root,
        &project.root,
    )
    .expect("origin/dev/0.3.x Claude project writer identity");
    assert_dual_discovery_keys(
        &snapshot,
        &project.root,
        crate::lock::LockAccess::Mutate,
        &legacy_writer,
    );

    let observed = project.root.join("home/.claude/skills");
    let legacy_observer = crate::lock::LockResource::describe_entry(
        crate::lock::LockResourceKind::DiscoveryEntry,
        crate::lock::LockAccess::Observe,
        &project.root,
        &classify(&observed).expect("missing Claude user scope"),
    )
    .expect("legacy project-scope observer identity");
    assert_dual_discovery_keys(
        &snapshot,
        &observed,
        crate::lock::LockAccess::Observe,
        &legacy_observer,
    );
}

#[test]
fn the_same_source_already_visible_elsewhere_is_reused_rather_than_duplicated() {
    let project = Project::new("cross-scope-same-source");
    let source = project.source_skill("alpha");
    project.make_dir(LEGACY);
    let nested = project.make_dir("nested");
    let ancestor_store = project.make_dir("nested/.agents/skills");
    if !symlink_dir_or_skip(&source, &ancestor_store.join("alpha")) {
        return;
    }

    let mut context = project.codex_context(ConflictPolicy::Error);
    context.launch_cwd = nested;

    let plan = plan_codex(&project, &context).expect("the same source is already visible");

    assert!(link_destinations(&plan).is_empty());
    assert_eq!(reuse_destinations(&plan), [ancestor_store.join("alpha")]);
}

#[test]
fn the_same_source_reached_as_a_regular_child_of_a_linked_collection_is_reused() {
    let project = Project::new("cross-scope-linked-collection-same-source");
    let source = project.source_skill("alpha");
    let store = project.make_dir(PREFERRED);
    if !symlink_dir_or_skip(&project.sources, &store.join("collection")) {
        return;
    }

    let plan = plan_codex(&project, &project.codex_context(ConflictPolicy::Error))
        .expect("canonical source identity makes the nested regular directory reusable");

    assert!(link_destinations(&plan).is_empty());
    assert_eq!(reuse_destinations(&plan), [store.join("collection/alpha")]);
    assert_eq!(
        std::fs::canonicalize(store.join("collection/alpha")).unwrap(),
        source
    );
}

#[test]
fn the_expected_layout_does_not_turn_ordinary_mounts_into_reuse() {
    let project = Project::new("cross-scope-dedupe");
    project.source_skill("alpha");
    let store = project.make_dir(LEGACY);
    project.make_dir(".agents");
    if !symlink_dir_or_skip(&store, &project.preferred()) {
        return;
    }
    std::fs::create_dir_all(store.join("existing")).expect("unrelated entry");

    let plan = plan_codex(&project, &project.codex_context(ConflictPolicy::Error))
        .expect("the expected layout plans normally");

    assert_eq!(
        link_destinations(&plan),
        [project.preferred().join("alpha")],
        "the store reached through .agents/skills must not look like a foreign scope"
    );
    assert!(reuse_destinations(&plan).is_empty());
}

// ---------------------------------------------------------------------------
// Claude staging.
// ---------------------------------------------------------------------------

#[test]
fn claude_staging_injects_add_dir_and_leaves_the_project_alone() {
    let project = Project::new("claude-staging");
    project.source_skill("alpha");
    let project_skills = project.make_dir(".claude/skills");
    std::fs::create_dir_all(project_skills.join("existing")).expect("project skill");

    let context = project.context(AgentId::Claude, MountMode::Staging, ConflictPolicy::Error);
    let catalog = project.catalog(AgentId::Claude);

    let plan = assert_no_side_effects(&[&project.root], || {
        let snapshot = ClaudeAdapter.inspect_discovery(&context).unwrap();
        ClaudeAdapter
            .build_mount_plan(&context, &catalog, &snapshot)
            .expect("staging plans without touching the project")
    });

    assert_eq!(plan.launch.injected_args[0], OsString::from("--add-dir"));
    assert_eq!(
        PathBuf::from(&plan.launch.injected_args[1]),
        plan.discovery.entry
    );
    assert_eq!(plan.launch.injected_args[2], OsString::from("--settings"));
    let settings: serde_json::Value = serde_json::from_str(
        plan.launch.injected_args[3]
            .to_str()
            .expect("generated settings are Unicode JSON"),
    )
    .expect("generated settings are valid JSON");
    assert_eq!(settings["skillOverrides"]["alpha"], "on");
    assert!(
        plan.discovery
            .backing_store
            .starts_with(&plan.discovery.entry),
        "Skills stage inside the directory handed to --add-dir"
    );
    assert!(
        !plan.discovery.backing_store.starts_with(&project.root),
        "staging must never resolve into the project"
    );
    assert!(
        plan.actions
            .iter()
            .any(|action| matches!(action.operation, MountAction::CreateDirectory { .. })),
        "the staging tree is planned for creation"
    );
}

#[test]
fn a_project_skill_with_the_same_key_blocks_staging_by_default() {
    let project = Project::new("claude-project-conflict");
    project.source_skill("alpha");
    let project_skills = project.make_dir(".claude/skills");
    std::fs::create_dir_all(project_skills.join("alpha")).expect("project skill");

    let context = project.context(AgentId::Claude, MountMode::Staging, ConflictPolicy::Error);
    let catalog = project.catalog(AgentId::Claude);
    let snapshot = ClaudeAdapter.inspect_discovery(&context).unwrap();

    let error = ClaudeAdapter
        .build_mount_plan(&context, &catalog, &snapshot)
        .expect_err("the child would see two Skills under one name");

    assert_eq!(error.category(), ExitCategory::Filesystem);
}

#[test]
fn an_add_dir_scope_participates_in_conflict_detection() {
    let project = Project::new("claude-add-dir");
    project.source_skill("alpha");
    let extra = project.make_dir("extra/.claude/skills");
    std::fs::create_dir_all(extra.join("alpha")).expect("add-dir skill");

    let mut context = project.context(AgentId::Claude, MountMode::Staging, ConflictPolicy::Error);
    context.passthrough_args = vec![
        OsString::from("--add-dir"),
        project.root.join("extra").into_os_string(),
    ];
    let catalog = project.catalog(AgentId::Claude);
    let snapshot = ClaudeAdapter.inspect_discovery(&context).unwrap();
    assert!(
        snapshot
            .scopes
            .iter()
            .any(|scope| scope.kind == ScopeKind::ClaudeAddDir)
    );

    let error = ClaudeAdapter
        .build_mount_plan(&context, &catalog, &snapshot)
        .expect_err("a passthrough scope can veto a mount");

    assert_eq!(error.category(), ExitCategory::Filesystem);
}

#[test]
fn a_launch_cwd_ancestor_claude_scope_blocks_staging() {
    let project = Project::new("claude-ancestor-conflict");
    project.source_skill("alpha");
    let nested = project.make_dir("nested/deeper");
    let existing = project.make_dir("nested/.claude/skills/alpha");
    std::fs::write(
        existing.join("SKILL.md"),
        "---\nname: alpha\ndescription: ancestor fixture\n---\n",
    )
    .expect("ancestor Skill metadata");
    let mut context = project.context(AgentId::Claude, MountMode::Staging, ConflictPolicy::Error);
    context.launch_cwd = nested;

    let error = plan_claude(&project, &context)
        .expect_err("the pinned Claude release loads ancestor project scopes at startup");

    assert_eq!(error.category(), ExitCategory::Filesystem);
}

#[test]
fn a_relative_claude_add_dir_is_resolved_from_the_launch_cwd() {
    let project = Project::new("claude-relative-add-dir");
    project.source_skill("alpha");
    let nested = project.make_dir("nested");
    let existing = project.make_dir("nested/extra/.claude/skills/alpha");
    std::fs::write(
        existing.join("SKILL.md"),
        "---\nname: alpha\ndescription: add-dir fixture\n---\n",
    )
    .expect("add-dir Skill metadata");
    let mut context = project.context(AgentId::Claude, MountMode::Staging, ConflictPolicy::Error);
    context.launch_cwd = nested;
    context.passthrough_args = vec![OsString::from("--add-dir"), OsString::from("extra")];

    let error = plan_claude(&project, &context)
        .expect_err("relative add-dir discovery uses the child launch CWD");

    assert_eq!(error.category(), ExitCategory::Filesystem);
}

#[test]
fn a_same_source_claude_user_skill_is_reused_without_cleanup_ownership() {
    let project = Project::new("claude-user-same-source");
    let source = project.source_skill("alpha");
    let user_store = project.make_dir("home/.claude/skills");
    if !symlink_dir_or_skip(&source, &user_store.join("alpha")) {
        return;
    }
    let context = project.context(AgentId::Claude, MountMode::Staging, ConflictPolicy::Error);

    let plan = plan_claude(&project, &context).expect("the source is already visible to Claude");

    assert_eq!(reuse_destinations(&plan), [user_store.join("alpha")]);
    assert!(link_destinations(&plan).is_empty());
    assert!(
        plan.created_actions()
            .all(|action| !matches!(&action.operation, MountAction::ReuseExistingLink { .. }))
    );
}

#[test]
fn claude_config_dir_relocates_the_user_skill_scope() {
    let project = Project::new("claude-config-dir-user-scope");
    project.source_skill("alpha");
    let existing = project.make_dir("custom-claude/skills/alpha");
    std::fs::write(
        existing.join("SKILL.md"),
        "---\nname: alpha\ndescription: relocated user fixture\n---\n",
    )
    .expect("relocated user Skill metadata");
    let mut context = project.context(AgentId::Claude, MountMode::Staging, ConflictPolicy::Error);
    context.agent.claude_mut().config_dir = project.root.join("custom-claude");

    let error = plan_claude(&project, &context)
        .expect_err("CLAUDE_CONFIG_DIR replaces the default user discovery root");

    assert_eq!(error.category(), ExitCategory::Filesystem);
}

#[test]
fn a_foreign_managed_claude_skill_cannot_be_skipped() {
    for policy in [ConflictPolicy::Error, ConflictPolicy::Skip] {
        let project = Project::new(&format!("claude-managed-foreign-{policy:?}"));
        project.source_skill("alpha");
        let existing = project.make_dir("claude-managed/skills/alpha");
        std::fs::write(
            existing.join("SKILL.md"),
            "---\nname: alpha\ndescription: managed fixture\n---\n",
        )
        .expect("managed Skill metadata");
        let context = project.context(AgentId::Claude, MountMode::Staging, policy);

        let error = plan_claude(&project, &context)
            .expect_err("the managed scope outranks staging under every conflict policy");

        assert_eq!(error.category(), ExitCategory::Filesystem, "{policy:?}");
        assert!(error.to_string().contains("claude managed"), "{policy:?}");
    }
}

#[test]
fn a_project_alias_to_managed_claude_skills_keeps_managed_precedence() {
    for policy in [ConflictPolicy::Error, ConflictPolicy::Skip] {
        let project = Project::new(&format!("claude-managed-alias-{policy:?}"));
        project.source_skill("alpha");
        let managed_skill = project.make_dir("claude-managed/skills/alpha");
        std::fs::write(
            managed_skill.join("SKILL.md"),
            "---\nname: alpha\ndescription: managed alias fixture\n---\n",
        )
        .expect("managed alias Skill metadata");
        project.make_dir(".claude");
        if !symlink_dir_or_skip(
            &project.root.join("claude-managed/skills"),
            &project.root.join(".claude/skills"),
        ) {
            return;
        }
        let context = project.context(AgentId::Claude, MountMode::Staging, policy);

        let error = plan_claude(&project, &context)
            .expect_err("a project alias cannot downgrade enterprise precedence");

        assert_eq!(error.category(), ExitCategory::Filesystem, "{policy:?}");
        assert!(error.to_string().contains("claude managed"), "{policy:?}");
    }
}

#[test]
fn a_same_source_managed_alias_is_reused_without_losing_scope_evidence() {
    let project = Project::new("claude-managed-alias-same-source");
    let source = project.source_skill("alpha");
    let managed = project.make_dir("claude-managed/skills");
    if !symlink_dir_or_skip(&source, &managed.join("alpha")) {
        return;
    }
    project.make_dir(".claude");
    if !symlink_dir_or_skip(&managed, &project.root.join(".claude/skills")) {
        return;
    }
    let context = project.context(AgentId::Claude, MountMode::Staging, ConflictPolicy::Error);

    let snapshot = ClaudeAdapter
        .inspect_discovery(&context)
        .expect("managed alias discovery");
    assert!(
        snapshot
            .scopes
            .iter()
            .any(|scope| scope.kind == ScopeKind::ClaudeManaged),
        "terminal deduplication must retain managed policy evidence"
    );
    let plan = ClaudeAdapter
        .build_mount_plan(&context, &project.catalog(AgentId::Claude), &snapshot)
        .expect("the exact managed source remains reusable through an alias");

    assert_eq!(
        reuse_destinations(&plan),
        [project.root.join(".claude/skills/alpha")]
    );
    assert!(link_destinations(&plan).is_empty());
}

#[test]
fn an_exact_source_managed_claude_skill_is_reused() {
    let project = Project::new("claude-managed-same-source");
    let source = project.source_skill("alpha");
    let managed = project.make_dir("claude-managed/skills");
    if !symlink_dir_or_skip(&source, &managed.join("alpha")) {
        return;
    }
    let context = project.context(AgentId::Claude, MountMode::Staging, ConflictPolicy::Error);

    let plan = plan_claude(&project, &context).expect("managed exact-source visibility is stable");

    assert_eq!(reuse_destinations(&plan), [managed.join("alpha")]);
    assert!(link_destinations(&plan).is_empty());
}

#[test]
fn a_foreign_claude_user_skill_errors_or_is_preserved_by_skip() {
    let project = Project::new("claude-user-foreign");
    project.source_skill("alpha");
    let existing = project.make_dir("home/.claude/skills/alpha");
    std::fs::write(
        existing.join("SKILL.md"),
        "---\nname: alpha\ndescription: user fixture\n---\n",
    )
    .expect("user Skill metadata");

    let error = plan_claude(
        &project,
        &project.context(AgentId::Claude, MountMode::Staging, ConflictPolicy::Error),
    )
    .expect_err("a foreign user Skill is ambiguous under the default policy");
    assert_eq!(error.category(), ExitCategory::Filesystem);

    let plan = plan_claude(
        &project,
        &project.context(AgentId::Claude, MountMode::Staging, ConflictPolicy::Skip),
    )
    .expect("skip preserves the visible user Skill");
    assert!(link_destinations(&plan).is_empty());
    assert_eq!(plan.preserved.len(), 1);
    assert_eq!(plan.preserved[0].existing, existing);
    assert_eq!(plan.preserved[0].scope, ScopeKind::ClaudeUser);
    let settings: serde_json::Value = serde_json::from_str(
        plan.launch.injected_args[3]
            .to_str()
            .expect("generated settings are Unicode JSON"),
    )
    .expect("generated settings are valid JSON");
    assert_eq!(
        settings["skillOverrides"]["alpha"], "on",
        "skip accepts the existing Skill, so it must remain visible for this session"
    );
}

#[test]
fn claude_case_variant_diagnostics_are_typed_and_conflicts_fail_closed() {
    let project = Project::new("claude-user-case-variants");
    project.source_skill("alpha");
    for name in ["alpha", "Alpha"] {
        let existing = project.make_dir(&format!("home/.claude/skills/{name}"));
        std::fs::write(
            existing.join("SKILL.md"),
            format!("---\nname: {name}\ndescription: case fixture\n---\n"),
        )
        .expect("case-variant metadata");
    }
    let context = project.context(AgentId::Claude, MountMode::Staging, ConflictPolicy::Error);
    let snapshot = ClaudeAdapter
        .inspect_discovery(&context)
        .expect("case variants are retained as evidence");

    let distinct_entries = std::fs::read_dir(project.root.join("home/.claude/skills"))
        .expect("user Skill scope")
        .filter_map(Result::ok)
        .count();
    if distinct_entries == 2 {
        assert!(
            snapshot
                .warnings
                .iter()
                .any(|warning| warning.kind == DiagnosticKind::ClaudeDiscovery),
            "a case-sensitive host retains both variants and reports a typed ambiguity"
        );
    }
    let error = ClaudeAdapter
        .build_mount_plan(&context, &project.catalog(AgentId::Claude), &snapshot)
        .expect_err("case-variant conflicts fail closed under the default policy");
    assert_eq!(error.category(), ExitCategory::Filesystem);
}

#[test]
fn every_staging_lock_resource_keeps_a_logical_key_before_the_root_exists() {
    let project = Project::new("claude-staging-locks");
    project.source_skill("alpha");
    let context = project.context(AgentId::Claude, MountMode::Staging, ConflictPolicy::Error);

    let snapshot = ClaudeAdapter.inspect_discovery(&context).unwrap();

    assert!(!snapshot.lock_resources.is_empty());
    assert!(
        snapshot
            .lock_resources
            .iter()
            .any(|resource| resource.access == crate::lock::LockAccess::Mutate),
        "the unique staging destination and helper chain require mutation access"
    );
    assert!(
        snapshot
            .lock_resources
            .iter()
            .any(|resource| resource.access == crate::lock::LockAccess::Observe),
        "shared project and user discovery scopes remain observations"
    );
    for resource in &snapshot.lock_resources {
        assert!(
            !resource.identity.logical_path().as_os_str().is_empty(),
            "a resource that does not exist yet still needs a lock key"
        );
        assert!(
            resource.identity.physical.is_none(),
            "nothing has been created, so no physical identity exists"
        );
    }
}

// ---------------------------------------------------------------------------
// The read-only guarantee.
// ---------------------------------------------------------------------------

#[test]
fn codex_planning_creates_nothing_even_when_the_whole_layout_is_missing() {
    let project = Project::new("read-only-codex");
    project.source_skill("alpha");
    project.source_skill("beta");
    let context = project.codex_context(ConflictPolicy::Error);

    let plan = assert_no_side_effects(&[&project.root], || {
        plan_codex(&project, &context).expect("a fully missing layout still plans")
    });

    assert_eq!(
        created_directories(&plan),
        [
            project.root.join(".agents").as_path(),
            project.preferred().as_path(),
        ]
    );
    for directory in created_directories(&plan) {
        assert!(
            !directory.exists(),
            "{} was planned, not created",
            directory.display()
        );
    }
    for destination in link_destinations(&plan) {
        assert!(!destination.exists(), "no link may exist after planning");
    }
}

#[test]
fn codex_launch_pins_the_inspected_cwd_and_discovery_configuration() {
    let project = Project::new("codex-pinned-session");
    project.source_skill("alpha");
    project.source_skill("beta");
    let context = project.codex_context(ConflictPolicy::Error);

    let plan = plan_codex(&project, &context).expect("pinned launch plan");

    assert_eq!(
        plan.launch.injected_args,
        vec![
            OsString::from("-C"),
            project.root.as_os_str().to_os_string(),
            OsString::from("-c"),
            OsString::from("project_root_markers=[\".git\"]"),
            OsString::from("-c"),
            OsString::from(
                "skills.config=[{name=\"alpha\",enabled=true},{name=\"beta\",enabled=true}]"
            ),
        ]
    );
}

#[test]
fn a_late_conflict_leaves_earlier_candidates_unapplied() {
    let project = Project::new("read-only-late-conflict");
    project.source_skill("alpha");
    project.source_skill("zeta");
    let store = project.make_dir(PREFERRED);
    std::fs::create_dir_all(store.join("zeta")).expect("conflicting entry");

    let context = project.codex_context(ConflictPolicy::Error);

    assert_no_side_effects(&[&project.root], || {
        let error = plan_codex(&project, &context).expect_err("the last candidate conflicts");
        assert_eq!(error.category(), ExitCategory::Filesystem);
    });

    assert!(
        !store.join("alpha").exists(),
        "the earlier candidate must not have been applied"
    );
}

#[test]
fn action_ids_follow_the_order_actions_apply_in() {
    let project = Project::new("plan-ordering");
    project.source_skill("beta");
    project.source_skill("alpha");

    let plan = plan_codex(&project, &project.codex_context(ConflictPolicy::Error)).unwrap();

    assert_eq!(
        plan.actions
            .iter()
            .map(|action| action.id)
            .collect::<Vec<_>>(),
        (1..=u32::try_from(plan.actions.len()).unwrap()).collect::<Vec<_>>()
    );
    let verbs = plan
        .actions
        .iter()
        .map(|action| action.operation.verb())
        .collect::<Vec<_>>();
    assert_eq!(
        verbs,
        ["MKDIR", "MKDIR", "LINK", "LINK"],
        "the preferred directory chain precedes Skill links"
    );
    assert!(
        plan.actions
            .iter()
            .all(|action| action.temporary_path.is_none()),
        "a preliminary plan has no session identifier to name a staging sibling with"
    );
}

#[test]
fn planning_is_deterministic_for_unchanged_input() {
    let project = Project::new("plan-deterministic");
    project.source_skill("gamma");
    project.source_skill("alpha");
    let context = project.codex_context(ConflictPolicy::Error);

    let first = plan_codex(&project, &context).unwrap();
    let second = plan_codex(&project, &context).unwrap();

    assert_eq!(first, second);
}

#[test]
fn the_registry_serves_one_static_adapter_for_every_supported_agent() {
    // Static references, not boxed values: adapters are stateless and the set is compile-time
    // closed, so lookup must allocate nothing and must be stable across calls.
    for agent in AgentId::ALL {
        let first = crate::agent::adapter(*agent);
        let second = crate::agent::adapter(*agent);
        assert!(
            std::ptr::eq(
                std::ptr::from_ref::<dyn AgentAdapter>(first).cast::<u8>(),
                std::ptr::from_ref::<dyn AgentAdapter>(second).cast::<u8>()
            ),
            "registry lookup must return one shared value"
        );
    }
}

#[test]
fn each_registered_adapter_reports_its_own_dated_evidence() {
    assert_eq!(
        crate::agent::adapter(AgentId::Codex)
            .version_spec()
            .last_tested_banner(),
        "codex-cli 0.146.0"
    );
    assert_eq!(
        crate::agent::adapter(AgentId::Claude)
            .version_spec()
            .last_tested_banner(),
        "2.1.220 (Claude Code)"
    );
}

#[test]
fn declarative_catalog_policy_records_each_agents_own_requirements() {
    let codex = crate::agent::adapter(AgentId::Codex).catalog_policy();
    assert!(codex.requires_exact_skill_md_entry);
    assert!(codex.always_parses_metadata);
    assert!(codex.requires_name);

    let claude = crate::agent::adapter(AgentId::Claude).catalog_policy();
    assert!(!claude.requires_exact_skill_md_entry);
    assert!(!claude.always_parses_metadata);
    assert!(!claude.requires_name);

    // Neither policy may relax a rule the catalog owns unconditionally.
    for policy in [codex, claude] {
        assert!(policy.requires_description);
        assert!(policy.requires_matching_name);
    }
}

#[test]
fn declarative_destination_stores_match_each_agents_planned_namespace() {
    let project = Project::new("destination-stores");
    let codex = project.context(AgentId::Codex, MountMode::Project, ConflictPolicy::Error);
    assert_eq!(
        crate::agent::adapter(AgentId::Codex).destination_stores(&codex),
        vec![project.preferred()]
    );

    let staging = project.context(AgentId::Claude, MountMode::Staging, ConflictPolicy::Error);
    assert!(
        crate::agent::adapter(AgentId::Claude)
            .destination_stores(&staging)
            .is_empty(),
        "an isolated staging root cannot sit inside a selected source"
    );

    let claude_project =
        project.context(AgentId::Claude, MountMode::Project, ConflictPolicy::Error);
    assert_eq!(
        crate::agent::adapter(AgentId::Claude).destination_stores(&claude_project),
        vec![project.root.join(".claude/skills")]
    );
}

/// A concrete adapter called with another Agent's resolved context is an internal invariant break.
///
/// Normal parsing and registry lookup make the mismatch unconstructable, so this can only be
/// reached by calling an adapter directly — and it must fail closed rather than inspect the wrong
/// roots.
#[test]
fn an_adapter_rejects_a_resolved_context_belonging_to_another_agent() {
    let project = Project::new("wrong-variant");
    let claude = project.context(AgentId::Claude, MountMode::Staging, ConflictPolicy::Error);
    let codex = project.codex_context(ConflictPolicy::Error);

    let error = CodexAdapter
        .inspect_discovery(&claude)
        .expect_err("the Codex adapter must refuse a resolved Claude context");
    assert_eq!(error.category(), ExitCategory::Internal);

    let error = ClaudeAdapter
        .inspect_discovery(&codex)
        .expect_err("the Claude adapter must refuse a resolved Codex context");
    assert_eq!(error.category(), ExitCategory::Internal);
}
