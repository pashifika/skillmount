//! Codex adapter: preferred `.agents/skills` mounts with observed `.agents` and legacy `.codex`
//! discovery roots.

use std::collections::BTreeSet;
use std::ffi::{OsStr, OsString};
use std::fs;
use std::path::{Path, PathBuf};

use crate::agent::{
    AgentAdapter, DiscoveryScope, DiscoverySnapshot, ScopeKind, dedupe_scopes_by_terminal,
    inspect_scope,
};
use crate::diagnostic::{Diagnostic, DiagnosticKind};
use crate::domain::{AgentId, RunContext, SkillCatalog, SkillNameKey};
use crate::error::{AppError, PlanError};
use crate::lock::{LockResource, LockResourceKind};
use crate::mount::plan::apply_conflict_policy;
use crate::mount::resolve::{PathKind, ResolvedEntry, classify};
use crate::mount::{
    ActionSequence, DiscoveryPlan, LaunchPlan, MountAction, MountPlan, PathPrecondition,
};

/// Relative discovery entry Codex reads.
const AUTHORITATIVE: &str = ".agents/skills";
/// Relative compatibility store and legacy discovery root Codex retains.
const COMPATIBILITY: &str = ".codex/skills";
/// Maximum distinct terminal directories one Codex discovery root may traverse.
const MAX_DISCOVERY_DIRECTORIES: usize = 16_384;

/// The Codex agent adapter.
#[derive(Debug, Clone, Copy, Default)]
pub struct CodexAdapter;

/// Outcome of the `.agents/skills` and `.codex/skills` state table.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct CodexBacking {
    /// Visible path selected Skills are written through.
    pub(crate) store: PathBuf,
    /// Observed state of that path.
    pub(crate) store_state: PathKind,
    /// Directories the plan must create first, parents before children.
    pub(crate) create_directories: Vec<PathBuf>,
    /// Target of the authoritative link, when one has to be created.
    pub(crate) authoritative_link_target: Option<PathBuf>,
    /// Non-fatal observations about the layout.
    pub(crate) warnings: Vec<Diagnostic>,
}

/// Applies the authoritative Codex discovery-entry state table.
///
/// `.agents/skills` is authoritative for placement: when it exists, its configuration decides the
/// mount destination. `.codex/skills` remains an independently visible legacy discovery root even
/// when it is not the selected backing store. Selecting "whichever side is not a link" is
/// deliberately not implemented; that heuristic is unsafe when both sides are directories, when
/// both are links, or when the authoritative entry points somewhere else entirely.
///
/// # Errors
///
/// Returns [`AppError::Plan`] when the authoritative entry is broken, cyclic, over-deep, or not a
/// directory, and when the authoritative entry is absent while the compatibility entry is itself
/// unresolvable. In each case no safe destination exists.
pub(crate) fn resolve_backing(
    project_root: &Path,
    authoritative: &ResolvedEntry,
    compatibility: &ResolvedEntry,
) -> Result<CodexBacking, AppError> {
    if authoritative.kind.is_ambiguous() {
        return Err(PlanError::AmbiguousDiscoveryEntry {
            path: authoritative.entry.clone(),
            state: authoritative.kind.label(),
        }
        .into());
    }

    let mut warnings = Vec::new();
    match authoritative.kind {
        PathKind::DirectoryLink => {
            // The authoritative entry already decides where Skills live. It is never rewritten,
            // even when it points away from the compatibility store.
            let reaches_compatibility = authoritative.shares_terminal_with(compatibility);
            if !reaches_compatibility && compatibility.kind != PathKind::Missing {
                warnings.push(Diagnostic::warning_with_kind(
                    DiagnosticKind::CodexDiscovery,
                    format!(
                        "{} links outside {}; new mounts use the authoritative entry while the separate legacy root remains visible for conflict detection",
                        authoritative.entry.display(),
                        compatibility.entry.display()
                    ),
                    authoritative.entry.clone(),
                ));
            }
            let store = if reaches_compatibility {
                compatibility.entry.clone()
            } else {
                authoritative.entry.clone()
            };
            let store_state = if reaches_compatibility {
                compatibility.kind
            } else {
                authoritative.kind
            };
            Ok(CodexBacking {
                store,
                store_state,
                create_directories: Vec::new(),
                authoritative_link_target: None,
                warnings,
            })
        }
        PathKind::Directory => {
            if matches!(
                compatibility.kind,
                PathKind::Directory | PathKind::DirectoryLink
            ) && !authoritative.shares_terminal_with(compatibility)
            {
                warnings.push(Diagnostic::warning_with_kind(
                    DiagnosticKind::CodexDiscovery,
                    format!(
                        "{} and {} resolve to separate directories; new mounts use the authoritative entry while both roots remain visible for conflict detection",
                        authoritative.entry.display(),
                        compatibility.entry.display()
                    ),
                    compatibility.entry.clone(),
                ));
            }
            Ok(CodexBacking {
                store: authoritative.entry.clone(),
                store_state: authoritative.kind,
                create_directories: Vec::new(),
                authoritative_link_target: None,
                warnings,
            })
        }
        PathKind::Missing => resolve_missing_authoritative(project_root, compatibility, warnings),
        // Ambiguous states returned early; `Missing`, `Directory`, and `DirectoryLink` are the rest.
        other => Err(PlanError::AmbiguousDiscoveryEntry {
            path: authoritative.entry.clone(),
            state: other.label(),
        }
        .into()),
    }
}

