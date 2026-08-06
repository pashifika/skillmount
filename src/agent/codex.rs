//! Codex adapter: preferred `.agents/skills` mounts with observed `.agents` and legacy `.codex`
//! discovery roots.

use std::collections::{BTreeMap, BTreeSet};
use std::ffi::{OsStr, OsString};
use std::fs;
use std::path::{Path, PathBuf};

use crate::agent::version::VersionSpec;
use crate::agent::{
    AgentAdapter, DiscoveryScope, DiscoverySnapshot, ExistingSkill, ScopeKind,
    dedupe_scopes_by_terminal, discovery_indexes, insert_direct_deterministically,
};
use crate::diagnostic::{Diagnostic, DiagnosticKind};
use crate::domain::{AgentId, CatalogPolicy, RunContext, SkillCatalog, SkillNameKey};
use crate::error::{AppError, PlanError};
use crate::lock::{LockResource, LockResourceKind};
use crate::mount::plan::apply_conflict_policy;
use crate::mount::resolve::{PathKind, ResolvedEntry, classify};
use crate::mount::{
    ActionSequence, DiscoveryPlan, LaunchPlan, MountAction, MountPlan, PathPrecondition,
};

#[cfg(target_os = "macos")]
mod macos_ffi;

/// Relative discovery entry Codex reads.
const PREFERRED: &str = ".agents/skills";
/// Relative legacy discovery root Codex retains.
const LEGACY: &str = ".codex/skills";
/// Maximum distinct terminal directories one Codex discovery root may traverse.
const MAX_DISCOVERY_DIRECTORIES: usize = 2_000;
/// Maximum directory entries one Codex discovery root may inspect.
const MAX_DISCOVERY_ENTRIES: usize = 20_000;
/// Maximum directory depth below one configured Codex discovery root.
const MAX_DISCOVERY_DEPTH: usize = 6;
/// Maximum serialized path inventory returned by Codex's filesystem walk.
const MAX_DISCOVERY_RESPONSE_BYTES: usize = 4 * 1024 * 1024;
/// Per-entry accounting used by Codex in addition to the serialized path.
const DISCOVERY_RESPONSE_ITEM_OVERHEAD_BYTES: usize = 64;
/// Codex banner attached to the adapter's last-tested discovery evidence.
const LAST_TESTED_CODEX_BANNER: &str = "codex-cli 0.146.0";
const CODEX_VERSION_SPEC: VersionSpec = VersionSpec::new(
    "Codex CLI",
    LAST_TESTED_CODEX_BANNER,
    "SKILLMOUNT_TEST_CODEX_VERSION",
);
/// Local fail-closed bound for a plugin manifest used only to determine namespace behavior.
const MAX_PLUGIN_MANIFEST_BYTES: u64 = 64 * 1024;
const SYSTEM_SKILL_NAMES: [&str; 6] = [
    "imagegen",
    "openai-docs",
    "plugin-creator",
    "review-agent",
    "skill-creator",
    "skill-installer",
];
/// Ordered plugin-manifest spellings recognized by Codex 0.146.0.
const PLUGIN_MANIFEST_PATHS: [&str; 3] = [
    ".codex-plugin/plugin.json",
    ".claude-plugin/plugin.json",
    ".cursor-plugin/plugin.json",
];

#[derive(serde::Deserialize)]
struct CodexPluginManifestName {
    #[serde(default, rename = "name")]
    _name: String,
}

/// The Codex agent adapter.
#[derive(Debug, Clone, Copy, Default)]
pub struct CodexAdapter;

/// Outcome of resolving the one logical `.agents/skills` destination entry.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct CodexDestination {
    /// Visible path selected Skills are written through.
    pub(crate) entry: PathBuf,
    /// Observed state of that path.
    pub(crate) entry_state: PathKind,
    /// Directories the plan must create first, parents before children.
    pub(crate) create_directories: Vec<PathBuf>,
}

/// Resolves the preferred Codex mount entry.
///
/// New links are always addressed through `<project>/.agents/skills`. An existing link at that
/// path is respected as configuration, but a missing entry is created as a regular directory;
/// `.codex/skills` is a visible legacy scope, never a backing-store candidate.
///
/// # Errors
///
/// Returns [`AppError::Plan`] when the preferred entry or its existing parent is broken,
/// cyclic, over-deep, or not a directory.
pub(crate) fn resolve_destination(
    project_root: &Path,
    preferred: &ResolvedEntry,
) -> Result<CodexDestination, AppError> {
    if preferred.kind.is_ambiguous() {
        return Err(PlanError::AmbiguousDiscoveryEntry {
            path: preferred.entry.clone(),
            state: preferred.kind.label(),
        }
        .into());
    }

    // A missing preferred root will exist by the time Codex starts. Model the terminal it will
    // receive after the planned directory creation instead of checking only roots that exist now.
    #[cfg(windows)]
    {
        let canonical_root = preferred
            .terminal
            .clone()
            .unwrap_or_else(|| crate::paths::canonical_anchor(&preferred.entry));
        ensure_codex_root_path_uri_is_ordinary(&canonical_root, &preferred.entry)?;
    }

    match preferred.kind {
        PathKind::Directory | PathKind::DirectoryLink => Ok(CodexDestination {
            entry: preferred.entry.clone(),
            entry_state: preferred.kind,
            create_directories: Vec::new(),
        }),
        PathKind::Missing => resolve_missing_preferred(project_root, preferred),
        // Ambiguous states returned early; `Missing`, `Directory`, and `DirectoryLink` are the rest.
        other => Err(PlanError::AmbiguousDiscoveryEntry {
            path: preferred.entry.clone(),
            state: other.label(),
        }
        .into()),
    }
}

/// Handles every row of the state table where `.agents/skills` does not exist.
fn resolve_missing_preferred(
    project_root: &Path,
    preferred: &ResolvedEntry,
) -> Result<CodexDestination, AppError> {
    let agents_parent = project_root.join(".agents");
    let parent = classify(&agents_parent)?;
    match parent.kind {
        PathKind::Directory | PathKind::DirectoryLink => Ok(CodexDestination {
            entry: preferred.entry.clone(),
            entry_state: PathKind::Missing,
            create_directories: vec![preferred.entry.clone()],
        }),
        PathKind::Missing => Ok(CodexDestination {
            entry: preferred.entry.clone(),
            entry_state: PathKind::Missing,
            create_directories: vec![agents_parent, preferred.entry.clone()],
        }),
        other => Err(PlanError::AmbiguousDiscoveryEntry {
            path: parent.entry,
            state: other.label(),
        }
        .into()),
    }
}

