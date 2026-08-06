//! OMP 17.2.9 provider scanning, destination snapshot, and lock resources.
//!
//! Provider order, root paths, entry layout, description requirements, and filters reproduce the
//! contract recorded in ADR 0034. Traversal is one directory level per root, no-follow at the
//! classification boundary, and never recursive.

use std::collections::{BTreeMap, BTreeSet};
use std::ffi::{OsStr, OsString};
use std::fs;
use std::path::{Path, PathBuf};

use super::plugins;
use super::settings::{self, SkillSettings};
use crate::agent::{DiscoveryScope, ExistingSkill, ScopeKind};
use crate::diagnostic::{Diagnostic, DiagnosticKind};
use crate::domain::{OmpAgent, RunContext, SkillNameKey};
use crate::error::{AppError, PlanError};
use crate::lock::{LockResource, LockResourceKind};
use crate::mount::resolve::{PathKind, ResolvedEntry, classify};
use crate::paths::OMP_CONFIG_DIR_NAME;

/// Relative discovery entry an OMP session mounts into.
pub(super) const DESTINATION_SUFFIX: &str = ".omp/skills";
/// Maximum bytes read from one `SKILL.md` while building the conflict inventory.
const MAX_SKILL_MD_BYTES: u64 = 256 * 1024;
/// Maximum directory entries inspected in one OMP Skill root.
///
/// OMP itself is unbounded here. A bound keeps planning finite without changing any outcome for a
/// root of realistic size, and crossing it fails closed rather than truncating the inventory.
const MAX_ROOT_ENTRIES: usize = 20_000;
/// Maximum provider roots inspected in one session.
///
/// The registered providers and the ancestor walk are bounded by path depth, but
/// `skills.customDirectories` and the extension registries are arrays in untrusted documents, so
/// their length would otherwise decide how much work planning does. Each root costs a `classify`, a
/// `read_dir`, a retained scope, and a lock resource, and the whole fan-out is paid once per
/// inspection. Crossing the bound fails closed for the same reason [`MAX_ROOT_ENTRIES`] does.
const MAX_PROVIDER_ROOTS: usize = 4_096;

/// One OMP provider root, in the order OMP scans it.
struct ProviderRoot {
    scope: ScopeKind,
    provider: &'static str,
    project_level: bool,
    path: PathBuf,
    requires_description: bool,
    includes_self: bool,
}

/// Everything one OMP inspection observed.
///
/// The merged settings are deliberately absent. Building a plan from them would mix this
/// observation with the one whose lock set was verified, so a caller that needs them reads them
/// separately through [`load_settings`].
pub(super) struct Inspection {
    pub(super) scopes: Vec<DiscoveryScope>,
    pub(super) destination: PathBuf,
    pub(super) destination_state: ResolvedEntry,
    pub(super) lock_resources: Vec<LockResource>,
    pub(super) warnings: Vec<Diagnostic>,
}

/// Loads only the Skill-affecting OMP settings, without scanning any provider root.
///
/// The visibility gate needs the merged settings but not the namespace, and every settings input is
/// inside the session's lock set, so this reads far less than a full [`inspect`].
///
/// # Errors
///
/// Returns an error when a settings layer cannot be read or is malformed.
pub(super) fn load_settings(context: &RunContext) -> Result<SkillSettings, AppError> {
    let omp = context.agent.omp()?;
    settings::load(&omp.agent_dir, &context.launch_cwd)
}

