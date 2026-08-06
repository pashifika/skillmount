//! Claude Code adapter: isolated session staging surfaced through `--add-dir`.

use std::collections::BTreeMap;
use std::ffi::{OsStr, OsString};
use std::path::{Path, PathBuf};

use crate::agent::version::VersionSpec;
use crate::agent::{
    AgentAdapter, DiscoverySnapshot, ScopeKind, dedupe_scopes_by_terminal, discovery_indexes,
    inspect_scope,
};
use crate::diagnostic::{Diagnostic, DiagnosticKind};
use crate::domain::{AgentId, CatalogPolicy, MountMode, RunContext, SkillCatalog};
use crate::error::AppError;
use crate::lock::{LockResource, LockResourceKind};
use crate::mount::plan::apply_conflict_policy;
use crate::mount::resolve::classify;
use crate::mount::{
    ActionSequence, DiscoveryPlan, LaunchPlan, MountAction, MountPlan, PathPrecondition,
};
use crate::state::{PENDING_SESSION, session_root_base};

/// Namespace Claude Code searches inside any directory it is given.
const SKILLS_SUFFIX: &str = ".claude/skills";

/// Claude Code banner attached to the adapter's last-tested discovery evidence.
const LAST_TESTED_CLAUDE_BANNER: &str = "2.1.220 (Claude Code)";
const CLAUDE_VERSION_SPEC: VersionSpec = VersionSpec::new(
    "Claude Code",
    LAST_TESTED_CLAUDE_BANNER,
    "SKILLMOUNT_TEST_CLAUDE_VERSION",
);

/// Passthrough arguments that change the normal Skill discovery model.
///
/// Mounting Skills and then starting an agent under another discovery mode can waste the mount and
/// mislead the operator, so these fail before anything is planned. A future
/// `--allow-agent-conflicts` option can downgrade them to warnings.
const SKILL_DISABLING_ARGS: [&str; 3] = ["--bare", "--safe-mode", "--disable-slash-commands"];

/// Passthrough settings inputs that could undo `SkillMount`'s session visibility override.
const SKILL_VISIBILITY_ARGS: [&str; 3] = ["--managed-settings", "--setting-sources", "--settings"];

/// Passthrough controls that detach the logical session or relocate its discovery root.
const SESSION_BOUNDARY_ARGS: [&str; 5] = ["--background", "--bg", "--tmux", "--worktree", "-w"];

/// Last-tested commands, plus implementation-time additions observed in Claude Code 2.1.222, that
/// do not start a supervised foreground session.
///
/// Claude selects a command from the first unconsumed positional argument, including after a
/// standalone `--`. Forwarding one of these commands would hand the staged root to an operator or
/// service process whose lifetime and discovery behavior are outside this adapter's contract.
const NON_SESSION_SUBCOMMANDS: [&str; 15] = [
    "agents",
    "auth",
    "auto-mode",
    "doctor",
    "gateway",
    "install",
    "import",
    "mcp",
    "plugin",
    "plugins",
    "project",
    "setup-token",
    "ultrareview",
    "update",
    "upgrade",
];

/// Last-tested options, plus implementation-time additions observed in Claude Code 2.1.222, that
/// consume exactly one following value.
///
/// This table exists only to keep flag-shaped values opaque while locating Skill discovery
/// controls. It is not a second Claude CLI parser and deliberately has no validation policy for
/// these options.
const SINGLE_VALUE_ARGS: [&str; 23] = [
    "--agent",
    "--agents",
    "--autocompact",
    "--append-system-prompt",
    "--debug-file",
    "--effort",
    "--fallback-model",
    "--input-format",
    "--json-schema",
    "--max-budget-usd",
    "--managed-settings",
    "--model",
    "--name",
    "-n",
    "--output-format",
    "--permission-mode",
    "--plugin-dir",
    "--plugin-url",
    "--remote-control-session-name-prefix",
    "--session-id",
    "--setting-sources",
    "--settings",
    "--system-prompt",
];

/// Pinned options whose value is optional and never consumes the next option token.
const OPTIONAL_VALUE_ARGS: [&str; 9] = [
    "--debug",
    "-d",
    "--from-pr",
    "--prompt-suggestions",
    "--remote-control",
    "--resume",
    "-r",
    "--worktree",
    "-w",
];