impl CodexAdapter {
    fn preferred_entry(context: &RunContext) -> PathBuf {
        context.project_root.join(PREFERRED)
    }

    fn legacy_entry(context: &RunContext) -> PathBuf {
        context.project_root.join(LEGACY)
    }

    /// Collects every `.agents/skills` and `.codex/skills` between the launch CWD and the project
    /// root, exclusive.
    ///
    /// The project root's own entry is inspected separately as the preferred scope.
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
                ScopeKind::CodexAncestorAgents,
                &ancestor.join(PREFERRED),
            )?);
            scopes.push(inspect_codex_scope(
                ScopeKind::CodexAncestorLegacy,
                &ancestor.join(LEGACY),
            )?);
        }
        Ok(scopes)
    }

    /// Collects user, bundled-system, and administrator roots the supported Codex loader reads.
    fn global_scopes(context: &RunContext) -> Result<Vec<DiscoveryScope>, AppError> {
        let codex = context.agent.codex()?;
        let mut system =
            inspect_codex_scope(ScopeKind::CodexSystem, &codex.home.join("skills/.system"))?;
        reserve_embedded_system_skills(&mut system, &codex.home);
        let mut scopes = vec![
            inspect_codex_scope(ScopeKind::CodexUserAgents, &codex.user_home.join(PREFERRED))?,
            inspect_codex_scope(ScopeKind::CodexUserLegacy, &codex.home.join("skills"))?,
            system,
        ];
        if let Some(admin) = &codex.admin_skills {
            scopes.push(inspect_codex_scope(ScopeKind::CodexAdmin, admin)?);
        }
        Ok(scopes)
    }
}

/// Verifies the Codex launch invariants that remain mandatory for every observed release.
fn verify_launch_invariants(context: &RunContext) -> Result<(), AppError> {
    verify_managed_configuration(context)
}

/// Verifies higher-precedence configuration that can change the inspected discovery model.
fn verify_managed_configuration(context: &RunContext) -> Result<(), AppError> {
    // A debug-only marker lets the process-level transaction suite introduce the same hard
    // condition after planning. Release binaries contain neither the lookup nor this test seam.
    #[cfg(debug_assertions)]
    if let Some(path) =
        std::env::var_os("SKILLMOUNT_TEST_CODEX_MANAGED_CONFIG_PATH").map(PathBuf::from)
    {
        match fs::symlink_metadata(&path) {
            Ok(_) => return Err(unsupported_managed_configuration()),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
            Err(error) => {
                return Err(AppError::Usage(format!(
                    "cannot inspect the deterministic Codex managed-configuration marker {}: {error}",
                    path.display()
                )));
            }
        }
    }

    #[cfg(debug_assertions)]
    if let Some(value) = std::env::var_os("SKILLMOUNT_TEST_CODEX_MANAGED_CONFIG") {
        return match value.to_str() {
            Some("absent") => Ok(()),
            Some("present") => Err(unsupported_managed_configuration()),
            _ => Err(AppError::Internal(
                "SKILLMOUNT_TEST_CODEX_MANAGED_CONFIG must be absent, present, or unset".to_owned(),
            )),
        };
    }

    #[cfg(windows)]
    let managed_file = context.agent.codex()?.home.join("managed_config.toml");
    #[cfg(unix)]
    let managed_file = PathBuf::from("/etc/codex/managed_config.toml");
    match fs::symlink_metadata(&managed_file) {
        Ok(_) => return Err(unsupported_managed_configuration()),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
        Err(error) => {
            return Err(AppError::Usage(format!(
                "cannot prove the pinned Codex discovery configuration because {} cannot be inspected: {error}",
                managed_file.display()
            )));
        }
    }

    #[cfg(target_os = "macos")]
    {
        if macos_ffi::managed_configuration_present().map_err(|error| {
            AppError::Usage(format!(
                "cannot inspect the macOS Codex managed-preference domain: {error}"
            ))
        })? {
            return Err(unsupported_managed_configuration());
        }
    }

    let _ = context;
    Ok(())
}

fn unsupported_managed_configuration() -> AppError {
    AppError::Usage(
        "Codex legacy managed configuration can override the session-pinned project-root markers after SkillMount inspects its roots; this adapter revision does not support that higher-precedence layer"
            .to_owned(),
    )
}

/// Returns the dated version evidence used by the shared advisory observer.
const fn version_spec() -> VersionSpec {
    CODEX_VERSION_SPEC
}

/// Reserves the embedded names installed by the supported Codex before it loads any Skill root.
fn reserve_embedded_system_skills(scope: &mut DiscoveryScope, codex_home: &Path) {
    let root = codex_home.join("skills/.system");
    for name in SYSTEM_SKILL_NAMES {
        let key = SkillNameKey::new(OsStr::new(name));
        if scope.existing_skills.contains_key(&key) {
            continue;
        }
        super::insert_deterministically(
            scope,
            ExistingSkill {
                comparison_key: key,
                raw_name: OsString::from(name),
                entry: root.join(name),
                // The directory may be absent during preflight, but the supported child may create
                // or replace it before loading roots. This is an anticipated child-visible
                // directory, not an unsupported missing path. Conflict policy treats every
                // system-cache name as unskippable because persistent configuration may disable or
                // replace the cache before loading.
                kind: PathKind::Directory,
                source_canonical: None,
            },
            DiagnosticKind::CodexDiscovery,
        );
    }
}