/// Handles every row of the state table where `.agents/skills` does not exist.
fn resolve_missing_authoritative(
    project_root: &Path,
    compatibility: &ResolvedEntry,
    warnings: Vec<Diagnostic>,
) -> Result<CodexBacking, AppError> {
    let agents_parent = project_root.join(".agents");
    match compatibility.kind {
        PathKind::Directory => Ok(CodexBacking {
            store: compatibility.entry.clone(),
            store_state: compatibility.kind,
            create_directories: missing_parents(&agents_parent),
            authoritative_link_target: Some(compatibility.entry.clone()),
            warnings,
        }),
        PathKind::DirectoryLink => {
            // The authoritative link is pointed at the terminal directory rather than at another
            // link, so the chain the agent follows stays one hop deep.
            let terminal = compatibility.terminal.clone().ok_or_else(|| {
                AppError::Internal(
                    "a resolvable directory link must expose a terminal directory".to_owned(),
                )
            })?;
            Ok(CodexBacking {
                store: compatibility.entry.clone(),
                store_state: compatibility.kind,
                create_directories: missing_parents(&agents_parent),
                authoritative_link_target: Some(terminal),
                warnings,
            })
        }
        PathKind::Missing => {
            let mut create_directories = missing_parents(&project_root.join(".codex"));
            create_directories.push(compatibility.entry.clone());
            create_directories.extend(missing_parents(&agents_parent));
            Ok(CodexBacking {
                store: compatibility.entry.clone(),
                store_state: PathKind::Missing,
                create_directories,
                authoritative_link_target: Some(compatibility.entry.clone()),
                warnings,
            })
        }
        other => Err(PlanError::AmbiguousDiscoveryEntry {
            path: compatibility.entry.clone(),
            state: other.label(),
        }
        .into()),
    }
}

/// Returns `path` when it has to be created, or nothing when it already exists.
///
/// Only one level is considered because both callers pass a direct child of the project root,
/// which is itself guaranteed to exist by path resolution.
fn missing_parents(path: &Path) -> Vec<PathBuf> {
    if path.exists() {
        Vec::new()
    } else {
        vec![path.to_path_buf()]
    }
}

impl CodexAdapter {
    fn authoritative_entry(context: &RunContext) -> PathBuf {
        context.project_root.join(AUTHORITATIVE)
    }

    fn compatibility_entry(context: &RunContext) -> PathBuf {
        context.project_root.join(COMPATIBILITY)
    }

    /// Collects every `.agents/skills` and `.codex/skills` between the launch CWD and the project
    /// root, exclusive.
    ///
    /// The project root's own entry is inspected separately as the authoritative scope.
    fn ancestor_scopes(context: &RunContext) -> Result<Vec<DiscoveryScope>, AppError> {
        let mut scopes = Vec::new();
        for ancestor in context.launch_cwd.ancestors() {
            if ancestor == context.project_root {
                break;
            }
            if !ancestor.starts_with(&context.project_root) {
                break;
            }
            scopes.push(inspect_codex_scope(
                ScopeKind::CodexAncestor,
                &ancestor.join(AUTHORITATIVE),
            )?);
            scopes.push(inspect_codex_scope(
                ScopeKind::CodexAncestorCompatibility,
                &ancestor.join(COMPATIBILITY),
            )?);
        }
        Ok(scopes)
    }
}

