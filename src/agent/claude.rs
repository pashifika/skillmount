//! Claude Code adapter: isolated session staging surfaced through `--add-dir`.

use std::ffi::{OsStr, OsString};
use std::path::{Path, PathBuf};

use crate::agent::{
    AgentAdapter, DiscoverySnapshot, ScopeKind, dedupe_scopes_by_terminal, inspect_scope,
};
use crate::diagnostic::Diagnostic;
use crate::domain::{AgentId, MountMode, RunContext, SkillCatalog};
use crate::error::AppError;
use crate::lock::{LockResource, LockResourceKind};
use crate::mount::plan::apply_conflict_policy;
use crate::mount::resolve::classify;
use crate::mount::{
    ActionSequence, DiscoveryPlan, LaunchPlan, MountAction, MountPlan, PathPrecondition,
};
use crate::state::{PENDING_SESSION, session_root_base, user_home};

/// Namespace Claude Code searches inside any directory it is given.
const SKILLS_SUFFIX: &str = ".claude/skills";

/// Passthrough arguments that switch off Skill loading entirely.
///
/// Mounting Skills and then starting an agent that ignores them wastes the mount and misleads the
/// operator, so these fail before anything is planned. A future `--allow-agent-conflicts` option
/// can downgrade them to warnings.
const SKILL_DISABLING_ARGS: [&str; 3] = ["--bare", "--safe-mode", "--disable-slash-commands"];

/// The Claude Code agent adapter.
#[derive(Debug, Clone, Copy, Default)]
pub struct ClaudeAdapter;

/// Where a session would stage its Skills, described without creating anything.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct StagingLayout {
    /// Session directory that would also hold the transaction journal.
    pub(crate) session: PathBuf,
    /// Directory handed to Claude through `--add-dir`.
    pub(crate) add_dir_root: PathBuf,
    /// Namespace inside the staging root that receives one link per Skill.
    pub(crate) skills: PathBuf,
}

impl StagingLayout {
    /// Computes the layout a session would stage into.
    ///
    /// A run that has not minted an identifier yet uses [`PENDING_SESSION`], which is deliberately
    /// unusable as a real directory name on Windows: a preliminary plan must be recognisable as
    /// one, and never accidentally applied. A mutating run replans with its real identifier before
    /// anything is created, which is also what puts two concurrent sessions in separate roots.
    ///
    /// # Errors
    ///
    /// Returns [`AppError::MissingInput`] when the platform state location cannot be resolved.
    pub(crate) fn for_context(context: &RunContext) -> Result<Self, AppError> {
        let component = context
            .session_id
            .as_ref()
            .map_or_else(|| PENDING_SESSION.to_owned(), ToString::to_string);
        let session = session_root_base()?.join(component);
        let add_dir_root = session.join("root");
        let skills = add_dir_root.join(SKILLS_SUFFIX);
        Ok(Self {
            session,
            add_dir_root,
            skills,
        })
    }
}

/// Returns every ancestor of `leaf` beneath `boundary` that does not exist yet, parents first.
///
/// The boundary is load-bearing, not a tidiness measure. Everything at or above it is shared by
/// every session — the state root and its `sessions` directory, or the project root — and a shared
/// directory must never become a transaction action. Two concurrent sessions would then each plan
/// to create it, the second would find it occupied, and a precondition check would fail a run that
/// is doing nothing wrong. Shared storage is created idempotently outside the transaction instead;
/// the journal and lock directories are handled the same way.
fn missing_directory_chain(leaf: &Path, boundary: &Path) -> Vec<PathBuf> {
    let mut missing = leaf
        .ancestors()
        .take_while(|ancestor| {
            !ancestor.as_os_str().is_empty() && *ancestor != boundary && !ancestor.exists()
        })
        .map(Path::to_path_buf)
        .collect::<Vec<_>>();
    missing.reverse();
    missing
}

/// Extracts the directories a passthrough `--add-dir` makes visible to the child.
///
/// Both `--add-dir=VALUE` and `--add-dir VALUE...` are recognised. The separated form consumes
/// following values until the next option, because Claude Code accepts several directories per
/// occurrence. Over-collecting is the safe direction: an extra inspected scope can only add a
/// conflict check, never remove one.
pub(crate) fn parse_add_dirs(args: &[OsString]) -> Vec<PathBuf> {
    let flag = OsStr::new("--add-dir");
    let mut directories = Vec::new();
    let mut index = 0;
    while index < args.len() {
        let argument = args[index].as_os_str();
        if let Some(value) = strip_prefix(argument, "--add-dir=") {
            if !value.is_empty() {
                directories.push(PathBuf::from(value));
            }
            index += 1;
            continue;
        }
        if argument != flag {
            index += 1;
            continue;
        }
        index += 1;
        while index < args.len() && strip_prefix(args[index].as_os_str(), "-").is_none() {
            directories.push(PathBuf::from(&args[index]));
            index += 1;
        }
    }
    directories
}