/// Mirrors Codex's recursive `**/SKILL.md` discovery while retaining immediate path occupancy.
fn inspect_codex_scope(kind: ScopeKind, entry: &Path) -> Result<DiscoveryScope, AppError> {
    let state = classify(entry)?;
    let mut scope = DiscoveryScope {
        kind,
        state,
        aliases: Vec::new(),
        observed_directories: Vec::new(),
        existing_skills: BTreeMap::new(),
        direct_entries: BTreeMap::new(),
        warnings: Vec::new(),
    };
    if !matches!(
        scope.state.kind,
        PathKind::Directory | PathKind::DirectoryLink
    ) {
        return Ok(scope);
    }
    // Codex canonicalizes every discovery root before walking it. Retain the visible spelling for
    // reuse evidence while `terminal` below supplies the canonical identity, cycle break, and
    // metadata fallback. The system policy therefore ignores links encountered below this root,
    // but it does not suppress a linked root itself.
    let root_terminal = scope.state.terminal.clone().ok_or_else(|| {
        AppError::Internal("a usable Codex discovery root must expose a terminal path".to_owned())
    })?;
    #[cfg(windows)]
    ensure_codex_root_path_uri_is_ordinary(&root_terminal, entry)?;
    let mut walk = CodexWalkState {
        kind,
        root: entry,
        canonical_root: &root_terminal,
        inspected_entries: 0,
        response_bytes: 0,
    };
    let mut pending = vec![(entry.to_path_buf(), 0_usize)];
    let mut visited = BTreeSet::new();
    while let Some((directory, depth)) = pending.pop() {
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
        scope.observed_directories.push(terminal.clone());
        inspect_codex_skill_entry(&mut scope, &directory, &state, &terminal)?;
        let children = inspect_codex_directory_entries(&mut scope, &directory, depth, &mut walk)?;
        pending.extend(children.into_iter().rev());
    }

    scope.observed_directories.sort();
    scope.observed_directories.dedup();
    Ok(scope)
}

fn inspect_codex_skill_entry(
    scope: &mut DiscoveryScope,
    directory: &Path,
    state: &ResolvedEntry,
    terminal: &Path,
) -> Result<(), AppError> {
    let exact_skill_md = crate::catalog::find_exact_entry(directory, OsStr::new("SKILL.md"))
        .map_err(|error| AppError::MissingInput {
            path: directory.to_path_buf(),
            reason: error.to_string(),
        })?;
    let Some(exact_skill_md) = exact_skill_md else {
        return Ok(());
    };
    let discovered_skill_md = exact_skill_md.path();
    match fs::symlink_metadata(&discovered_skill_md) {
        Ok(metadata) if metadata.file_type().is_file() => {
            // The pinned loader canonicalizes each discovered SKILL.md before parsing it. Its
            // missing-name fallback therefore uses the target directory name rather than an alias
            // through which a linked collection was reached.
            let skill_md = fs::canonicalize(&discovered_skill_md).map_err(|error| {
                PlanError::UnsupportedLayout {
                    path: discovered_skill_md.clone(),
                    reason: format!(
                        "Codex Skill inventory is incomplete because the discovered SKILL.md cannot be canonicalized: {error}"
                    ),
                }
            })?;
            let skill_directory = skill_md.parent().ok_or_else(|| {
                AppError::Internal(
                    "a canonical Codex SKILL.md must have a containing directory".to_owned(),
                )
            })?;
            inspect_codex_skill(
                scope,
                directory,
                state,
                terminal,
                skill_directory,
                &skill_md,
            )
        }
        Ok(_) => {
            scope.warnings.push(Diagnostic::warning_with_kind(
                DiagnosticKind::CodexDiscovery,
                "Codex will not load this SKILL.md because its directory entry is not a regular file",
                discovered_skill_md,
            ));
            Ok(())
        }
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(error) => Err(AppError::MissingInput {
            path: discovered_skill_md,
            reason: error.to_string(),
        }),
    }
}

struct CodexWalkState<'a> {
    kind: ScopeKind,
    root: &'a Path,
    canonical_root: &'a Path,
    inspected_entries: usize,
    response_bytes: usize,
}

fn inspect_codex_directory_entries(
    scope: &mut DiscoveryScope,
    directory: &Path,
    depth: usize,
    walk: &mut CodexWalkState<'_>,
) -> Result<Vec<(PathBuf, usize)>, AppError> {
    let entries = fs::read_dir(directory).map_err(|error| AppError::MissingInput {
        path: directory.to_path_buf(),
        reason: error.to_string(),
    })?;
    let mut children = Vec::new();
    for child in entries {
        let child = child.map_err(|error| AppError::MissingInput {
            path: directory.to_path_buf(),
            reason: error.to_string(),
        })?;
        walk.inspected_entries += 1;
        if walk.inspected_entries > MAX_DISCOVERY_ENTRIES {
            return Err(PlanError::UnsupportedLayout {
                path: walk.root.to_path_buf(),
                reason: format!(
                    "recursive Codex discovery exceeds {MAX_DISCOVERY_ENTRIES} directory entries"
                ),
            }
            .into());
        }

        let raw_name = child.file_name();
        let child_path = child.path();
        if !codex_directory_entry_name_is_representable(&raw_name) {
            return Err(PlanError::UnsupportedLayout {
                path: child_path,
                reason: "Codex Skill inventory is incomplete because Codex 0.146.0 converts this non-Unicode directory-entry name lossily before traversing it".to_owned(),
            }
            .into());
        }
        let child_file_type = child.file_type().map_err(|error| AppError::MissingInput {
            path: child_path.clone(),
            reason: error.to_string(),
        })?;
        let child_state = classify(&child_path)?;
        let returned_by_codex = match child_state.kind {
            PathKind::Directory => true,
            PathKind::DirectoryLink => walk.kind != ScopeKind::CodexSystem,
            PathKind::NotDirectory => child_file_type.is_file(),
            PathKind::Missing
            | PathKind::BrokenLink
            | PathKind::CyclicLink
            | PathKind::DepthExceeded => false,
        };
        if returned_by_codex {
            let response_path = child_path.strip_prefix(walk.root).map_or_else(
                |_| child_path.clone(),
                |suffix| walk.canonical_root.join(suffix),
            );
            reserve_codex_response_bytes(&mut walk.response_bytes, &response_path, walk.root)?;
        }
        if depth == 0 {
            // Discovery walks the canonical root, but placement still addresses the visible root
            // spelling. Retain occupancy at that logical destination instead of leaking a target
            // path into the mount plan.
            let direct_path = walk.root.join(&raw_name);
            let direct_state = classify(&direct_path)?;
            insert_direct_deterministically(
                scope,
                ExistingSkill {
                    comparison_key: SkillNameKey::new(&raw_name),
                    raw_name: raw_name.clone(),
                    entry: direct_path,
                    kind: direct_state.kind,
                    source_canonical: direct_state.terminal,
                },
            );
        }
        let traversable_directory = child_state.kind == PathKind::Directory
            || (child_state.kind == PathKind::DirectoryLink && walk.kind != ScopeKind::CodexSystem);
        if depth < MAX_DISCOVERY_DEPTH && !is_hidden(&raw_name) && traversable_directory {
            children.push((child_path, depth + 1));
        }
    }
    children.sort_by(|left, right| left.0.cmp(&right.0));
    Ok(children)
}