/// Pinned options that consume non-option values until the next option token.
const VARIADIC_VALUE_ARGS: [&str; 9] = [
    "--allowedTools",
    "--allowed-tools",
    "--betas",
    "--disallowedTools",
    "--disallowed-tools",
    "--file",
    "--mcp-config",
    "--tools",
    "--add-dir",
];

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
    /// any transaction-owned staging entry is created, which is also what puts two concurrent
    /// sessions in separate roots.
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

/// Extracts the raw directory values named by passthrough `--add-dir` arguments.
///
/// Both `--add-dir=VALUE` and `--add-dir VALUE...` are recognised. The separated form consumes
/// following values until the next option, because Claude Code accepts several directories per
/// occurrence. Over-collecting is the safe direction: an extra inspected scope can only add a
/// conflict check, never remove one.
pub(crate) fn parse_add_dirs(args: &[OsString]) -> Result<Vec<PathBuf>, AppError> {
    scan_passthrough(args).map(|scan| scan.add_dirs)
}

#[derive(Debug, Default)]
struct PassthroughScan {
    add_dirs: Vec<PathBuf>,
    disabling_arg: Option<&'static str>,
    visibility_arg: Option<&'static str>,
    session_boundary_arg: Option<&'static str>,
    non_session_subcommand: Option<&'static str>,
}

fn scan_passthrough(args: &[OsString]) -> Result<PassthroughScan, AppError> {
    let mut scan = PassthroughScan::default();
    let mut directories = Vec::new();
    let mut index = 0;
    let mut command_position_open = true;
    while index < args.len() {
        let argument = args[index].as_os_str();
        if argument == OsStr::new("--") {
            if command_position_open {
                scan.non_session_subcommand = args
                    .get(index + 1)
                    .and_then(|argument| non_session_subcommand(argument));
            }
            break;
        }
        if let Some(rejected) = SKILL_DISABLING_ARGS
            .iter()
            .find(|candidate| argument == OsStr::new(candidate))
        {
            scan.disabling_arg = Some(rejected);
            break;
        }
        if let Some(rejected) = SKILL_VISIBILITY_ARGS.iter().find(|candidate| {
            argument == OsStr::new(candidate)
                || strip_prefix(argument, &format!("{candidate}=")).is_some()
        }) {
            scan.visibility_arg = Some(rejected);
            break;
        }
        if let Some(rejected) = session_boundary_arg(argument) {
            scan.session_boundary_arg = Some(rejected);
            break;
        }
        if let Some(value) = strip_prefix(argument, "--add-dir=") {
            if value.is_empty() {
                return Err(AppError::Usage(
                    "Claude Code --add-dir requires a non-empty directory value".to_owned(),
                ));
            }
            directories.push(PathBuf::from(value));
            index += 1;
            continue;
        }
        if contains_os(&SINGLE_VALUE_ARGS, argument) {
            index = index.saturating_add(2);
            continue;
        }
        if contains_os(&OPTIONAL_VALUE_ARGS, argument) {
            index += 1;
            if index < args.len() && !is_option(&args[index]) {
                index += 1;
            }
            continue;
        }
        if !contains_os(&VARIADIC_VALUE_ARGS, argument) {
            if command_position_open && !is_option(argument) {
                if let Some(rejected) = non_session_subcommand(argument) {
                    scan.non_session_subcommand = Some(rejected);
                    break;
                }
                command_position_open = false;
            }
            index += 1;
            continue;
        }
        index += 1;
        let value_start = index;
        while index < args.len() && !is_option(&args[index]) {
            if argument == OsStr::new("--add-dir") {
                directories.push(PathBuf::from(&args[index]));
            }
            index += 1;
        }
        if argument == OsStr::new("--add-dir") && index == value_start {
            return Err(AppError::Usage(
                "Claude Code --add-dir requires at least one directory value".to_owned(),
            ));
        }
    }
    scan.add_dirs = directories;
    Ok(scan)
}

fn contains_os(values: &[&str], candidate: &OsStr) -> bool {
    values.iter().any(|value| candidate == OsStr::new(value))
}

