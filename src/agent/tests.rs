use std::ffi::OsString;
use std::path::{Path, PathBuf};

use super::claude::ClaudeAdapter;
use super::codex::{CodexAdapter, resolve_backing};
use super::{AgentAdapter, ScopeKind, inspect_scope};
use crate::catalog::{CatalogRequest, resolve_catalog};
use crate::domain::{
    AgentId, ConflictPolicy, LinkMode, MountMode, RunContext, RunOptions, SkillCatalog,
    SkillNameKey, SourceOccurrence, ValidationLevel,
};
use crate::error::ExitCategory;
use crate::mount::resolve::{PathKind, classify};
use crate::mount::{MountAction, MountPlan};
use crate::test_support::{
    TestDir, assert_no_side_effects, remove_directory_link, symlink_dir_or_skip,
};

const AUTHORITATIVE: &str = ".agents/skills";
const COMPATIBILITY: &str = ".codex/skills";

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

    fn authoritative(&self) -> PathBuf {
        self.root.join(AUTHORITATIVE)
    }

    fn compatibility(&self) -> PathBuf {
        self.root.join(COMPATIBILITY)
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
            agent,
            invocation_cwd: self.root.clone(),
            launch_cwd: self.root.clone(),
            project_root: self.root.clone(),
            skill_sources: Vec::new(),
            session_id: None,
            agent_bin: PathBuf::from(agent.executable_name()),
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

/// Returns only the per-Skill links, excluding the authoritative discovery link.
fn link_destinations(plan: &MountPlan) -> Vec<&Path> {
    plan.actions
        .iter()
        .filter_map(|action| match &action.operation {
            MountAction::CreateDirectoryLink { destination, .. }
                if destination != &plan.discovery.entry =>
            {
                Some(destination.as_path())
            }
            _ => None,
        })
        .collect()
}

/// Returns the target of the authoritative discovery link, when the plan creates one.
fn authoritative_link_target(plan: &MountPlan) -> Option<&Path> {
    plan.actions
        .iter()
        .find_map(|action| match &action.operation {
            MountAction::CreateDirectoryLink {
                source,
                destination,
                ..
            } if destination == &plan.discovery.entry => Some(source.as_path()),
            _ => None,
        })
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
// Section 14.2 state table: every row of the Codex discovery-entry resolution.
// ---------------------------------------------------------------------------

#[test]
fn agents_links_to_codex_store_is_the_expected_layout() {
    let project = Project::new("codex-row-link-to-c");
    let store = project.make_dir(COMPATIBILITY);
    project.make_dir(".agents");
    if !symlink_dir_or_skip(&store, &project.authoritative()) {
        return;
    }

    let backing = resolve_backing(
        &project.root,
        &classify(&project.authoritative()).unwrap(),
        &classify(&project.compatibility()).unwrap(),
    )
    .expect("the expected layout resolves");

    assert_eq!(backing.store, project.compatibility());
    assert!(
        backing.authoritative_link_target.is_none(),
        "an existing authoritative entry is never rewritten"
    );
    assert!(backing.create_directories.is_empty());
    assert!(backing.warnings.is_empty());
}

#[test]
fn agents_linking_elsewhere_wins_over_the_compatibility_store_and_warns() {
    let project = Project::new("codex-row-link-elsewhere");
    project.make_dir(COMPATIBILITY);
    let elsewhere = project.make_dir("shared-store");
    project.make_dir(".agents");
    if !symlink_dir_or_skip(&elsewhere, &project.authoritative()) {
        return;
    }

    let backing = resolve_backing(
        &project.root,
        &classify(&project.authoritative()).unwrap(),
        &classify(&project.compatibility()).unwrap(),
    )
    .expect("an authoritative link elsewhere still resolves");

    assert_eq!(backing.store, project.authoritative());
    assert!(backing.authoritative_link_target.is_none());
    assert_eq!(backing.warnings.len(), 1, "the divergence must be reported");
}

#[test]
fn a_regular_agents_directory_is_authoritative_over_every_compatibility_state() {
    let project = Project::new("codex-row-regular-a");
    let authoritative = project.make_dir(AUTHORITATIVE);

    // C missing.
    let backing = resolve_backing(
        &project.root,
        &classify(&authoritative).unwrap(),
        &classify(&project.compatibility()).unwrap(),
    )
    .unwrap();
    assert_eq!(backing.store, authoritative);
    assert!(backing.warnings.is_empty());

    // C a separate regular directory: never merged, and reported.
    project.make_dir(COMPATIBILITY);
    let backing = resolve_backing(
        &project.root,
        &classify(&authoritative).unwrap(),
        &classify(&project.compatibility()).unwrap(),
    )
    .unwrap();
    assert_eq!(backing.store, authoritative);
    assert_eq!(backing.warnings.len(), 1);
    assert!(backing.create_directories.is_empty());
}

#[test]
fn a_compatibility_link_back_to_agents_selects_agents_without_warning() {
    let project = Project::new("codex-row-c-links-a");
    let authoritative = project.make_dir(AUTHORITATIVE);
    project.make_dir(".codex");
    if !symlink_dir_or_skip(&authoritative, &project.compatibility()) {
        return;
    }

    let backing = resolve_backing(
        &project.root,
        &classify(&authoritative).unwrap(),
        &classify(&project.compatibility()).unwrap(),
    )
    .unwrap();

    assert_eq!(backing.store, authoritative);
    assert!(
        backing.warnings.is_empty(),
        "both entries already name one directory, so there is nothing to report"
    );
}

#[test]
fn a_missing_agents_entry_is_planned_against_an_existing_codex_store() {
    let project = Project::new("codex-row-missing-a");
    let store = project.make_dir(COMPATIBILITY);

    let backing = resolve_backing(
        &project.root,
        &classify(&project.authoritative()).unwrap(),
        &classify(&store).unwrap(),
    )
    .unwrap();

    assert_eq!(backing.store, store);
    assert_eq!(backing.authoritative_link_target, Some(store));
    assert_eq!(
        backing.create_directories,
        [project.root.join(".agents")],
        "only the missing parent of the authoritative entry is created"
    );
}

#[test]
fn a_missing_agents_entry_links_past_a_compatibility_link_to_its_terminal() {
    let project = Project::new("codex-row-missing-a-linked-c");
    let terminal = project.make_dir("shared-store");
    project.make_dir(".codex");
    if !symlink_dir_or_skip(&terminal, &project.compatibility()) {
        return;
    }

    let backing = resolve_backing(
        &project.root,
        &classify(&project.authoritative()).unwrap(),
        &classify(&project.compatibility()).unwrap(),
    )
    .unwrap();

    assert_eq!(backing.store, project.compatibility());
    assert_eq!(
        backing.authoritative_link_target,
        Some(terminal),
        "the new link points at the terminal directory so the chain stays one hop deep"
    );
}

#[test]
fn both_entries_missing_plans_the_full_helper_chain_in_dependency_order() {
    let project = Project::new("codex-row-both-missing");

    let backing = resolve_backing(
        &project.root,
        &classify(&project.authoritative()).unwrap(),
        &classify(&project.compatibility()).unwrap(),
    )
    .unwrap();

    assert_eq!(backing.store, project.compatibility());
    assert_eq!(
        backing.create_directories,
        [
            project.root.join(".codex"),
            project.compatibility(),
            project.root.join(".agents"),
        ],
        "a parent is always created before the entry beneath it"
    );
    assert_eq!(
        backing.authoritative_link_target,
        Some(project.compatibility())
    );
}

#[test]
fn an_unresolvable_authoritative_entry_fails_before_any_action_exists() {
    let project = Project::new("codex-row-unresolvable");
    project.make_dir(".agents");

    // Broken link.
    if !symlink_dir_or_skip(&project.root.join("absent"), &project.authoritative()) {
        return;
    }
    let error = resolve_backing(
        &project.root,
        &classify(&project.authoritative()).unwrap(),
        &classify(&project.compatibility()).unwrap(),
    )
    .expect_err("a broken authoritative entry has no safe destination");
    assert_eq!(error.category(), ExitCategory::Filesystem);
    remove_directory_link(&project.authoritative());

    // Non-directory.
    std::fs::write(project.authoritative(), "not a namespace").expect("file fixture");
    let error = resolve_backing(
        &project.root,
        &classify(&project.authoritative()).unwrap(),
        &classify(&project.compatibility()).unwrap(),
    )
    .expect_err("a file cannot hold Skills");
    assert_eq!(error.category(), ExitCategory::Filesystem);
    std::fs::remove_file(project.authoritative()).expect("reset fixture");

    // Link cycle.
    let other = project.root.join(".agents/other");
    assert!(symlink_dir_or_skip(&other, &project.authoritative()));
    assert!(symlink_dir_or_skip(&project.authoritative(), &other));
    let resolved = classify(&project.authoritative()).unwrap();
    assert_eq!(resolved.kind, PathKind::CyclicLink);
    let error = resolve_backing(
        &project.root,
        &resolved,
        &classify(&project.compatibility()).unwrap(),
    )
    .expect_err("a cycle has no terminal directory");
    assert_eq!(error.category(), ExitCategory::Filesystem);
}

#[test]
fn a_missing_authoritative_entry_over_a_broken_store_fails_closed() {
    let project = Project::new("codex-row-broken-c");
    project.make_dir(".codex");
    if !symlink_dir_or_skip(&project.root.join("absent"), &project.compatibility()) {
        return;
    }

    let error = resolve_backing(
        &project.root,
        &classify(&project.authoritative()).unwrap(),
        &classify(&project.compatibility()).unwrap(),
    )
    .expect_err("a broken compatibility store cannot back a new authoritative link");

    assert_eq!(error.category(), ExitCategory::Filesystem);
}

// ---------------------------------------------------------------------------
// Scope inspection, including the ADR-010 behaviour for non-portable names.
// ---------------------------------------------------------------------------

#[test]
fn an_entry_that_is_not_a_portable_name_still_occupies_its_logical_key() {
    let project = Project::new("scope-non-portable");
    let store = project.make_dir(COMPATIBILITY);
    std::fs::create_dir_all(store.join("My_Skill")).expect("uppercase entry");
    std::fs::create_dir_all(store.join("rust--review")).expect("double-hyphen entry");

    let scope = inspect_scope(ScopeKind::CodexCompatibility, &store).unwrap();

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

    let first = inspect_scope(ScopeKind::CodexCompatibility, &forward).unwrap();
    let second = inspect_scope(ScopeKind::CodexCompatibility, &reverse).unwrap();

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
            .map(|existing| existing.raw_name.clone())
            .collect::<Vec<_>>()
    };

    assert_eq!(keys(&first), ["alpha", "beta", "middle", "zeta"]);
    assert_eq!(keys(&first), keys(&second));
    assert_eq!(raw_names(&first), raw_names(&second));
    assert_eq!(
        inspect_scope(ScopeKind::CodexCompatibility, &forward).unwrap(),
        first,
        "repeating one inspection must also be stable"
    );
}

#[test]
fn a_missing_scope_reports_no_occupants_instead_of_failing() {
    let project = Project::new("scope-missing");

    let scope = inspect_scope(ScopeKind::CodexAuthoritative, &project.authoritative()).unwrap();

    assert_eq!(scope.state.kind, PathKind::Missing);
    assert!(scope.existing_skills.is_empty());
}

// ---------------------------------------------------------------------------
// Section 15.6 conflict table.
// ---------------------------------------------------------------------------

#[test]
fn a_missing_destination_plans_a_link_under_both_policies() {
    for policy in [ConflictPolicy::Error, ConflictPolicy::Skip] {
        let project = Project::new("conflict-missing");
        let source = project.source_skill("alpha");
        project.make_dir(COMPATIBILITY);

        let plan = plan_codex(&project, &project.codex_context(policy)).expect("plan builds");

        assert_eq!(
            link_destinations(&plan),
            [project.compatibility().join("alpha")]
        );
        assert_eq!(
            authoritative_link_target(&plan),
            Some(project.compatibility().as_path()),
            "a missing .agents/skills is linked at the existing store"
        );
        assert_eq!(
            plan.actions
                .iter()
                .filter_map(|action| match &action.operation {
                    MountAction::CreateDirectoryLink {
                        source,
                        destination,
                        ..
                    } if destination != &plan.discovery.entry => Some(source.as_path()),
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
    let store = project.make_dir(COMPATIBILITY);
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
        plan.owned_actions().all(|action| match &action.operation {
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
    let store = project.make_dir(COMPATIBILITY);
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
    let store = project.make_dir(COMPATIBILITY);
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
    let store = project.make_dir(COMPATIBILITY);
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
    let store = project.make_dir(COMPATIBILITY);
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
    let store = project.make_dir(COMPATIBILITY);
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
    project.make_dir(COMPATIBILITY);
    let nested = project.make_dir("nested");
    let ancestor_store = project.make_dir("nested/.agents/skills");
    std::fs::create_dir_all(ancestor_store.join("alpha")).expect("ancestor skill");

    let mut context = project.codex_context(ConflictPolicy::Error);
    context.launch_cwd = nested;

    let error = plan_codex(&project, &context)
        .expect_err("a Skill already visible to the child must not be silently duplicated");

    assert_eq!(error.category(), ExitCategory::Filesystem);
}

#[test]
fn the_same_source_already_visible_elsewhere_is_reused_rather_than_duplicated() {
    let project = Project::new("cross-scope-same-source");
    let source = project.source_skill("alpha");
    project.make_dir(COMPATIBILITY);
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
fn the_expected_layout_does_not_turn_ordinary_mounts_into_reuse() {
    let project = Project::new("cross-scope-dedupe");
    project.source_skill("alpha");
    let store = project.make_dir(COMPATIBILITY);
    project.make_dir(".agents");
    if !symlink_dir_or_skip(&store, &project.authoritative()) {
        return;
    }
    std::fs::create_dir_all(store.join("existing")).expect("unrelated entry");

    let plan = plan_codex(&project, &project.codex_context(ConflictPolicy::Error))
        .expect("the expected layout plans normally");

    assert_eq!(
        link_destinations(&plan),
        [store.join("alpha")],
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
fn every_staging_lock_resource_keeps_a_logical_key_before_the_root_exists() {
    let project = Project::new("claude-staging-locks");
    project.source_skill("alpha");
    let context = project.context(AgentId::Claude, MountMode::Staging, ConflictPolicy::Error);

    let snapshot = ClaudeAdapter.inspect_discovery(&context).unwrap();

    assert_eq!(snapshot.lock_resources.len(), 2);
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
            project.root.join(".codex").as_path(),
            project.compatibility().as_path(),
            project.root.join(".agents").as_path(),
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
fn a_late_conflict_leaves_earlier_candidates_unapplied() {
    let project = Project::new("read-only-late-conflict");
    project.source_skill("alpha");
    project.source_skill("zeta");
    let store = project.make_dir(COMPATIBILITY);
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
        ["MKDIR", "MKDIR", "MKDIR", "LINK", "LINK", "LINK"],
        "helper directories precede the authoritative link, which precedes Skills"
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