fn codex_directory_entry_name_is_representable(name: &OsStr) -> bool {
    name.to_str().is_some()
}

/// Rejects a canonical Windows root for which Codex creates an opaque `PathUri`.
///
/// The pinned walker immediately joins child names onto the root URI, while opaque URIs reject
/// every non-empty join. This predicate intentionally recognizes a conservative subset of the
/// `url` crate's accepted UNC hosts; rejecting an unusual but representable host is safer than
/// claiming a native inventory Codex cannot produce.
#[cfg(windows)]
fn ensure_codex_root_path_uri_is_ordinary(
    canonical_root: &Path,
    visible_root: &Path,
) -> Result<(), AppError> {
    if codex_root_path_uri_is_ordinary(canonical_root) {
        return Ok(());
    }
    Err(PlanError::UnsupportedLayout {
        path: visible_root.to_path_buf(),
        reason: "Codex Skill inventory is incomplete because the canonical Windows discovery root cannot be represented as an ordinary file URI".to_owned(),
    }
    .into())
}

#[cfg(windows)]
fn codex_root_path_uri_is_ordinary(path: &Path) -> bool {
    use std::path::{Component, Prefix};

    if !path.is_absolute() || path.to_str().is_none() {
        return false;
    }
    let Some(Component::Prefix(prefix)) = path.components().next() else {
        return false;
    };
    match prefix.kind() {
        Prefix::Disk(_) | Prefix::VerbatimDisk(_) => true,
        Prefix::UNC(server, _) | Prefix::VerbatimUNC(server, _) => {
            server.to_str().is_some_and(is_conservative_url_host)
        }
        Prefix::Verbatim(_) | Prefix::DeviceNS(_) => false,
    }
}

#[cfg(windows)]
fn is_conservative_url_host(host: &str) -> bool {
    use std::net::Ipv4Addr;

    // `PathUri::TryFrom<Url>` removes a localhost authority. On Windows that makes
    // `to_abs_path()` lose the UNC convention and fail instead of returning this root.
    if host.eq_ignore_ascii_case("localhost") {
        return false;
    }
    // URL host parsing canonicalizes IPv6 text before `to_file_path()` reconstructs the UNC
    // spelling. Do not assume two native server spellings identify the same resource.
    if host.starts_with('[') || host.ends_with(']') {
        return false;
    }
    if host.parse::<Ipv4Addr>().is_ok() {
        return true;
    }
    if host.is_empty() || host.len() > 253 || host.starts_with('.') || host.ends_with('.') {
        return false;
    }
    let mut labels = host.split('.').peekable();
    let mut final_label_has_alpha = false;
    while let Some(label) = labels.next() {
        if label.is_empty()
            || label.len() > 63
            || label.starts_with('-')
            || label.ends_with('-')
            || label
                .as_bytes()
                .get(2..4)
                .is_some_and(|separator| separator == b"--")
            || !label
                .bytes()
                .all(|byte| byte.is_ascii_alphanumeric() || byte == b'-')
        {
            return false;
        }
        if labels.peek().is_none() {
            // WHATWG host parsing treats a final hexadecimal IPv4 number as an address and
            // rewrites its spelling. Native UNC traversal cannot stand in for that different
            // round-tripped path.
            if label
                .get(..2)
                .is_some_and(|prefix| prefix.eq_ignore_ascii_case("0x"))
                && label
                    .get(2..)
                    .is_some_and(|digits| digits.bytes().all(|byte| byte.is_ascii_hexdigit()))
            {
                return false;
            }
            final_label_has_alpha = label.bytes().any(|byte| byte.is_ascii_alphabetic());
        }
    }
    final_label_has_alpha
}

/// Accounts for Codex's serialized walk response before trusting the inventory as complete.
fn reserve_codex_response_bytes(
    response_bytes: &mut usize,
    path: &Path,
    root: &Path,
) -> Result<(), AppError> {
    let item_bytes =
        codex_path_uri_upper_bound(path).saturating_add(DISCOVERY_RESPONSE_ITEM_OVERHEAD_BYTES);
    let Some(total) = response_bytes.checked_add(item_bytes) else {
        return Err(discovery_response_limit(root));
    };
    if total > MAX_DISCOVERY_RESPONSE_BYTES {
        return Err(discovery_response_limit(root));
    }
    *response_bytes = total;
    Ok(())
}

fn discovery_response_limit(root: &Path) -> AppError {
    PlanError::UnsupportedLayout {
        path: root.to_path_buf(),
        reason: format!(
            "recursive Codex discovery exceeds the {MAX_DISCOVERY_RESPONSE_BYTES}-byte walk response limit"
        ),
    }
    .into()
}

/// Conservatively bounds the `PathUri::to_string()` length used by the supported Codex walker.
///
/// Sixteen bytes cover the file-URI scheme and platform prefix. Bytes outside the unreserved URI
/// path set are charged as percent-encoded triples; charging a few legal reserved bytes as triples
/// can reject early but cannot let Codex truncate a snapshot `SkillMount` called complete.
#[cfg(unix)]
fn codex_path_uri_upper_bound(path: &Path) -> usize {
    use std::os::unix::ffi::OsStrExt;

    16_usize.saturating_add(
        path.as_os_str()
            .as_bytes()
            .iter()
            .map(|byte| encoded_uri_byte_len(*byte, false))
            .sum::<usize>(),
    )
}

#[cfg(windows)]
fn codex_path_uri_upper_bound(path: &Path) -> usize {
    let text = path.as_os_str().to_string_lossy();
    16_usize.saturating_add(
        text.as_bytes()
            .iter()
            .map(|byte| encoded_uri_byte_len(*byte, true))
            .sum::<usize>(),
    )
}

const fn encoded_uri_byte_len(byte: u8, backslash_is_separator: bool) -> usize {
    if byte.is_ascii_alphanumeric()
        || matches!(byte, b'-' | b'.' | b'_' | b'~' | b'/' | b':')
        || (backslash_is_separator && byte == b'\\')
    {
        1
    } else {
        3
    }
}