fn session_boundary_arg(argument: &OsStr) -> Option<&'static str> {
    SESSION_BOUNDARY_ARGS
        .iter()
        .find(|candidate| argument == OsStr::new(candidate))
        .copied()
        .or_else(|| strip_prefix(argument, "--worktree=").map(|_| "--worktree"))
        .or_else(|| strip_prefix(argument, "--tmux=").map(|_| "--tmux"))
        .or_else(|| {
            strip_prefix(argument, "-w")
                .filter(|value| !value.is_empty())
                .map(|_| "-w")
        })
}

fn non_session_subcommand(argument: &OsStr) -> Option<&'static str> {
    NON_SESSION_SUBCOMMANDS
        .iter()
        .find(|candidate| argument == OsStr::new(candidate))
        .copied()
}

fn is_option(value: &OsStr) -> bool {
    strip_prefix(value, "-").is_some()
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

    /// Collects every unqualified project scope the pinned Claude release reads at startup.
    fn project_scopes(context: &RunContext) -> Result<Vec<crate::agent::DiscoveryScope>, AppError> {
        let mut scopes = vec![inspect_claude_scope(
            ScopeKind::ClaudeProject,
            &context.project_root.join(SKILLS_SUFFIX),
        )?];
        for ancestor in context.launch_cwd.ancestors() {
            if ancestor == context.project_root {
                break;
            }
            if !ancestor.starts_with(&context.project_root) {
                break;
            }
            scopes.push(inspect_claude_scope(
                ScopeKind::ClaudeAncestor,
                &ancestor.join(SKILLS_SUFFIX),
            )?);
        }
        Ok(scopes)
    }
}

fn inspect_claude_scope(
    kind: ScopeKind,
    entry: &Path,
) -> Result<crate::agent::DiscoveryScope, AppError> {
    let mut scope = inspect_scope(kind, entry)?;
    for warning in &mut scope.warnings {
        warning.kind = DiagnosticKind::ClaudeDiscovery;
    }
    Ok(scope)
}

/// Verifies the Claude launch invariants that remain mandatory for every observed release.
fn verify_launch_invariants() -> Result<(), AppError> {
    verify_environment()
}

/// Verifies environment switches that change the discovery model this adapter inspected.
fn verify_environment() -> Result<(), AppError> {
    // Debug builds expose a filesystem marker so the real-process transaction suite can introduce
    // a discovery-changing control after apply. Release builds contain no such lookup.
    #[cfg(debug_assertions)]
    if let Some(path) =
        std::env::var_os("SKILLMOUNT_TEST_CLAUDE_ENVIRONMENT_CONTROL_PATH").map(PathBuf::from)
    {
        match std::fs::symlink_metadata(&path) {
            Ok(_) => {
                return Err(AppError::Usage(
                    "the deterministic Claude environment-control marker changes normal Skill discovery"
                        .to_owned(),
                ));
            }
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
            Err(error) => {
                return Err(AppError::Usage(format!(
                    "cannot inspect the deterministic Claude environment-control marker {}: {error}",
                    path.display()
                )));
            }
        }
    }

    if env_flag_enabled(std::env::var_os("CLAUDE_CODE_SAFE_MODE").as_deref()) {
        return Err(AppError::Usage(
            "CLAUDE_CODE_SAFE_MODE changes Claude Code's normal Skill discovery; unset it or run the agent directly"
                .to_owned(),
        ));
    }
    if env_flag_enabled(std::env::var_os("CLAUDE_CODE_SIMPLE").as_deref()) {
        return Err(AppError::Usage(
            "CLAUDE_CODE_SIMPLE changes Claude Code's normal Skill discovery; unset it or run the agent directly"
                .to_owned(),
        ));
    }

    Ok(())
}

/// Returns the dated version evidence used by the shared advisory observer.
const fn version_spec() -> VersionSpec {
    CLAUDE_VERSION_SPEC
}

fn env_flag_enabled(value: Option<&OsStr>) -> bool {
    value.is_some_and(|value| {
        !value.is_empty() && value != OsStr::new("0") && !value.eq_ignore_ascii_case("false")
    })
}

impl AgentAdapter for ClaudeAdapter {
    fn version_spec(&self) -> VersionSpec {
        version_spec()
    }