/// Mirrors Codex's recursive `**/SKILL.md` discovery while retaining immediate path occupancy.
fn inspect_codex_scope(kind: ScopeKind, entry: &Path) -> Result<DiscoveryScope, AppError> {
    let mut scope = inspect_scope(kind, entry)?;
    scope.existing_skills.clear();
    // Direct filename collisions remain physical occupancy through `direct_entries`, but they are
    // not logical Codex Skill collisions until valid frontmatter declares the same name.
    scope.warnings.clear();
    if !matches!(
        scope.state.kind,
        PathKind::Directory | PathKind::DirectoryLink
    ) {
        return Ok(scope);
    }

    let mut pending = vec![entry.to_path_buf()];
    let mut visited = BTreeSet::new();
    while let Some(directory) = pending.pop() {
        let state = classify(&directory)?;
        if !matches!(state.kind, PathKind::Directory | PathKind::DirectoryLink) {
            continue;
        }
        let terminal = state.terminal.clone().ok_or_else(|| {
            AppError::Internal(
                "a usable Codex discovery directory must expose a terminal path".to_owned(),
            )
        })?;
        if !visited.insert(terminal.clone()) {
            continue;
        }
        if visited.len() > MAX_DISCOVERY_DIRECTORIES {
            return Err(PlanError::UnsupportedLayout {
                path: entry.to_path_buf(),
                reason: format!(
                    "recursive Codex discovery exceeds {MAX_DISCOVERY_DIRECTORIES} distinct directories"
                ),
            }
            .into());
        }

        let skill_md = directory.join("SKILL.md");
        match fs::symlink_metadata(&skill_md) {
            Ok(_) => match crate::catalog::frontmatter::metadata(&skill_md) {
                Ok(metadata) => {
                    let name = metadata.name.filter(|name| !name.trim().is_empty());
                    let description = metadata
                        .description
                        .filter(|description| !description.trim().is_empty());
                    if let (Some(name), Some(_)) = (name, description) {
                        super::insert_deterministically(
                            &mut scope,
                            crate::agent::ExistingSkill {
                                comparison_key: SkillNameKey::new(OsStr::new(&name)),
                                raw_name: OsString::from(name),
                                entry: directory.clone(),
                                kind: state.kind,
                                source_canonical: Some(terminal.clone()),
                            },
                            DiagnosticKind::CodexDiscovery,
                        );
                    } else {
                        scope.warnings.push(Diagnostic::warning_with_kind(
                            DiagnosticKind::CodexDiscovery,
                            "Codex Skill metadata requires non-empty name and description fields",
                            skill_md,
                        ));
                    }
                }
                Err(reason) => scope.warnings.push(Diagnostic::warning_with_kind(
                    DiagnosticKind::CodexDiscovery,
                    format!("Codex will not load this malformed SKILL.md: {reason}"),
                    skill_md,
                )),
            },
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
            Err(error) => {
                return Err(AppError::MissingInput {
                    path: skill_md,
                    reason: error.to_string(),
                });
            }
        }

        let entries = fs::read_dir(&directory).map_err(|error| AppError::MissingInput {
            path: directory.clone(),
            reason: error.to_string(),
        })?;
        let mut children = Vec::new();
        for child in entries {
            let child = child.map_err(|error| AppError::MissingInput {
                path: directory.clone(),
                reason: error.to_string(),
            })?;
            let child_path = child.path();
            let child_state = classify(&child_path)?;
            if matches!(
                child_state.kind,
                PathKind::Directory | PathKind::DirectoryLink
            ) {
                children.push(child_path);
            }
        }
        children.sort();
        pending.extend(children.into_iter().rev());
    }

    Ok(scope)
}

impl AgentAdapter for CodexAdapter {
    fn id(&self) -> AgentId {
        AgentId::Codex
    }