/// Adds a valid Codex Skill or a warning for malformed frontmatter.
fn inspect_codex_skill(
    scope: &mut DiscoveryScope,
    visible_directory: &Path,
    visible_state: &ResolvedEntry,
    visible_terminal: &Path,
    metadata_directory: &Path,
    skill_md: &Path,
) -> Result<(), AppError> {
    match crate::catalog::frontmatter::codex_metadata(skill_md, metadata_directory) {
        Ok(metadata) => {
            let name = metadata.name.ok_or_else(|| {
                AppError::Internal(
                    "Codex metadata parsing succeeded without a logical name".to_owned(),
                )
            })?;
            super::insert_deterministically(
                scope,
                ExistingSkill {
                    comparison_key: SkillNameKey::new(OsStr::new(&name)),
                    raw_name: OsString::from(name),
                    entry: visible_directory.to_path_buf(),
                    kind: visible_state.kind,
                    source_canonical: Some(visible_terminal.to_path_buf()),
                },
                DiagnosticKind::CodexDiscovery,
            );
        }
        Err(crate::catalog::frontmatter::CodexMetadataError::Rejected(reason)) => {
            scope.warnings.push(Diagnostic::warning_with_kind(
                DiagnosticKind::CodexDiscovery,
                format!("Codex will not load this malformed SKILL.md: {reason}"),
                skill_md.to_path_buf(),
            ));
        }
        Err(crate::catalog::frontmatter::CodexMetadataError::Incomplete(reason)) => {
            return Err(PlanError::UnsupportedLayout {
                path: skill_md.to_path_buf(),
                reason: format!("Codex Skill inventory is incomplete: {reason}"),
            }
            .into());
        }
    }
    Ok(())
}

/// Rejects a selected source whose mounted logical name Codex would namespace-qualify.
///
/// Codex canonicalizes a followed `SKILL.md` and treats its containing source directory as a
/// namespace lookup root. A valid manifest at that directory or any ancestor would therefore
/// change `name` into `plugin:name`, while `SkillMount`'s injected enable rule still addresses the
/// portable base name. Existing discovered Skills may be indexed conservatively by that base name,
/// but a selected source must never cross the launch boundary under a different logical name.
fn verify_selected_plugin_namespaces(catalog: &SkillCatalog) -> Result<(), AppError> {
    for resolution in &catalog.resolutions {
        let source = &resolution.selected.origin.source_canonical;
        if let Some(manifest) = nearest_plugin_manifest(source)? {
            return Err(PlanError::UnsupportedLayout {
                path: manifest,
                reason: format!(
                    "Codex 0.146.0 would namespace-qualify selected Skill {} from this plugin manifest, but this adapter can only enable its portable base name",
                    resolution.selected.mount_name
                ),
            }
            .into());
        }
    }
    Ok(())
}

fn nearest_plugin_manifest(source: &Path) -> Result<Option<PathBuf>, AppError> {
    for ancestor in source.ancestors() {
        if let Some(manifest) = plugin_manifest_at(ancestor)? {
            return Ok(Some(manifest));
        }
    }
    Ok(None)
}

fn plugin_manifest_at(root: &Path) -> Result<Option<PathBuf>, AppError> {
    for relative in PLUGIN_MANIFEST_PATHS {
        let candidate = root.join(relative);
        match fs::metadata(&candidate) {
            Ok(metadata) if metadata.is_file() => {
                let contents = crate::catalog::frontmatter::read_bounded_regular_file(
                    &candidate,
                    "Codex plugin manifest",
                    MAX_PLUGIN_MANIFEST_BYTES,
                )
                .map_err(|error| {
                    PlanError::UnsupportedLayout {
                        path: candidate.clone(),
                        reason: format!(
                            "the selected Skill's potential Codex plugin manifest cannot be read completely: {error}"
                        ),
                    }
                })?;
                // Codex stops at the first regular manifest spelling at a root. A malformed first
                // file suppresses lower-precedence spellings, so do not continue after parse
                // failure. Deserializing the same one-field shape preserves unknown-field,
                // duplicate-name, default-name, and value-type behavior.
                return Ok(serde_json::from_slice::<CodexPluginManifestName>(&contents)
                    .is_ok()
                    .then_some(candidate));
            }
            Ok(_) => {}
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
            Err(error) => {
                return Err(PlanError::UnsupportedLayout {
                    path: candidate,
                    reason: format!(
                        "the selected Skill's potential Codex plugin manifest cannot be inspected: {error}"
                    ),
                }
                .into());
            }
        }
    }
    Ok(None)
}

#[cfg(unix)]
fn is_hidden(name: &OsStr) -> bool {
    use std::os::unix::ffi::OsStrExt;

    name.as_bytes().first() == Some(&b'.')
}

#[cfg(windows)]
fn is_hidden(name: &OsStr) -> bool {
    use std::os::windows::ffi::OsStrExt;

    name.encode_wide().next() == Some(u16::from(b'.'))
}

/// Returns whether a forwarded Codex argument changes the CWD used for project discovery.
fn changes_codex_root(argument: &OsStr) -> bool {
    argument == OsStr::new("-C")
        || argument == OsStr::new("--cd")
        || os_starts_with(argument, "--cd=")
        || (os_starts_with(argument, "-C") && argument != OsStr::new("-C"))
}

/// Returns whether a forwarded Codex argument moves Skill discovery to another host.
fn changes_codex_discovery_host(argument: &OsStr) -> bool {
    argument == OsStr::new("--remote")
        || os_starts_with(argument, "--remote=")
        || argument == OsStr::new("--remote-auth-token-env")
        || os_starts_with(argument, "--remote-auth-token-env=")
}

/// Returns whether a forwarded option can replace the pinned discovery contract through config.
fn changes_codex_discovery_config(argument: &OsStr) -> bool {
    argument == OsStr::new("-c")
        || argument == OsStr::new("--config")
        || os_starts_with(argument, "--config=")
        || (os_starts_with(argument, "-c") && argument != OsStr::new("-c"))
        || argument == OsStr::new("-p")
        || argument == OsStr::new("--profile")
        || os_starts_with(argument, "--profile=")
        || (os_starts_with(argument, "-p") && argument != OsStr::new("-p"))
        || argument == OsStr::new("--enable")
        || os_starts_with(argument, "--enable=")
        || argument == OsStr::new("--disable")
        || os_starts_with(argument, "--disable=")
        || argument == OsStr::new("--ignore-user-config")
}