    fn catalog_policy(&self) -> CatalogPolicy {
        // Claude Code loads a direct `SKILL.md` entry and falls back to the directory name, so it
        // needs neither an exact entry name nor a frontmatter name at basic validation. A present
        // name must still address the mounted directory, and a description is still required.
        CatalogPolicy {
            requires_exact_skill_md_entry: false,
            always_parses_metadata: false,
            requires_name: false,
            requires_description: true,
            requires_matching_name: true,
        }
    }

    fn destination_stores(&self, context: &RunContext) -> Vec<PathBuf> {
        // A staging root lives under SkillMount's own state, never inside a selected source, so
        // only an explicit project mount contributes a cycle candidate.
        match context.options.mount_mode {
            MountMode::Project => vec![context.project_root.join(SKILLS_SUFFIX)],
            MountMode::Staging => Vec::new(),
        }
    }

    fn validate_launch_invariants(&self, _context: &RunContext) -> Result<(), AppError> {
        verify_launch_invariants()
    }

    fn validate_passthrough_args(&self, args: &[OsString]) -> Result<Vec<Diagnostic>, AppError> {
        let scan = scan_passthrough(args)?;
        if let Some(rejected) = scan.disabling_arg {
            return Err(AppError::Usage(format!(
                "{rejected} changes Claude Code's normal Skill discovery, so mounted Skills would not satisfy the session contract; remove it or run the agent directly"
            )));
        }
        if let Some(rejected) = scan.visibility_arg {
            return Err(AppError::Usage(format!(
                "{rejected} can override selected Skill visibility after SkillMount plans the session; remove it or run the agent directly"
            )));
        }
        if let Some(rejected) = scan.session_boundary_arg {
            return Err(AppError::Usage(format!(
                "{rejected} detaches Claude Code or relocates its discovery root outside SkillMount's supervised session contract; remove it or run the agent directly"
            )));
        }
        if let Some(rejected) = scan.non_session_subcommand {
            return Err(AppError::Usage(format!(
                "Claude Code subcommand {rejected:?} does not start a supervised foreground session; run that subcommand directly"
            )));
        }
        Ok(Vec::new())
    }