/// Inspects the complete effective OMP Skill namespace without modifying any of it.
///
/// # Errors
///
/// Returns an error when a settings layer, plugin manifest, or Skill root cannot be inspected, or
/// when a root's contribution cannot be proven from declarative state.
pub(super) fn inspect(context: &RunContext) -> Result<Inspection, AppError> {
    let omp = context.agent.omp()?;
    let settings = settings::load(&omp.agent_dir, &context.launch_cwd)?;
    let plugin_roots = plugins::resolve(
        &omp.agent_dir,
        &omp.plugins_dir,
        &omp.user_home,
        &context.launch_cwd,
    )?;

    let scope_directory = context.launch_cwd.join(OMP_CONFIG_DIR_NAME);
    let scope_state = classify(&scope_directory)?;
    reject_unusable_namespace(&scope_state)?;
    let destination = context.launch_cwd.join(DESTINATION_SUFFIX);
    let destination_state = classify(&destination)?;
    reject_unusable_namespace(&destination_state)?;

    let mut scopes = Vec::new();
    let mut warnings = Vec::new();
    let mut claimed: BTreeSet<SkillNameKey> = BTreeSet::new();
    let mut physical: BTreeSet<PathBuf> = BTreeSet::new();

    // `skills.enabled == false` means OMP discovers nothing at all, so a mount would be inert. The
    // adapter still reports the destination so diagnostics can explain the empty namespace.
    let roots = if settings.enabled {
        provider_roots(omp, context, &plugin_roots, &settings)?
    } else {
        warnings.push(Diagnostic::warning_with_kind(
            DiagnosticKind::General,
            "OMP setting skills.enabled is false, so this release plans no visible Skill; the \
             session would start with an empty Skill namespace"
                .to_owned(),
            omp.agent_dir.clone(),
        ));
        Vec::new()
    };

    for root in roots {
        let mut scope = scan(&root, &settings, &mut claimed)?;
        for warning in &mut scope.warnings {
            warning.kind = DiagnosticKind::General;
        }
        if let Some(terminal) = &scope.state.terminal {
            physical.insert(terminal.clone());
        }
        scopes.push(scope);
    }

    let lock_resources = lock_resources(
        context,
        &destination,
        &destination_state,
        &scopes,
        &settings,
        &plugin_roots,
        &physical,
    )?;

    for scope in &scopes {
        warnings.extend(scope.warnings.iter().cloned());
    }

    Ok(Inspection {
        scopes,
        destination,
        destination_state,
        lock_resources,
        warnings,
    })
}

/// Refuses a destination path that cannot hold, or be replaced by, a Skill namespace.
///
/// A broken, cyclic, over-deep, or non-directory entry has no identity a later mutation could rely
/// on. Planning a directory over it would describe a change apply must then refuse, so the session
/// fails here with the exact chain instead.
fn reject_unusable_namespace(state: &ResolvedEntry) -> Result<(), AppError> {
    if state.kind.is_usable_namespace() {
        return Ok(());
    }
    Err(PlanError::UnsupportedLayout {
        path: state.entry.clone(),
        reason: format!(
            "the OMP session destination resolves as {} rather than a missing path, a directory, or \
             a directory link, so no safe mount destination exists; account for and repair that \
             entry before retrying",
            state.kind.label()
        ),
    }
    .into())
}