fn exits_without_a_codex_session(argument: &OsStr) -> bool {
    argument == OsStr::new("--help")
        || argument == OsStr::new("--version")
        || os_starts_with(argument, "-h")
        || os_starts_with(argument, "-V")
}

/// Returns whether a forwarded command is not one bounded Codex session.
fn is_unsupported_codex_command(argument: &OsStr) -> bool {
    [
        "login",
        "logout",
        "mcp",
        "plugin",
        "mcp-server",
        "app-server",
        "remote-control",
        "app",
        "completion",
        "update",
        "doctor",
        "sandbox",
        "debug",
        "apply",
        "a",
        "archive",
        "delete",
        "unarchive",
        "cloud",
        "cloud-tasks",
        "exec-server",
        "execpolicy",
        "responses-api-proxy",
        "stdio-to-uds",
        "features",
        "help",
    ]
    .iter()
    .any(|command| argument == OsStr::new(command))
}

fn validate_codex_command(args: &[OsString]) -> Result<(), AppError> {
    if contains_bare_image_option(args) {
        return Err(unsupported_bare_image());
    }
    let Some((command_index, command)) = first_root_positional(args) else {
        return Err(unsupported_interactive());
    };
    if command == OsStr::new("resume") || command == OsStr::new("fork") {
        return Err(unsupported_resume(command));
    }
    if is_unsupported_codex_command(command) {
        return Err(unsupported_command(command));
    }
    if command == OsStr::new("exec") || command == OsStr::new("e") {
        if let Some((_, nested)) = first_exec_positional(&args[command_index + 1..]) {
            if nested == OsStr::new("resume") {
                return Err(unsupported_resume(nested));
            }
            if nested == OsStr::new("help") {
                return Err(unsupported_command(nested));
            }
        }
        return Ok(());
    }
    if command == OsStr::new("review") {
        return Ok(());
    }
    Err(unsupported_interactive())
}

fn contains_bare_image_option(args: &[OsString]) -> bool {
    args.iter()
        .take_while(|argument| argument.as_os_str() != OsStr::new("--"))
        .any(|argument| argument == OsStr::new("-i") || argument == OsStr::new("--image"))
}

fn first_root_positional(args: &[OsString]) -> Option<(usize, &OsStr)> {
    first_positional(args, RootOptionSet::Root)
}

fn first_exec_positional(args: &[OsString]) -> Option<(usize, &OsStr)> {
    first_positional(args, RootOptionSet::Exec)
}

#[derive(Debug, Clone, Copy)]
enum RootOptionSet {
    Root,
    Exec,
}

fn first_positional(args: &[OsString], option_set: RootOptionSet) -> Option<(usize, &OsStr)> {
    let mut index = 0;
    while index < args.len() {
        let argument = args[index].as_os_str();
        if argument == OsStr::new("--") {
            return None;
        }
        // The launch validator rejects a bare variadic image option before reaching this parser.
        // Keep this defensive branch so a future internal caller cannot guess across it. An
        // attached value closes that occurrence and parsing resumes at the next token.
        if argument == OsStr::new("-i") || argument == OsStr::new("--image") {
            return None;
        }
        if os_starts_with(argument, "-i") || os_starts_with(argument, "--image=") {
            index += 1;
            continue;
        }
        if option_consumes_value(argument, option_set) {
            index = index.saturating_add(2);
            continue;
        }
        if os_starts_with(argument, "-") && argument != OsStr::new("-") {
            index += 1;
            continue;
        }
        return Some((index, argument));
    }
    None
}

fn option_consumes_value(argument: &OsStr, option_set: RootOptionSet) -> bool {
    if [
        "-c",
        "--config",
        "--enable",
        "--disable",
        "--remote",
        "--remote-auth-token-env",
        "-m",
        "--model",
        "--local-provider",
        "-p",
        "--profile",
        "-s",
        "--sandbox",
        "-C",
        "--cd",
        "--add-dir",
        "-a",
        "--ask-for-approval",
    ]
    .iter()
    .any(|option| argument == OsStr::new(option))
    {
        return true;
    }
    matches!(option_set, RootOptionSet::Exec)
        && ["--output-schema", "--color", "-o", "--output-last-message"]
            .iter()
            .any(|option| argument == OsStr::new(option))
}

fn unsupported_resume(command: &OsStr) -> AppError {
    AppError::Usage(format!(
        "Codex command {} can restore a session whose discovery CWD differs from the roots SkillMount inspected; resume and fork sessions are not supported",
        Path::new(command).display()
    ))
}

fn unsupported_command(command: &OsStr) -> AppError {
    AppError::Usage(format!(
        "Codex command {} is not a single bounded exec or review session; service and operator commands are not supported",
        Path::new(command).display()
    ))
}

fn unsupported_interactive() -> AppError {
    AppError::Usage(
        "interactive Codex TUI sessions are not supported because Codex 0.146.0 can reload higher-precedence managed configuration during /resume, /fork, /new, or side-conversation transitions; use a bounded `exec` or `review` session"
            .to_owned(),
    )
}

fn unsupported_bare_image() -> AppError {
    AppError::Usage(
        "bare Codex -i/--image is variadic, and a later option can terminate its values and expose an unsupported nested command; use one attached -iVALUE or --image=VALUE occurrence per image"
            .to_owned(),
    )
}

/// Builds the pinned session overrides that keep Codex inside the inspected discovery contract.
fn injected_session_args(
    context: &RunContext,
    catalog: &SkillCatalog,
    preserved: &[crate::mount::PreservedSkill],
) -> Vec<OsString> {
    let mut arguments = vec![
        OsString::from("-C"),
        context.launch_cwd.as_os_str().to_os_string(),
        OsString::from("-c"),
        OsString::from("project_root_markers=[\".git\"]"),
    ];
    let enabled_names = catalog
        .resolutions
        .iter()
        .filter(|resolution| {
            let key = resolution.selected.mount_name.comparison_key();
            !preserved.iter().any(|entry| entry.comparison_key == key)
        })
        .map(|resolution| {
            format!(
                "{{name=\"{}\",enabled=true}}",
                resolution.selected.mount_name.as_str()
            )
        })
        .collect::<Vec<_>>();
    if !enabled_names.is_empty() {
        arguments.push(OsString::from("-c"));
        arguments.push(OsString::from(format!(
            "skills.config=[{}]",
            enabled_names.join(",")
        )));
    }
    arguments
}