/// Returns the remainder of `value` after `prefix`, preserving platform-native encoding.
#[cfg(unix)]
fn strip_prefix(value: &OsStr, prefix: &str) -> Option<OsString> {
    use std::os::unix::ffi::{OsStrExt, OsStringExt};

    value
        .as_bytes()
        .strip_prefix(prefix.as_bytes())
        .map(|rest| OsString::from_vec(rest.to_vec()))
}

/// Returns the remainder of `value` after `prefix`, preserving platform-native encoding.
#[cfg(windows)]
fn strip_prefix(value: &OsStr, prefix: &str) -> Option<OsString> {
    use std::os::windows::ffi::{OsStrExt, OsStringExt};

    let units = value.encode_wide().collect::<Vec<_>>();
    let expected = prefix.encode_utf16().collect::<Vec<_>>();
    units
        .strip_prefix(expected.as_slice())
        .map(OsString::from_wide)
}

impl ClaudeAdapter {
    /// Returns the namespace selected Skills are written into, and the layout backing it.
    fn destination(
        context: &RunContext,
    ) -> Result<(PathBuf, PathBuf, Option<StagingLayout>), AppError> {
        match context.options.mount_mode {
            MountMode::Staging => {
                let layout = StagingLayout::for_context(context)?;
                Ok((
                    layout.add_dir_root.clone(),
                    layout.skills.clone(),
                    Some(layout),
                ))
            }
            // Explicitly requested project mounting. The default never reaches this branch, so the
            // project's own `.claude/skills` stays untouched unless the operator opts in.
            MountMode::Project => {
                let skills = context.project_root.join(SKILLS_SUFFIX);
                Ok((context.project_root.clone(), skills, None))
            }
        }
    }
}

impl AgentAdapter for ClaudeAdapter {
    fn id(&self) -> AgentId {
        AgentId::Claude
    }