/// Builds every provider root in OMP's effective priority and registration order.
fn provider_roots(
    omp: &OmpAgent,
    context: &RunContext,
    plugin_roots: &plugins::PluginRoots,
    settings: &SkillSettings,
) -> Result<Vec<ProviderRoot>, AppError> {
    let boundary = walk_boundary(&context.launch_cwd, &omp.user_home)?;
    let ancestors = ancestors_to(&context.launch_cwd, boundary.as_deref());
    let mut roots = Vec::new();

    // 1. native (priority 100): project ancestors nearest first, then the user agent directory.
    for (index, ancestor) in ancestors.iter().enumerate() {
        roots.push(ProviderRoot {
            scope: if index == 0 {
                ScopeKind::OmpProject
            } else {
                ScopeKind::OmpAncestor
            },
            provider: "native",
            project_level: true,
            path: ancestor.join(DESTINATION_SUFFIX),
            requires_description: true,
            includes_self: false,
        });
    }
    roots.push(ProviderRoot {
        scope: ScopeKind::OmpUser,
        provider: "native",
        project_level: false,
        path: omp.agent_dir.join("skills"),
        requires_description: true,
        includes_self: false,
    });

    // 2. omp-plugins (90).
    for root in &plugin_roots.omp {
        roots.push(ProviderRoot {
            scope: ScopeKind::OmpPlugin,
            provider: "omp-plugins",
            project_level: root.project_level,
            path: root.skills_dir.clone(),
            requires_description: true,
            includes_self: root.includes_self,
        });
    }

    // 3. claude (80).
    roots.push(compatibility_root(
        "claude",
        false,
        omp.user_home.join(".claude/skills"),
    ));
    for ancestor in &ancestors {
        if *ancestor == omp.user_home {
            continue;
        }
        roots.push(compatibility_root(
            "claude",
            true,
            ancestor.join(".claude/skills"),
        ));
    }

    // 4. claude-plugins (70).
    for root in &plugin_roots.claude {
        roots.push(ProviderRoot {
            scope: ScopeKind::OmpPlugin,
            provider: "claude-plugins",
            project_level: root.project_level,
            path: root.skills_dir.clone(),
            requires_description: false,
            includes_self: root.includes_self,
        });
    }

    roots.extend(compatibility_roots(omp, context, &ancestors));

    // 9. omp-managed (5), dead last, always enabled, description required.
    roots.push(ProviderRoot {
        scope: ScopeKind::OmpManaged,
        provider: "omp-managed",
        project_level: false,
        path: omp.agent_dir.join("managed-skills"),
        requires_description: true,
        includes_self: false,
    });

    // `skills.customDirectories` is not a provider. It is scanned after every provider and its
    // entries override a same-named provider Skill, so it is appended last and its scopes are
    // allowed to claim a name another scope already claimed.
    for directory in &settings.custom_directories {
        roots.push(ProviderRoot {
            scope: ScopeKind::OmpCustom,
            provider: "custom",
            project_level: false,
            path: expand_home(directory, &omp.user_home),
            requires_description: true,
            includes_self: false,
        });
    }

    if roots.len() > MAX_PROVIDER_ROOTS {
        return Err(AppError::MissingInput {
            path: context.launch_cwd.clone(),
            reason: format!(
                "OMP configuration names more than {MAX_PROVIDER_ROOTS} Skill roots, so this \
                 release cannot prove a complete conflict inventory"
            ),
        });
    }
    Ok(roots)
}

/// Builds the compatibility-provider roots OMP reads for other Agents' layouts.
///
/// Priority order is `agents` and `codex` at 70, `opencode` at 55, then `github` at 30. Within one
/// provider the order is OMP's own: `.agent` before `.agents`, and project scans before user scans
/// where that provider walks ancestors at all.
fn compatibility_roots(
    omp: &OmpAgent,
    context: &RunContext,
    ancestors: &[PathBuf],
) -> Vec<ProviderRoot> {
    let mut roots = Vec::new();

    for ancestor in ancestors {
        if *ancestor == omp.user_home {
            continue;
        }
        for candidate in [".agent/skills", ".agents/skills"] {
            roots.push(compatibility_root("agents", true, ancestor.join(candidate)));
        }
    }
    for candidate in [".agent/skills", ".agents/skills"] {
        roots.push(compatibility_root(
            "agents",
            false,
            omp.user_home.join(candidate),
        ));
    }

    // The codex project scope is the launch CWD only, with no walk up.
    roots.push(compatibility_root(
        "codex",
        false,
        omp.user_home.join(".codex/skills"),
    ));
    roots.push(compatibility_root(
        "codex",
        true,
        context.launch_cwd.join(".codex/skills"),
    ));

    roots.push(compatibility_root(
        "opencode",
        false,
        omp.user_home.join(".config/opencode/skills"),
    ));
    roots.push(compatibility_root(
        "opencode",
        true,
        context.launch_cwd.join(".opencode/skills"),
    ));

    // `github` is project only and, unlike the others, requires a description.
    roots.push(ProviderRoot {
        scope: ScopeKind::OmpCompatibility,
        provider: "github",
        project_level: true,
        path: context.launch_cwd.join(".github/skills"),
        requires_description: true,
        includes_self: false,
    });

    roots
}

fn compatibility_root(provider: &'static str, project_level: bool, path: PathBuf) -> ProviderRoot {
    ProviderRoot {
        scope: ScopeKind::OmpCompatibility,
        provider,
        project_level,
        path,
        requires_description: false,
        includes_self: false,
    }
}