#[cfg(unix)]
fn os_starts_with(value: &OsStr, prefix: &str) -> bool {
    use std::os::unix::ffi::OsStrExt;

    value.as_bytes().starts_with(prefix.as_bytes())
}

#[cfg(windows)]
fn os_starts_with(value: &OsStr, prefix: &str) -> bool {
    use std::os::windows::ffi::OsStrExt;

    let value = value.encode_wide().collect::<Vec<_>>();
    let prefix = prefix.encode_utf16().collect::<Vec<_>>();
    value.starts_with(&prefix)
}

/// Describes a scope with a stable project anchor when possible and a self anchor otherwise.
fn scope_root_lock(context: &RunContext, scope: &DiscoveryScope) -> Result<LockResource, AppError> {
    if scope.state.entry.starts_with(&context.project_root) {
        LockResource::describe_entry(
            LockResourceKind::DiscoveryEntry,
            &context.project_root,
            &scope.state,
        )
    } else {
        Ok(LockResource::describe_unanchored(
            LockResourceKind::DiscoveryEntry,
            &scope.state.entry,
        ))
    }
}

impl AgentAdapter for CodexAdapter {
    fn version_spec(&self) -> VersionSpec {
        version_spec()
    }

    fn catalog_policy(&self) -> CatalogPolicy {
        // Codex indexes a Skill by its frontmatter name and discovers only an exact regular
        // `SKILL.md` directory entry, so these requirements hold even when generic metadata
        // validation is disabled: the injected enable rule must address the same logical name the
        // child loads.
        CatalogPolicy {
            requires_exact_skill_md_entry: true,
            always_parses_metadata: true,
            requires_name: true,
            requires_description: true,
            requires_matching_name: true,
        }
    }

    fn destination_stores(&self, context: &RunContext) -> Vec<PathBuf> {
        vec![Self::preferred_entry(context)]
    }

    fn validate_launch_invariants(&self, context: &RunContext) -> Result<(), AppError> {
        verify_launch_invariants(context)
    }

    fn validate_spawn_boundary(
        &self,
        context: &RunContext,
        catalog: &SkillCatalog,
        _discovery: &DiscoverySnapshot,
        _plan: &MountPlan,
    ) -> Result<(), AppError> {
        verify_launch_invariants(context)?;
        verify_selected_plugin_namespaces(catalog)
    }

    fn validate_passthrough_args(&self, args: &[OsString]) -> Result<Vec<Diagnostic>, AppError> {
        if contains_bare_image_option(args) {
            return Err(unsupported_bare_image());
        }
        for argument in args {
            if argument == OsStr::new("--") {
                break;
            }
            if changes_codex_root(argument) {
                return Err(AppError::Usage(format!(
                    "Codex argument {} changes the child discovery root after SkillMount has inspected and locked it; use SkillMount's --cwd option instead",
                    Path::new(argument).display()
                )));
            }
            if changes_codex_discovery_host(argument) {
                return Err(AppError::Usage(format!(
                    "Codex argument {} moves Skill discovery to a remote app server after SkillMount has inspected and locked local roots; remote sessions are not supported",
                    Path::new(argument).display()
                )));
            }
            if changes_codex_discovery_config(argument) {
                return Err(AppError::Usage(format!(
                    "Codex argument {} can change Skill roots, filters, or bundled-Skill visibility after SkillMount has fixed its discovery contract; forwarded config and profile overrides are not supported",
                    Path::new(argument).display()
                )));
            }
            if exits_without_a_codex_session(argument) {
                return Err(unsupported_command(argument));
            }
        }
        validate_codex_command(args)?;
        Ok(Vec::new())
    }