    fn inspect_discovery(&self, context: &RunContext) -> Result<DiscoverySnapshot, AppError> {
        let claude = context.agent.claude()?;
        let (discovery_entry, backing_store, layout) = Self::destination(context)?;
        let backing_state = classify(&backing_store)?;

        let mut scopes = vec![match context.options.mount_mode {
            MountMode::Staging => inspect_claude_scope(ScopeKind::ClaudeStaging, &backing_store)?,
            MountMode::Project => inspect_claude_scope(ScopeKind::ClaudeProject, &backing_store)?,
        }];
        if context.options.mount_mode == MountMode::Staging {
            scopes.extend(Self::project_scopes(context)?);
        } else {
            scopes.extend(
                Self::project_scopes(context)?
                    .into_iter()
                    .filter(|scope| scope.state.entry != backing_store),
            );
        }
        scopes.push(inspect_claude_scope(
            ScopeKind::ClaudeManaged,
            &claude.managed_skills,
        )?);
        scopes.push(inspect_claude_scope(
            ScopeKind::ClaudeUser,
            &claude.config_dir.join("skills"),
        )?);
        for directory in parse_add_dirs(&context.passthrough_args)? {
            let directory = crate::paths::absolute_from(&context.launch_cwd, &directory)?;
            scopes.push(inspect_claude_scope(
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
        let (visible_skills, mount_entries) = discovery_indexes(&scopes, &backing_store);

        Ok(DiscoverySnapshot {
            agent: AgentId::Claude,
            scopes,
            visible_skills,
            mount_entries,
            discovery_entry,
            backing_store_canonical: super::canonical_backing(&backing_store, &backing_state),
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

        let mut injected_args = match context.options.mount_mode {
            MountMode::Staging => vec![
                OsString::from("--add-dir"),
                discovery.discovery_entry.clone().into_os_string(),
            ],
            MountMode::Project => Vec::new(),
        };
        let overrides = catalog
            .resolutions
            .iter()
            .map(|resolution| (resolution.selected.mount_name.as_str(), "on"))
            .collect::<BTreeMap<_, _>>();
        if !overrides.is_empty() {
            let settings = serde_json::to_string(&serde_json::json!({
                "skillOverrides": overrides,
            }))
            .map_err(|error| {
                AppError::Usage(format!(
                    "cannot serialize Claude Code Skill visibility settings: {error}"
                ))
            })?;
            injected_args.push(OsString::from("--settings"));
            injected_args.push(OsString::from(settings));
        }

        Ok(MountPlan {
            agent: AgentId::Claude,
            discovery: DiscoveryPlan {
                entry: discovery.discovery_entry.clone(),
                backing_store: discovery.backing_store.clone(),
            },
            actions: actions.into_actions(),
            preserved,
            launch: LaunchPlan {
                executable: context.executable().to_path_buf(),
                cwd: context.launch_cwd.clone(),
                injected_args,
                passthrough_args: context.passthrough_args.clone(),
                environment_overrides: Vec::new(),
            },
        })
    }
}

#[cfg(test)]
mod tests {
    use super::{
        ClaudeAdapter, LAST_TESTED_CLAUDE_BANNER, env_flag_enabled, missing_directory_chain,
        parse_add_dirs, version_spec,
    };
    use crate::agent::AgentAdapter;
    use std::ffi::OsString;
    use std::path::PathBuf;

    fn args(values: &[&str]) -> Vec<OsString> {
        values.iter().map(OsString::from).collect()
    }

    #[test]
    fn add_dir_is_recognised_in_both_argument_forms() {
        assert_eq!(
            parse_add_dirs(&args(&["--add-dir=/a", "--add-dir", "/b", "/c", "--other"]))
                .expect("valid add-dir arguments"),
            [
                PathBuf::from("/a"),
                PathBuf::from("/b"),
                PathBuf::from("/c")
            ]
        );
    }

    #[test]
    fn unrelated_arguments_contribute_no_scopes() {
        assert!(
            parse_add_dirs(&args(&["--model", "opus", "--verbose"]))
                .expect("unrelated options")
                .is_empty()
        );
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
    fn session_detaching_or_root_relocating_arguments_fail_before_planning() {
        for rejected in [
            "--bg",
            "--background",
            "--worktree",
            "--worktree=review",
            "-w",
            "-wreview",
            "--tmux",
            "--tmux=classic",
        ] {
            let error = ClaudeAdapter
                .validate_passthrough_args(&args(&[rejected]))
                .expect_err("session detachment and root relocation must be rejected");
            assert_eq!(error.category(), crate::error::ExitCategory::Usage);
        }
    }

    #[test]
    fn non_session_subcommands_fail_before_planning() {
        for rejected in [
            "agents",
            "auth",
            "auto-mode",
            "doctor",
            "gateway",
            "install",
            "mcp",
            "plugin",
            "plugins",
            "project",
            "setup-token",
            "import",
            "ultrareview",
            "update",
            "upgrade",
        ] {
            for passthrough in [args(&[rejected]), args(&["--verbose", rejected])] {
                let error = ClaudeAdapter
                    .validate_passthrough_args(&passthrough)
                    .expect_err("non-session Claude subcommands must be rejected");
                assert_eq!(error.category(), crate::error::ExitCategory::Usage);
            }
        }

        let error = ClaudeAdapter
            .validate_passthrough_args(&args(&["--", "agents", "list"]))
            .expect_err("the separator does not prevent Claude subcommand dispatch");
        assert_eq!(error.category(), crate::error::ExitCategory::Usage);

        let error = ClaudeAdapter
            .validate_passthrough_args(&args(&["--autocompact", "200000", "import"]))
            .expect_err("the observed autocompact value must not hide the command position");
        assert_eq!(error.category(), crate::error::ExitCategory::Usage);
    }

    #[test]
    fn passthrough_settings_that_can_hide_selected_skills_are_rejected() {
        for rejected in [
            args(&["--settings", "settings.json"]),
            args(&["--settings={\"skillOverrides\":{}}"]),
            args(&["--managed-settings", "{}"]),
            args(&["--managed-settings={}"]),
            args(&["--setting-sources", "user"]),
            args(&["--setting-sources=project"]),
        ] {
            let error = ClaudeAdapter
                .validate_passthrough_args(&rejected)
                .expect_err("user settings cannot override SkillMount's visibility contract");
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
    fn standalone_separator_keeps_flag_shaped_prompt_text_opaque() {
        let passthrough = args(&[
            "--",
            "--bare",
            "--setting-sources",
            "project",
            "--background",
            "--worktree=review",
            "--tmux",
            "--add-dir",
            "/not-a-scope",
        ]);

        let diagnostics = ClaudeAdapter
            .validate_passthrough_args(&passthrough)
            .expect("everything after Claude's separator is prompt text");

        assert!(diagnostics.is_empty());
        assert!(
            parse_add_dirs(&passthrough)
                .expect("opaque prompt text")
                .is_empty()
        );
    }

    #[test]
    fn values_consumed_by_other_options_are_not_treated_as_discovery_controls() {
        for passthrough in [
            args(&["--model", "--bare", "prompt"]),
            args(&["--model", "--settings", "prompt"]),
            args(&["--model", "--setting-sources", "prompt"]),
            args(&["--model", "--background", "prompt"]),
            args(&["--model", "--worktree=review", "prompt"]),
            args(&["--model", "--tmux", "prompt"]),
            args(&["--model", "agents", "prompt"]),
            args(&["--system-prompt", "--disable-slash-commands", "prompt"]),
            args(&["--autocompact", "import", "prompt"]),
        ] {
            ClaudeAdapter
                .validate_passthrough_args(&passthrough)
                .expect("a flag-shaped value belongs to the preceding pinned option");
        }
    }

    #[test]
    fn a_subcommand_name_after_the_prompt_position_is_opaque() {
        ClaudeAdapter
            .validate_passthrough_args(&args(&["review", "agents"]))
            .expect("only Claude's first positional argument selects a subcommand");
    }

    #[test]
    fn safe_mode_environment_uses_the_pinned_boolean_semantics() {
        assert!(!env_flag_enabled(None));
        assert!(!env_flag_enabled(Some(OsString::from("").as_os_str())));
        assert!(!env_flag_enabled(Some(OsString::from("0").as_os_str())));
        assert!(!env_flag_enabled(Some(OsString::from("FALSE").as_os_str())));
        assert!(env_flag_enabled(Some(OsString::from("1").as_os_str())));
        assert!(env_flag_enabled(Some(OsString::from("true").as_os_str())));
    }

    #[test]
    fn disabling_flags_are_rejected_in_option_positions() {
        for passthrough in [
            args(&["--bare"]),
            args(&["prompt", "--safe-mode"]),
            args(&["--model=opus", "--disable-slash-commands", "prompt"]),
        ] {
            assert!(
                ClaudeAdapter
                    .validate_passthrough_args(&passthrough)
                    .is_err()
            );
        }
    }

    #[test]
    fn add_dir_requires_a_value_before_the_next_option_or_separator() {
        for passthrough in [args(&["--add-dir"]), args(&["--add-dir", "--"])] {
            let error = ClaudeAdapter
                .validate_passthrough_args(&passthrough)
                .expect_err("an empty add-dir occurrence is invalid before staging");
            assert_eq!(error.category(), crate::error::ExitCategory::Usage);
        }
    }

    #[test]
    fn a_non_unicode_add_dir_value_remains_platform_native() {
        let opaque = non_unicode_argument();
        let passthrough = vec![OsString::from("--add-dir"), opaque.clone()];

        assert_eq!(
            parse_add_dirs(&passthrough).expect("the parser does not require UTF-8"),
            [PathBuf::from(opaque)]
        );
    }

    #[test]
    fn version_spec_names_the_last_tested_claude_evidence() {
        assert_eq!(
            version_spec().last_tested_banner(),
            LAST_TESTED_CLAUDE_BANNER
        );
    }

    #[cfg(unix)]
    fn non_unicode_argument() -> OsString {
        use std::os::unix::ffi::OsStringExt as _;

        OsString::from_vec(vec![0xff])
    }

    #[cfg(windows)]
    fn non_unicode_argument() -> OsString {
        use std::os::windows::ffi::OsStringExt as _;

        OsString::from_wide(&[0xd800])
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