/// Expands a leading `~` the way OMP expands a custom-directory entry.
fn expand_home(entry: &str, user_home: &Path) -> PathBuf {
    if let Some(relative) = entry.strip_prefix("~/") {
        return user_home.join(relative);
    }
    if entry == "~" {
        return user_home.to_path_buf();
    }
    PathBuf::from(entry)
}

/// Returns OMP's own ancestor-walk boundary: the nearest repository root, else the user home.
fn walk_boundary(launch_cwd: &Path, user_home: &Path) -> Result<Option<PathBuf>, AppError> {
    for ancestor in launch_cwd.ancestors() {
        if classify(&ancestor.join(".git"))?.kind != PathKind::Missing {
            return Ok(Some(ancestor.to_path_buf()));
        }
    }
    Ok(Some(user_home.to_path_buf()))
}

/// Collects `launch_cwd` and its ancestors up to and including `boundary`.
///
/// The boundary directory is itself scanned, and a launch CWD outside the boundary walks to the
/// filesystem root, exactly as OMP's loop does.
fn ancestors_to(launch_cwd: &Path, boundary: Option<&Path>) -> Vec<PathBuf> {
    let mut ancestors = Vec::new();
    for ancestor in launch_cwd.ancestors() {
        ancestors.push(ancestor.to_path_buf());
        if boundary == Some(ancestor) {
            break;
        }
    }
    ancestors
}