    fn catalog_diagnostics(
        &self,
        context: &RunContext,
        catalog: &SkillCatalog,
        plan: &MountPlan,
    ) -> Vec<Diagnostic> {
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
            .filter(|resolution| {
                let key = resolution.selected.mount_name.comparison_key();
                !plan
                    .preserved
                    .iter()
                    .any(|preserved| preserved.comparison_key == key)
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
        let preferred_path = Self::preferred_entry(context);
        let legacy_path = Self::legacy_entry(context);
        let preferred = classify(&preferred_path)?;
        let legacy = classify(&legacy_path)?;
        let destination = resolve_destination(&context.project_root, &preferred)?;

        let mut scopes = vec![
            inspect_codex_scope(ScopeKind::CodexProjectAgents, &preferred_path)?,
            inspect_codex_scope(ScopeKind::CodexProjectLegacy, &legacy_path)?,
        ];
        scopes.extend(Self::ancestor_scopes(context)?);
        scopes.extend(Self::global_scopes(context)?);

        let mut lock_resources = Vec::new();
        for scope in &scopes {
            lock_resources.push(scope_root_lock(context, scope)?);
            for terminal in &scope.observed_directories {
                if scope.state.terminal.as_deref() != Some(terminal.as_path()) {
                    lock_resources.push(LockResource::describe_unanchored(
                        LockResourceKind::DiscoveryEntry,
                        terminal,
                    ));
                }
            }
        }
        let agents_parent = classify(&context.project_root.join(".agents"))?;
        if matches!(
            agents_parent.kind,
            PathKind::Directory | PathKind::DirectoryLink
        ) {
            lock_resources.push(LockResource::describe_entry(
                LockResourceKind::DiscoveryEntry,
                &context.project_root,
                &agents_parent,
            )?);
        }
        lock_resources.push(LockResource::describe(
            LockResourceKind::BackingStore,
            &context.project_root,
            &destination.entry,
        )?);
        lock_resources.sort_by_key(LockResource::ordering_key);
        lock_resources.dedup();

        let mut warnings = Vec::new();
        if matches!(legacy.kind, PathKind::Directory | PathKind::DirectoryLink)
            && !preferred.shares_terminal_with(&legacy)
        {
            warnings.push(Diagnostic::warning_with_kind(
                DiagnosticKind::CodexDiscovery,
                format!(
                    "{} remains a separate legacy Codex discovery root; new mounts use {}",
                    legacy_path.display(),
                    preferred_path.display()
                ),
                legacy_path,
            ));
        }
        // A preferred entry that links to the legacy store makes both scopes the same
        // physical directory. Keeping both would make the store's own contents look like a foreign
        // cross-scope Skill and turn every mount into a spurious reuse.
        scopes = dedupe_scopes_by_terminal(scopes, &destination.entry);

        for scope in &scopes {
            warnings.extend(scope.warnings.iter().cloned());
        }
        let (visible_skills, mount_entries) = discovery_indexes(&scopes, &destination.entry);

        Ok(DiscoverySnapshot {
            agent: AgentId::Codex,
            scopes,
            visible_skills,
            mount_entries,
            discovery_entry: preferred_path,
            backing_store: destination.entry,
            backing_store_state: destination.entry_state,
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
        verify_selected_plugin_namespaces(catalog)?;
        let preferred = classify(&discovery.discovery_entry)?;
        let destination = resolve_destination(&context.project_root, &preferred)?;

        let mut actions = ActionSequence::default();
        // Dependency order: the `.agents` parent, then its `skills` directory, then Skills.
        for directory in &destination.create_directories {
            actions.push(
                MountAction::CreateDirectory {
                    path: directory.clone(),
                },
                PathPrecondition::Missing,
            );
        }
        let mut preserved = Vec::new();
        apply_conflict_policy(context, catalog, discovery, &mut actions, &mut preserved)?;
        let injected_args = injected_session_args(context, catalog, &preserved);

        Ok(MountPlan {
            agent: AgentId::Codex,
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
                environment_overrides: context.agent.codex()?.home_override.as_ref().map_or_else(
                    Vec::new,
                    |path| {
                        vec![(
                            OsString::from("CODEX_HOME"),
                            path.as_os_str().to_os_string(),
                        )]
                    },
                ),
            },
        })
    }
}

#[cfg(test)]
mod tests {
    use super::{
        DISCOVERY_RESPONSE_ITEM_OVERHEAD_BYTES, LAST_TESTED_CODEX_BANNER,
        MAX_DISCOVERY_RESPONSE_BYTES, codex_directory_entry_name_is_representable,
        codex_path_uri_upper_bound, reserve_codex_response_bytes, version_spec,
    };
    use std::ffi::OsString;
    use std::path::Path;

    #[test]
    fn codex_walk_response_budget_accepts_the_boundary_and_rejects_the_next_entry() {
        let path = Path::new("/skills/alpha/SKILL.md");
        let item = codex_path_uri_upper_bound(path) + DISCOVERY_RESPONSE_ITEM_OVERHEAD_BYTES;
        let mut used = MAX_DISCOVERY_RESPONSE_BYTES - item;

        reserve_codex_response_bytes(&mut used, path, Path::new("/skills"))
            .expect("the exact Codex response boundary is complete");
        assert_eq!(used, MAX_DISCOVERY_RESPONSE_BYTES);

        let error = reserve_codex_response_bytes(&mut used, path, Path::new("/skills"))
            .expect_err("the next returned path would make Codex truncate the walk");
        assert!(error.to_string().contains("walk response limit"));
    }

    #[test]
    fn version_spec_names_the_last_tested_codex_evidence() {
        assert_eq!(
            version_spec().last_tested_banner(),
            LAST_TESTED_CODEX_BANNER
        );
    }

    #[cfg(unix)]
    #[test]
    fn unix_non_unicode_directory_entry_is_not_codex_representable() {
        use std::os::unix::ffi::OsStringExt as _;

        let name = OsString::from_vec(vec![b's', b'k', b'i', b'l', b'l', 0xff]);

        assert!(!codex_directory_entry_name_is_representable(&name));
    }

    #[cfg(windows)]
    #[test]
    fn windows_non_unicode_directory_entry_is_not_codex_representable() {
        use std::os::windows::ffi::OsStringExt as _;

        let name = OsString::from_wide(&[
            u16::from(b's'),
            u16::from(b'k'),
            u16::from(b'i'),
            u16::from(b'l'),
            u16::from(b'l'),
            0xd800,
        ]);

        assert!(!codex_directory_entry_name_is_representable(&name));
    }

    #[cfg(windows)]
    #[test]
    fn windows_discovery_root_requires_an_ordinary_file_uri() {
        use super::codex_root_path_uri_is_ordinary;

        for ordinary in [
            Path::new(r"C:\skills"),
            Path::new(r"\\?\C:\skills"),
            Path::new(r"\\server\share\skills"),
            Path::new(r"\\?\UNC\server\share\skills"),
        ] {
            assert!(
                codex_root_path_uri_is_ordinary(ordinary),
                "{}",
                ordinary.display()
            );
        }
        for opaque in [
            Path::new(r"\\.\PIPE\skillmount"),
            Path::new(r"\\?\Volume{01234567-89ab-cdef-0123-456789abcdef}\skills"),
            Path::new(r"\\localhost\share\skills"),
            Path::new(r"\\LOCALHOST\share\skills"),
            Path::new(r"\\0x7f000001\share\skills"),
            Path::new(r"\\server.0x1\share\skills"),
            Path::new(r"\\0x\share\skills"),
            Path::new(r"\\server.0x\share\skills"),
            Path::new(r"\\[::1]\share\skills"),
            Path::new(r"\\[0:0:0:0:0:0:0:1]\share\skills"),
            Path::new(r"relative\skills"),
        ] {
            assert!(
                !codex_root_path_uri_is_ordinary(opaque),
                "{}",
                opaque.display()
            );
        }
    }

    #[cfg(windows)]
    #[test]
    fn windows_planned_preferred_root_rejects_an_opaque_canonical_anchor() {
        use super::resolve_destination;
        use crate::mount::resolve::{PathKind, ResolvedEntry};

        let project = Path::new(r"\\?\Volume{01234567-89ab-cdef-0123-456789abcdef}\project");
        let preferred = ResolvedEntry {
            entry: project.join(".agents/skills"),
            kind: PathKind::Missing,
            link_chain: Vec::new(),
            terminal: None,
        };

        let error = resolve_destination(project, &preferred)
            .expect_err("the future Codex root would receive an opaque PathUri");

        assert!(error.to_string().contains("ordinary file URI"), "{error}");
    }
}