    fn default_executable(&self) -> &'static OsStr {
        OsStr::new("codex")
    }

    fn validate_passthrough_args(&self, _args: &[OsString]) -> Result<Vec<Diagnostic>, AppError> {
        // Codex exposes no documented switch that disables Skill discovery, so nothing is
        // rejected. The launch CWD is set on the child process instead of injecting `-C`, which
        // keeps SkillMount from colliding with a user-supplied Codex argument.
        Ok(Vec::new())
    }

    fn catalog_diagnostics(&self, context: &RunContext, catalog: &SkillCatalog) -> Vec<Diagnostic> {
        catalog
            .resolutions
            .iter()
            .filter(|resolution| {
                !resolution
                    .selected
                    .origin
                    .source_canonical
                    .starts_with(&context.project_root)
            })
            .map(|resolution| {
                let skill = &resolution.selected;
                let mut diagnostic = Diagnostic::warning_with_kind(
                    DiagnosticKind::CodexPermissionSeparation,
                    format!(
                        "Codex can discover linked Skill {}, but discovery does not grant sandbox access to {}; if bundled files are denied, give this path explicit read access in a Codex permission profile. SkillMount does not change permissions or inject --add-dir",
                        skill.mount_name,
                        skill.origin.source_canonical.display()
                    ),
                    skill.origin.source_canonical.clone(),
                );
                diagnostic.source_ordinal = Some(skill.origin.source_ordinal);
                diagnostic
            })
            .collect()
    }

    fn inspect_discovery(&self, context: &RunContext) -> Result<DiscoverySnapshot, AppError> {
        let authoritative_path = Self::authoritative_entry(context);
        let compatibility_path = Self::compatibility_entry(context);
        let authoritative = classify(&authoritative_path)?;
        let compatibility = classify(&compatibility_path)?;
        let backing = resolve_backing(&context.project_root, &authoritative, &compatibility)?;

        let mut scopes = vec![
            inspect_codex_scope(ScopeKind::CodexAuthoritative, &authoritative_path)?,
            inspect_codex_scope(ScopeKind::CodexCompatibility, &compatibility_path)?,
        ];
        scopes.extend(Self::ancestor_scopes(context)?);
        let mut lock_resources = scopes
            .iter()
            .map(|scope| {
                LockResource::describe_entry(
                    LockResourceKind::DiscoveryEntry,
                    &context.project_root,
                    &scope.state,
                )
            })
            .collect::<Result<Vec<_>, _>>()?;
        lock_resources.push(LockResource::describe(
            LockResourceKind::BackingStore,
            &context.project_root,
            &backing.store,
        )?);
        lock_resources.sort_by_key(LockResource::ordering_key);
        lock_resources.dedup();

        let mut warnings = backing.warnings;
        // An authoritative entry that links to the compatibility store makes both scopes the same
        // physical directory. Keeping both would make the store's own contents look like a foreign
        // cross-scope Skill and turn every mount into a spurious reuse.
        scopes = dedupe_scopes_by_terminal(scopes, &backing.store);

        for scope in &scopes {
            warnings.extend(scope.warnings.iter().cloned());
        }

        Ok(DiscoverySnapshot {
            agent: AgentId::Codex,
            scopes,
            discovery_entry: authoritative_path,
            backing_store: backing.store,
            backing_store_state: backing.store_state,
            lock_resources,
            warnings,
        })
    }

    fn build_mount_plan(
        &self,
        context: &RunContext,
        catalog: &SkillCatalog,
        discovery: &DiscoverySnapshot,
    ) -> Result<MountPlan, AppError> {
        let authoritative = classify(&discovery.discovery_entry)?;
        let compatibility = classify(&Self::compatibility_entry(context))?;
        let backing = resolve_backing(&context.project_root, &authoritative, &compatibility)?;

        let mut actions = ActionSequence::default();
        // Dependency order: parents, then the store, then the authoritative link, then Skills.
        for directory in &backing.create_directories {
            actions.push(
                MountAction::CreateDirectory {
                    path: directory.clone(),
                },
                PathPrecondition::Missing,
            );
        }
        if let Some(target) = &backing.authoritative_link_target {
            actions.push(
                MountAction::CreateDirectoryLink {
                    source: target.clone(),
                    destination: discovery.discovery_entry.clone(),
                    mode: context.options.link_mode,
                },
                PathPrecondition::Missing,
            );
        }

        let mut preserved = Vec::new();
        apply_conflict_policy(context, catalog, discovery, &mut actions, &mut preserved)?;

        Ok(MountPlan {
            agent: AgentId::Codex,
            discovery: DiscoveryPlan {
                entry: discovery.discovery_entry.clone(),
                backing_store: discovery.backing_store.clone(),
            },
            actions: actions.into_actions(),
            preserved,
            launch: LaunchPlan {
                executable: context.agent_bin.clone(),
                cwd: context.launch_cwd.clone(),
                injected_args: Vec::new(),
                passthrough_args: context.passthrough_args.clone(),
            },
        })
    }
}