    fn default_executable(&self) -> &'static OsStr {
        OsStr::new("claude")
    }

    fn validate_passthrough_args(&self, args: &[OsString]) -> Result<Vec<Diagnostic>, AppError> {
        for argument in args {
            for rejected in SKILL_DISABLING_ARGS {
                if argument == OsStr::new(rejected) {
                    return Err(AppError::Usage(format!(
                        "{rejected} disables Claude Code Skill loading, so mounted Skills would be ignored; remove it or run the agent directly"
                    )));
                }
            }
        }
        Ok(Vec::new())
    }

    fn inspect_discovery(&self, context: &RunContext) -> Result<DiscoverySnapshot, AppError> {
        let (discovery_entry, backing_store, layout) = Self::destination(context)?;
        let backing_state = classify(&backing_store)?;

        let mut scopes = vec![match context.options.mount_mode {
            MountMode::Staging => inspect_scope(ScopeKind::ClaudeStaging, &backing_store)?,
            MountMode::Project => inspect_scope(ScopeKind::ClaudeProject, &backing_store)?,
        }];
        if context.options.mount_mode == MountMode::Staging {
            scopes.push(inspect_scope(
                ScopeKind::ClaudeProject,
                &context.project_root.join(SKILLS_SUFFIX),
            )?);
        }
        scopes.push(inspect_scope(
            ScopeKind::ClaudeUser,
            &user_home()?.join(SKILLS_SUFFIX),
        )?);
        for directory in parse_add_dirs(&context.passthrough_args) {
            scopes.push(inspect_scope(
                ScopeKind::ClaudeAddDir,
                &directory.join(SKILLS_SUFFIX),
            )?);
        }
        let mut warnings = Vec::new();
        scopes = dedupe_scopes_by_terminal(scopes, &backing_store);

        // A staging root is addressed by a session identifier no other process shares, so it needs
        // no anchor that stays stable across runs. Project mode writes into a directory other runs
        // also address, so its anchor must be the project root, which no plan ever creates.
        let mut lock_resources = if let Some(layout) = &layout {
            vec![
                LockResource::describe_unanchored(
                    LockResourceKind::DiscoveryEntry,
                    &layout.add_dir_root,
                ),
                LockResource::describe_unanchored(LockResourceKind::BackingStore, &layout.skills),
            ]
        } else {
            vec![
                LockResource::describe(
                    LockResourceKind::DiscoveryEntry,
                    &context.project_root,
                    &discovery_entry,
                )?,
                LockResource::describe(
                    LockResourceKind::BackingStore,
                    &context.project_root,
                    &backing_store,
                )?,
            ]
        };
        lock_resources.sort_by_key(LockResource::ordering_key);
        lock_resources.dedup();

        for scope in &scopes {
            warnings.extend(scope.warnings.iter().cloned());
        }

        Ok(DiscoverySnapshot {
            agent: AgentId::Claude,
            scopes,
            discovery_entry,
            backing_store,
            backing_store_state: backing_state.kind,
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
        // Directories at or above this boundary belong to every session, so they are never planned.
        let boundary = match context.options.mount_mode {
            MountMode::Staging => session_root_base()?,
            MountMode::Project => context.project_root.clone(),
        };
        let mut actions = ActionSequence::default();
        for directory in missing_directory_chain(&discovery.backing_store, &boundary) {
            actions.push(
                MountAction::CreateDirectory { path: directory },
                PathPrecondition::Missing,
            );
        }

        let mut preserved = Vec::new();
        apply_conflict_policy(context, catalog, discovery, &mut actions, &mut preserved)?;

        let injected_args = match context.options.mount_mode {
            MountMode::Staging => vec![
                OsString::from("--add-dir"),
                discovery.discovery_entry.clone().into_os_string(),
            ],
            MountMode::Project => Vec::new(),
        };

        Ok(MountPlan {
            agent: AgentId::Claude,
            discovery: DiscoveryPlan {
                entry: discovery.discovery_entry.clone(),
                backing_store: discovery.backing_store.clone(),
            },
            actions: actions.into_actions(),
            preserved,
            launch: LaunchPlan {
                executable: context.agent_bin.clone(),
                cwd: context.launch_cwd.clone(),
                injected_args,
                passthrough_args: context.passthrough_args.clone(),
            },
        })
    }
}

#[cfg(test)]
mod tests {
    use super::{ClaudeAdapter, missing_directory_chain, parse_add_dirs};
    use crate::agent::AgentAdapter;
    use std::ffi::OsString;
    use std::path::PathBuf;

    fn args(values: &[&str]) -> Vec<OsString> {
        values.iter().map(OsString::from).collect()
    }

    #[test]
    fn add_dir_is_recognised_in_both_argument_forms() {
        assert_eq!(
            parse_add_dirs(&args(&["--add-dir=/a", "--add-dir", "/b", "/c", "--other"])),
            [
                PathBuf::from("/a"),
                PathBuf::from("/b"),
                PathBuf::from("/c")
            ]
        );
    }

    #[test]
    fn unrelated_arguments_contribute_no_scopes() {
        assert!(parse_add_dirs(&args(&["--model", "opus", "--verbose"])).is_empty());
    }

    #[test]
    fn skill_disabling_arguments_fail_before_planning() {
        for rejected in ["--bare", "--safe-mode", "--disable-slash-commands"] {
            let error = ClaudeAdapter
                .validate_passthrough_args(&args(&[rejected]))
                .expect_err("Skill-disabling arguments must be rejected");
            assert_eq!(error.category(), crate::error::ExitCategory::Usage);
        }
    }

    #[test]
    fn ordinary_arguments_are_forwarded_without_complaint() {
        let diagnostics = ClaudeAdapter
            .validate_passthrough_args(&args(&["--model", "opus", "--add-dir", "/tmp"]))
            .expect("ordinary arguments are accepted");
        assert!(diagnostics.is_empty());
    }

    #[test]
    fn only_absent_ancestors_are_planned_for_creation() {
        let existing = std::env::temp_dir();
        let leaf = existing.join("skillmount-absent-a/b/c");

        let chain = missing_directory_chain(&leaf, &existing);

        assert_eq!(
            chain,
            [
                existing.join("skillmount-absent-a"),
                existing.join("skillmount-absent-a/b"),
                leaf
            ],
            "parents must precede children so the chain applies in order"
        );
        assert!(missing_directory_chain(&existing, &existing).is_empty());
    }

    #[test]
    fn nothing_at_or_above_the_shared_boundary_is_ever_planned() {
        // A staging root whose shared parents do not exist yet. Planning them would make two
        // concurrent sessions each claim the same `sessions` directory, and the loser would fail a
        // precondition check for doing nothing wrong.
        let base = std::env::temp_dir().join("skillmount-shared-base/sessions");
        let leaf = base.join("session-id/root/.claude/skills");

        let chain = missing_directory_chain(&leaf, &base);

        assert_eq!(
            chain,
            [
                base.join("session-id"),
                base.join("session-id/root"),
                base.join("session-id/root/.claude"),
                leaf
            ],
            "only directories inside this session's own root may be planned"
        );
    }
}