/// Scans one OMP Skill root, one directory level, applying that root's own rules.
fn scan(
    root: &ProviderRoot,
    settings: &SkillSettings,
    claimed: &mut BTreeSet<SkillNameKey>,
) -> Result<DiscoveryScope, AppError> {
    let state = classify(&root.path)?;
    let mut scope = DiscoveryScope {
        kind: root.scope,
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
    if let Some(terminal) = &scope.state.terminal {
        scope.observed_directories.push(terminal.clone());
    }

    let candidates = candidates_in(root)?;
    // A disabled source contributes no logical name. Its filesystem entries are still never
    // mutated, and the destination's own direct occupancy is recorded regardless.
    let source_enabled = settings.source_enabled(root.provider, root.project_level);

    for (entry, raw_name) in candidates {
        let skill_md = entry.join("SKILL.md");
        // A root without `SKILL.md` is silently not a Skill in OMP.
        if classify(&skill_md)?.kind == PathKind::Missing {
            continue;
        }
        let metadata =
            read_metadata(&skill_md, &raw_name).map_err(|reason| AppError::MissingInput {
                path: skill_md.clone(),
                reason: format!(
                    "cannot prove the OMP Skill identity of this entry, so the conflict inventory \
                     would be incomplete: {reason}"
                ),
            })?;
        let Some(metadata) = metadata else {
            // `enabled: false`, or a missing description where the provider requires one.
            continue;
        };
        if root.requires_description && metadata.description.is_none() {
            continue;
        }

        let key = SkillNameKey::new(OsStr::new(&metadata.name));
        let entry_state = classify(&entry)?;
        let existing = ExistingSkill {
            comparison_key: key.clone(),
            raw_name: OsString::from(&metadata.name),
            entry: entry.clone(),
            kind: entry_state.kind,
            source_canonical: entry_state.terminal,
        };

        // Direct occupancy answers whether a destination path is physically free and is recorded
        // under the on-disk entry name, independent of the logical OMP name.
        let direct_key = SkillNameKey::new(&raw_name);
        scope
            .direct_entries
            .entry(direct_key)
            .or_default()
            .push(ExistingSkill {
                comparison_key: SkillNameKey::new(&raw_name),
                raw_name: raw_name.clone(),
                entry: entry.clone(),
                kind: existing.kind,
                source_canonical: existing.source_canonical.clone(),
            });

        if !source_enabled {
            continue;
        }
        if !settings.name_visible(&metadata.name) {
            // An explicit operator filter hiding an entry that exists on disk is the one filter
            // decision worth naming: without it, a same-named entry that quietly stops being a
            // conflict looks like SkillMount overlooked it.
            scope.warnings.push(Diagnostic::warning_with_kind(
                DiagnosticKind::General,
                format!(
                    "OMP configuration hides existing Skill {} in this scope through \
                     disabledExtensions, skills.ignoredSkills, or skills.includeSkills, so it does \
                     not claim that logical name",
                    metadata.name
                ),
                entry.clone(),
            ));
            continue;
        }
        // OMP resolves auto-learned Skills dead last and always defers them to a same-named
        // enabled authored Skill. A mounted Skill is exactly that, so a managed entry can never
        // shadow one; counting it as a conflict would fail a session OMP would have satisfied, and
        // `--conflict=skip` would omit a Skill that actually wins. Its directory is outside every
        // destination, so excluding it authorizes no mutation.
        if root.provider == "omp-managed" {
            scope.warnings.push(Diagnostic::warning_with_kind(
                DiagnosticKind::General,
                format!(
                    "OMP auto-learned Skill {} defers to any same-named authored Skill, so it is \
                     not treated as a conflict",
                    metadata.name
                ),
                entry.clone(),
            ));
            continue;
        }
        // OMP's dedup is first wins across providers, except that a custom directory overrides an
        // already-seen provider Skill.
        let custom = root.provider == "custom";
        if !claimed.insert(key.clone()) && !custom {
            continue;
        }
        scope.existing_skills.entry(key).or_default().push(existing);
    }

    Ok(scope)
}

/// Enumerates the entries one OMP Skill root admits, one directory level, in path order.
///
/// OMP skips a dotted name, admits a directory or a symbolic link and then follows it — which is
/// what makes a transaction-owned directory link loadable — and reads nothing but
/// `<entry>/SKILL.md`.
fn candidates_in(root: &ProviderRoot) -> Result<Vec<(PathBuf, OsString)>, AppError> {
    let mut candidates = Vec::new();
    if root.includes_self {
        candidates.push((root.path.clone(), directory_name(&root.path)));
    }
    let entries = fs::read_dir(&root.path).map_err(|error| AppError::MissingInput {
        path: root.path.clone(),
        reason: format!("cannot enumerate the OMP Skill root: {error}"),
    })?;
    let mut inspected = 0usize;
    for child in entries {
        let child = child.map_err(|error| AppError::MissingInput {
            path: root.path.clone(),
            reason: format!("cannot enumerate the OMP Skill root: {error}"),
        })?;
        inspected += 1;
        if inspected > MAX_ROOT_ENTRIES {
            return Err(AppError::MissingInput {
                path: root.path.clone(),
                reason: format!(
                    "OMP Skill root holds more than {MAX_ROOT_ENTRIES} entries, so this release \
                     cannot prove a complete conflict inventory"
                ),
            });
        }
        let raw_name = child.file_name();
        if raw_name.as_encoded_bytes().first() == Some(&b'.') {
            continue;
        }
        if !matches!(
            classify(&child.path())?.kind,
            PathKind::Directory | PathKind::DirectoryLink
        ) {
            continue;
        }
        candidates.push((child.path(), raw_name));
    }
    candidates.sort_by(|left, right| left.0.cmp(&right.0));
    Ok(candidates)
}

/// The OMP frontmatter facts that decide a Skill's identity and visibility.
struct SkillFrontmatter {
    name: String,
    description: Option<String>,
}

/// Reads the OMP frontmatter contract from one `SKILL.md`.
///
/// Returns `Ok(None)` when OMP would drop the entry through `enabled: false`. A read that cannot
/// establish the effective name is an error, because an unnamed entry would silently leave the
/// conflict inventory incomplete.
fn read_metadata(skill_md: &Path, raw_name: &OsStr) -> Result<Option<SkillFrontmatter>, String> {
    let bytes = crate::catalog::frontmatter::read_bounded_regular_file(
        skill_md,
        "SKILL.md",
        MAX_SKILL_MD_BYTES,
    )?;
    let content = String::from_utf8(bytes).map_err(|_| "SKILL.md is not valid UTF-8".to_owned())?;
    let fallback = raw_name.to_str().ok_or_else(|| {
        "the containing directory name is not Unicode, so OMP's name fallback cannot be reproduced"
            .to_owned()
    })?;

    let Some(raw) = envelope(&content) else {
        // No frontmatter envelope at all: OMP falls back to the directory name and sees no
        // description.
        return Ok(Some(SkillFrontmatter {
            name: fallback.to_owned(),
            description: None,
        }));
    };

    let (enabled, name, description) =
        if let Ok(value) = serde_yaml_ng::from_str::<serde_yaml_ng::Value>(&raw) {
            let mapping = value.as_mapping();
            (
                mapping
                    .and_then(|mapping| mapping.get("enabled"))
                    .and_then(serde_yaml_ng::Value::as_bool),
                mapping
                    .and_then(|mapping| mapping.get("name"))
                    .and_then(|value| value.as_str().map(str::to_owned)),
                mapping
                    .and_then(|mapping| mapping.get("description"))
                    .and_then(|value| value.as_str().map(str::to_owned)),
            )
        } else {
            // OMP warns and then applies a line-wise `key: value` fallback rather than dropping the
            // Skill, so an invalid envelope still yields an identity.
            let scalars = line_scalars(&raw);
            (
                scalars.get("enabled").map(|value| value == "true"),
                scalars.get("name").cloned(),
                scalars.get("description").cloned(),
            )
        };

    if enabled == Some(false) {
        return Ok(None);
    }
    let name = name
        .map(|name| name.trim().to_owned())
        .filter(|name| !name.is_empty())
        .unwrap_or_else(|| fallback.to_owned());
    Ok(Some(SkillFrontmatter {
        name,
        description: description
            .map(|value| value.trim().to_owned())
            .filter(|value| !value.is_empty()),
    }))
}

/// Extracts OMP's frontmatter envelope: a leading `---` and the first following `\n---`.
fn envelope(content: &str) -> Option<String> {
    let normalized = content.replace("\r\n", "\n");
    let body = normalized.strip_prefix("---")?;
    let end = body.find("\n---")?;
    // OMP replaces a tab with two spaces before parsing, because YAML forbids tab indentation.
    Some(body[..end].replace('\t', "  "))
}

/// Applies OMP's line-wise `key: value` fallback for an unparseable envelope.
fn line_scalars(raw: &str) -> BTreeMap<String, String> {
    let mut scalars = BTreeMap::new();
    for line in raw.lines() {
        let Some((key, value)) = line.split_once(':') else {
            continue;
        };
        let key = key.trim();
        if key.is_empty()
            || !key
                .chars()
                .all(|token| token.is_alphanumeric() || token == '-' || token == '_')
        {
            continue;
        }
        scalars.insert(key.to_owned(), value.trim().trim_matches('"').to_owned());
    }
    scalars
}

fn directory_name(path: &Path) -> OsString {
    path.file_name().unwrap_or(OsStr::new("")).to_os_string()
}

/// Returns the missing `.omp` and `skills` directories a plan must create, outermost first.
///
/// The destination kind is taken as a value rather than read again, so a caller holding a snapshot
/// builds the chain from that same observation instead of a fresh one.
pub(super) fn missing_destination_chain(
    launch_cwd: &Path,
    destination_kind: PathKind,
) -> Vec<PathBuf> {
    if matches!(
        destination_kind,
        PathKind::Directory | PathKind::DirectoryLink
    ) {
        return Vec::new();
    }
    let mut missing = Vec::new();
    let scope = launch_cwd.join(OMP_CONFIG_DIR_NAME);
    if !scope.is_dir() {
        missing.push(scope.clone());
    }
    missing.push(scope.join("skills"));
    missing
}

/// Collects every logical and physical resource an OMP session must lock.
fn lock_resources(
    context: &RunContext,
    destination: &Path,
    destination_state: &ResolvedEntry,
    scopes: &[DiscoveryScope],
    settings: &SkillSettings,
    plugin_roots: &plugins::PluginRoots,
    physical: &BTreeSet<PathBuf>,
) -> Result<Vec<LockResource>, AppError> {
    let mut resources = Vec::new();
    for scope in scopes {
        resources.push(if scope.state.entry.starts_with(&context.project_root) {
            LockResource::describe_entry(
                LockResourceKind::DiscoveryEntry,
                &context.project_root,
                &scope.state,
            )?
        } else {
            LockResource::describe_unanchored(LockResourceKind::DiscoveryEntry, &scope.state.entry)
        });
    }
    // Every declarative input that decided the namespace is a resource too: the locked replan
    // rereads them, so a concurrent session must not be able to rewrite one in between.
    for input in settings.inputs.iter().chain(plugin_roots.inputs.iter()) {
        resources.push(LockResource::describe_unanchored(
            LockResourceKind::DiscoveryEntry,
            input,
        ));
    }
    for terminal in physical {
        resources.push(LockResource::describe_unanchored(
            LockResourceKind::DiscoveryEntry,
            terminal,
        ));
    }
    // The project scope is locked as well, because a plan may create it.
    let scope_directory = context.launch_cwd.join(OMP_CONFIG_DIR_NAME);
    resources.push(LockResource::describe(
        LockResourceKind::DiscoveryEntry,
        &context.project_root,
        &scope_directory,
    )?);
    resources.push(LockResource::describe(
        LockResourceKind::BackingStore,
        &context.project_root,
        destination,
    )?);
    // A destination reached through a link shares one physical directory with anything else that
    // links to it, so the canonical backing path contributes its own key.
    if let Some(terminal) = &destination_state.terminal {
        resources.push(LockResource::describe_unanchored(
            LockResourceKind::BackingStore,
            terminal,
        ));
    }

    resources.sort_by_key(LockResource::ordering_key);
    resources.dedup();
    Ok(resources)
}

#[cfg(test)]
mod tests {
    use super::{ancestors_to, envelope, expand_home, line_scalars};
    use std::path::{Path, PathBuf};

    #[test]
    fn the_walk_includes_the_boundary_and_stops_there() {
        let launch = PathBuf::from("/work/repo/nested/deep");
        let boundary = PathBuf::from("/work/repo");
        assert_eq!(
            ancestors_to(&launch, Some(&boundary)),
            [
                PathBuf::from("/work/repo/nested/deep"),
                PathBuf::from("/work/repo/nested"),
                PathBuf::from("/work/repo"),
            ]
        );
    }

    #[test]
    fn a_launch_cwd_outside_the_boundary_walks_to_the_filesystem_root() {
        let walk = ancestors_to(Path::new("/tmp/scratch"), Some(Path::new("/home/user")));
        assert_eq!(walk.last(), Some(&PathBuf::from("/")));
    }

    #[test]
    fn a_custom_directory_expands_a_leading_home_marker_only() {
        let home = Path::new("/home/user");
        assert_eq!(expand_home("~/skills", home), home.join("skills"));
        assert_eq!(expand_home("~", home), home);
        assert_eq!(
            expand_home("/abs/skills", home),
            PathBuf::from("/abs/skills")
        );
        assert_eq!(
            expand_home("rel/~/skills", home),
            PathBuf::from("rel/~/skills"),
            "a tilde that is not the first component stays literal"
        );
    }

    #[test]
    fn the_envelope_ends_at_the_first_closing_delimiter_and_untabs() {
        assert_eq!(
            envelope("---\nname: demo\n---\nbody\n").as_deref(),
            Some("\nname: demo")
        );
        assert_eq!(
            envelope("---\r\nname: demo\r\n---\r\nbody\r\n").as_deref(),
            Some("\nname: demo"),
            "CRLF is normalized before the envelope is cut"
        );
        assert_eq!(
            envelope("---\n\tname: demo\n---\n").as_deref(),
            Some("\n  name: demo")
        );
        assert_eq!(envelope("no frontmatter\n"), None);
        assert_eq!(
            envelope("---\nname: demo\n"),
            None,
            "an unterminated envelope"
        );
    }

    #[test]
    fn the_line_wise_fallback_recovers_the_identity_of_an_invalid_envelope() {
        let scalars = line_scalars("name: demo\ndescription: \"a: value\"\nenabled: true\n[bad\n");
        assert_eq!(scalars.get("name").map(String::as_str), Some("demo"));
        assert_eq!(
            scalars.get("description").map(String::as_str),
            Some("a: value")
        );
        assert_eq!(scalars.get("enabled").map(String::as_str), Some("true"));
    }
}
