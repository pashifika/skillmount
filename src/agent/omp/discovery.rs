//! OMP 17.2.9 provider scanning, destination snapshot, and lock resources.
//!
//! Provider order, root paths, entry layout, description requirements, and filters reproduce the
//! contract recorded in ADR 0034. Traversal is one directory level per root, no-follow at the
//! classification boundary, and never recursive.

use std::collections::btree_map::Entry;
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

/// The two folds OMP applies to the discovered superset, carried across every provider root.
#[derive(Default)]
struct Claims {
    /// Canonical `SKILL.md` files already loaded, so one physical Skill is never counted twice.
    physical: BTreeSet<PathBuf>,
    /// Logical names already claimed, and whether the claim came from a custom directory.
    logical: BTreeMap<SkillNameKey, bool>,
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
    let mut claims = Claims::default();
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
        let mut scope = scan(&root, &settings, &mut claims)?;
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
///
/// `expandTilde` (`tools/path-utils.ts:142-152`) has three branches, and `skills.ts:251` applies it
/// to every `skills.customDirectories` entry. Handling only `~/` left `~\my-skills` - the natural
/// Windows spelling - and `~my-skills` as literal relative paths, so the directory contributed
/// nothing to the conflict inventory and nothing to the lock set while OMP still scanned it. A
/// custom directory overrides a same-named provider Skill, so a mount could then be applied,
/// reported as successful, and silently overridden.
fn expand_home(entry: &str, user_home: &Path) -> PathBuf {
    if entry == "~" {
        return user_home.to_path_buf();
    }
    if let Some(relative) = entry
        .strip_prefix("~/")
        .or_else(|| entry.strip_prefix("~\\"))
    {
        return user_home.join(relative);
    }
    if let Some(relative) = entry.strip_prefix('~') {
        return user_home.join(relative);
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
    claims: &mut Claims,
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

    let entries = root_entries(root)?;

    for entry in &entries {
        // Destination occupancy answers whether a path is physically free, so it is recorded for
        // every immediate child under its on-disk name - before OMP's dot-name, entry-kind,
        // `SKILL.md`, `enabled`, and description filters, none of which free the path. This is the
        // same rule `agent::inspect_scope` applies for Codex and Claude. Recording it after those
        // filters left a real directory at the destination invisible to `apply_conflict_policy`,
        // so `--conflict=skip` could not preserve it and `--conflict=error` only failed once the
        // transaction was already open.
        crate::agent::insert_direct_deterministically(
            &mut scope,
            ExistingSkill {
                comparison_key: SkillNameKey::new(&entry.raw_name),
                raw_name: entry.raw_name.clone(),
                entry: entry.path.clone(),
                kind: entry.kind,
                source_canonical: entry.terminal.clone(),
            },
        );
    }

    let mut candidates = Vec::new();
    if root.includes_self {
        candidates.push((root.path.clone(), directory_name(&root.path)));
    }
    candidates.extend(
        entries
            .into_iter()
            .filter(|entry| entry.scannable)
            .map(|entry| (entry.path, entry.raw_name)),
    );
    candidates.sort_by(|left, right| left.0.cmp(&right.0));

    for (entry, raw_name) in candidates {
        let skill_md = entry.join("SKILL.md");
        let Some(metadata) = read_metadata(&skill_md, &raw_name)? else {
            // No readable `SKILL.md` at all - every reason OMP's `readFile` returns null.
            continue;
        };
        if metadata.enabled == Some(false) {
            continue;
        }
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

        if !claim_logical_name(
            root,
            settings,
            claims,
            &Candidate {
                existing: &existing,
                name: &metadata.name,
                skill_md: &skill_md,
            },
            &mut scope.warnings,
        ) {
            continue;
        }
        scope.existing_skills.entry(key).or_default().push(existing);
    }

    Ok(scope)
}

/// One discovered entry, as the claim decision sees it.
struct Candidate<'a> {
    existing: &'a ExistingSkill,
    /// Effective OMP name: the trimmed frontmatter `name`, else the directory basename.
    name: &'a str,
    /// The `SKILL.md` OMP would load, which is also the physical dedup key.
    skill_md: &'a Path,
}

/// Decides whether one discovered entry claims its logical OMP name.
///
/// Returns `false` for every reason OMP would not load the entry under that name, warning where the
/// reason is one an operator would otherwise mistake for an oversight. The entry's own filesystem
/// state is never touched either way; only the claim is withheld.
fn claim_logical_name(
    root: &ProviderRoot,
    settings: &SkillSettings,
    claims: &mut Claims,
    candidate: &Candidate<'_>,
    warnings: &mut Vec<Diagnostic>,
) -> bool {
    let Candidate {
        existing,
        name,
        skill_md,
    } = *candidate;
    let key = &existing.comparison_key;
    if !settings.source_enabled(root.provider, root.project_level) {
        // A disabled source is one reason a same-named entry stops being a conflict. Saying so is
        // what distinguishes "OMP will not see this" from "SkillMount overlooked it".
        warnings.push(Diagnostic::warning_with_kind(
            DiagnosticKind::General,
            format!(
                "OMP source {} is disabled in this scope, so existing Skill {name} is not visible \
                 to OMP and does not claim that logical name",
                root.provider
            ),
            existing.entry.clone(),
        ));
        return false;
    }
    if !settings.name_visible(name) {
        // An explicit operator filter hiding an entry that exists on disk is the other filter
        // decision worth naming.
        warnings.push(Diagnostic::warning_with_kind(
            DiagnosticKind::General,
            format!(
                "OMP configuration hides existing Skill {name} in this scope through \
                 disabledExtensions, skills.ignoredSkills, or skills.includeSkills, so it does not \
                 claim that logical name"
            ),
            existing.entry.clone(),
        ));
        return false;
    }
    // OMP resolves auto-learned Skills dead last and always defers them to a same-named enabled
    // authored Skill. A mounted Skill is exactly that, so a managed entry can never shadow one;
    // counting it as a conflict would fail a session OMP would have satisfied, and
    // `--conflict=skip` would omit a Skill that actually wins. Its directory is outside every
    // destination, so excluding it authorizes no mutation.
    if root.provider == "omp-managed" {
        warnings.push(Diagnostic::warning_with_kind(
            DiagnosticKind::General,
            format!(
                "OMP auto-learned Skill {name} defers to any same-named authored Skill, so it is \
                 not treated as a conflict"
            ),
            existing.entry.clone(),
        ));
        return false;
    }

    // OMP folds the loaded set twice, and the two passes run the folds in opposite orders.
    //
    // The physical key is the `realpath` of the `SKILL.md` file itself (`skills.ts:212` over
    // `capSkill.path`, which `helpers.ts:397` sets to the `SKILL.md` path), not the entry
    // directory: two real directories whose `SKILL.md` are links to one shared file are one Skill
    // to OMP. A failed resolution falls back to the literal path, as OMP's `catch` does.
    let physical = fs::canonicalize(skill_md).unwrap_or_else(|_| skill_md.to_path_buf());
    let custom = root.provider == "custom";

    // The custom pass checks the realpath before it overrides a name (`skills.ts:301-314`).
    if custom && claims.physical.contains(&physical) {
        return false;
    }

    // The logical fold is first-wins by name, with one exception: a custom directory overrides a
    // name already claimed by an ordinary provider (`skills.ts:303-314`), while two custom
    // directories keep first-wins between themselves (`skills.ts:316-318`).
    //
    // For an ordinary provider this fold runs inside the pre-realpath filter
    // (`skills.ts:203-204`), so it comes first and a name-duplicate never reaches - or burns - the
    // realpath set. Recording the realpath before the name fold used to drop a later entry that
    // OMP does load, which under-reports the namespace and lets a mount shadow it.
    let claimed = match claims.logical.entry(key.clone()) {
        Entry::Vacant(slot) => {
            slot.insert(custom);
            true
        }
        Entry::Occupied(mut slot) => {
            if !custom || *slot.get() {
                false
            } else {
                slot.insert(custom);
                true
            }
        }
    };
    if !claimed {
        return false;
    }

    // A realpath is recorded only for an entry that is actually stored (`skills.ts:245`, `:313`,
    // `:322`). The name claim above stays either way, matching `seenAuthoredSkillNames`.
    claims.physical.insert(physical)
}

/// One immediate child of a Skill root, classified once.
struct RootEntry {
    path: PathBuf,
    raw_name: OsString,
    kind: PathKind,
    terminal: Option<PathBuf>,
    /// Whether OMP would look for `<entry>/SKILL.md` here.
    scannable: bool,
}

/// Enumerates every immediate child of one OMP Skill root, one directory level, in path order.
///
/// Every child is returned, because a child that OMP ignores still occupies its path. `scannable`
/// carries OMP's own admission test: it skips a dotted name (`helpers.ts:417`) and admits only a
/// directory or a symbolic link, which it then follows (`helpers.ts:418,420`) — the rule that makes
/// a transaction-owned directory link loadable.
///
/// An unreadable root is not fatal. OMP warns once and contributes nothing from it
/// (`helpers.ts:376-381`), so refusing the whole run would fail a session OMP would have started;
/// the inventory is complete without entries OMP provably never loads.
fn root_entries(root: &ProviderRoot) -> Result<Vec<RootEntry>, AppError> {
    let Ok(entries) = fs::read_dir(&root.path) else {
        return Ok(Vec::new());
    };
    let mut collected = Vec::new();
    for child in entries {
        let Ok(child) = child else {
            continue;
        };
        if collected.len() >= MAX_ROOT_ENTRIES {
            return Err(AppError::MissingInput {
                path: root.path.clone(),
                reason: format!(
                    "OMP Skill root holds more than {MAX_ROOT_ENTRIES} entries, so this release \
                     cannot prove a complete conflict inventory"
                ),
            });
        }
        let raw_name = child.file_name();
        let state = classify(&child.path())?;
        let dotted = raw_name.as_encoded_bytes().first() == Some(&b'.');
        collected.push(RootEntry {
            path: child.path(),
            raw_name,
            kind: state.kind,
            terminal: state.terminal,
            scannable: !dotted
                && matches!(state.kind, PathKind::Directory | PathKind::DirectoryLink),
        });
    }
    collected.sort_by(|left, right| left.path.cmp(&right.path));
    Ok(collected)
}

/// The OMP frontmatter facts that decide a Skill's identity and visibility.
struct SkillFrontmatter {
    name: String,
    description: Option<String>,
    /// `enabled: false` drops the entry (`helpers.ts:387`); absent or any other value keeps it.
    enabled: Option<bool>,
}

/// Returns whether the selected Skill's own frontmatter would make OMP drop it.
///
/// The mount links the source directory into the destination, so the child reads exactly this
/// `SKILL.md`. `enabled: false` there means OMP loads nothing under that name, which is a mount
/// applied and then ignored. Generic catalog validation has no notion of `enabled`, so the check
/// belongs to the adapter that knows the rule.
pub(super) fn selected_is_disabled(skill_md: &Path) -> Result<bool, AppError> {
    let name = directory_name(skill_md.parent().unwrap_or(skill_md));
    Ok(read_metadata(skill_md, &name)?
        .is_some_and(|frontmatter| frontmatter.enabled == Some(false)))
}

/// Reads the OMP frontmatter contract from one `SKILL.md`.
///
/// Returns `Ok(None)` for every reason OMP itself drops the entry silently: no `SKILL.md`, a
/// dangling link, a non-regular file, an unreadable one, an empty one, or `enabled: false`. OMP's
/// `readFile` returns null on any error and for any non-regular path (`capability/fs.ts:23-33`) and
/// `loadSkill` then returns without loading (`discovery/helpers.ts:384-385`). Such an entry is
/// provably absent from OMP's namespace, so skipping keeps the conflict inventory complete, while
/// refusing would fail a session OMP would have started — and these paths are reachable from any of
/// the ~20 scanned roots, including ancestors and project-named plugin roots.
///
/// A file OMP *would* load but this release cannot model stays fatal, because an unmodelled entry
/// would silently leave the inventory incomplete: over [`MAX_SKILL_MD_BYTES`], not UTF-8, or a
/// containing directory name that is not Unicode.
fn read_metadata(skill_md: &Path, raw_name: &OsStr) -> Result<Option<SkillFrontmatter>, AppError> {
    let fatal = |reason: String| AppError::MissingInput {
        path: skill_md.to_path_buf(),
        reason: format!(
            "cannot prove the OMP Skill identity of this entry, so the conflict inventory would be \
             incomplete: {reason}"
        ),
    };

    // `fs::metadata` follows the link, so a dangling `SKILL.md` link reports an error here exactly
    // as `existsSync` reports false for it (`helpers.ts:420`).
    let Ok(state) = fs::metadata(skill_md) else {
        return Ok(None);
    };
    if !state.is_file() || state.len() == 0 {
        return Ok(None);
    }
    if state.len() > MAX_SKILL_MD_BYTES {
        return Err(fatal(format!(
            "SKILL.md exceeds {MAX_SKILL_MD_BYTES} bytes"
        )));
    }

    let bytes = crate::catalog::frontmatter::read_bounded_regular_file(
        skill_md,
        "SKILL.md",
        MAX_SKILL_MD_BYTES,
    )
    .map_err(fatal)?;
    let content =
        String::from_utf8(bytes).map_err(|_| fatal("SKILL.md is not valid UTF-8".to_owned()))?;
    let fallback = raw_name.to_str().ok_or_else(|| {
        fatal(
            "the containing directory name is not Unicode, so OMP's name fallback cannot be \
             reproduced"
                .to_owned(),
        )
    })?;

    let Some(raw) = envelope(&content) else {
        // No frontmatter envelope at all: OMP falls back to the directory name and sees no
        // description.
        return Ok(Some(SkillFrontmatter {
            name: fallback.to_owned(),
            description: None,
            enabled: None,
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

    let name = name
        .map(|name| name.trim().to_owned())
        .filter(|name| !name.is_empty())
        .unwrap_or_else(|| fallback.to_owned());
    Ok(Some(SkillFrontmatter {
        name,
        description: description
            .map(|value| value.trim().to_owned())
            .filter(|value| !value.is_empty()),
        enabled,
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
        let access = if scope.state.entry == destination {
            crate::lock::LockAccess::Mutate
        } else {
            crate::lock::LockAccess::Observe
        };
        let legacy_anchor = scope
            .state
            .entry
            .starts_with(&context.project_root)
            .then_some(context.project_root.as_path());
        resources.extend(LockResource::describe_shared_and_legacy_entry(
            access,
            legacy_anchor,
            &scope.state,
        )?);
    }
    // Declarative settings, plugin registries, and traversed roots only decide the observed
    // namespace. SkillMount never rewrites them.
    for input in settings.inputs.iter().chain(plugin_roots.inputs.iter()) {
        resources.extend(LockResource::describe_shared_and_legacy_unanchored(
            LockResourceKind::DiscoveryEntry,
            crate::lock::LockAccess::Observe,
            input,
        )?);
    }
    for terminal in physical {
        resources.extend(LockResource::describe_shared_and_legacy_unanchored(
            LockResourceKind::DiscoveryEntry,
            crate::lock::LockAccess::Observe,
            terminal,
        )?);
    }
    // The project scope is locked as well, because a plan may create it.
    let scope_directory = context.launch_cwd.join(OMP_CONFIG_DIR_NAME);
    resources.push(LockResource::describe_shared(
        LockResourceKind::DiscoveryEntry,
        crate::lock::LockAccess::Mutate,
        &scope_directory,
    )?);
    resources.push(LockResource::describe(
        LockResourceKind::DiscoveryEntry,
        crate::lock::LockAccess::Mutate,
        &context.project_root,
        &scope_directory,
    )?);
    resources.push(LockResource::describe(
        LockResourceKind::BackingStore,
        crate::lock::LockAccess::Mutate,
        &context.project_root,
        destination,
    )?);
    // A destination reached through a link shares one physical directory with anything else that
    // links to it, so the canonical backing path contributes its own key.
    if let Some(terminal) = &destination_state.terminal {
        resources.push(LockResource::describe_shared(
            LockResourceKind::BackingStore,
            crate::lock::LockAccess::Mutate,
            terminal,
        )?);
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
    fn a_custom_directory_expands_every_tilde_form_omp_expands() {
        // `expandTilde` (`tools/path-utils.ts:142-152`) has three branches. Handling only `~/`
        // left the natural Windows spelling `~\x` and the bare `~name` form as literal relative
        // paths, so that directory contributed nothing to the conflict inventory and nothing to the
        // lock set while OMP still scanned it.
        let home = Path::new("/home/user");
        assert_eq!(expand_home("~", home), home);
        assert_eq!(expand_home("~/skills", home), home.join("skills"));
        assert_eq!(expand_home("~\\skills", home), home.join("skills"));
        assert_eq!(expand_home("~skills", home), home.join("skills"));
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
    fn every_immediate_child_is_returned_and_only_some_are_scannable() {
        // Destination occupancy answers whether a path is free, so a dot-prefixed name and a plain
        // file must both be reported even though OMP never looks for a `SKILL.md` inside them
        // (`helpers.ts:417-418`). Recording occupancy only for scannable entries left a real
        // occupant invisible to `apply_conflict_policy`, so `--conflict=skip` could not preserve it
        // and `--conflict=error` only failed once the transaction was already open. The dotted case
        // is unreachable from an integration fixture, because a portable mount name can never begin
        // with a dot.
        let fixture = crate::test_support::TestDir::new("omp-root-entries");
        let root = fixture.0.join("skills");
        std::fs::create_dir_all(root.join("visible")).expect("visible entry");
        std::fs::create_dir_all(root.join(".hidden")).expect("dotted entry");
        std::fs::write(root.join("plain.txt"), b"x").expect("regular file entry");

        let entries = super::root_entries(&super::ProviderRoot {
            scope: crate::agent::ScopeKind::OmpProject,
            provider: "native",
            project_level: true,
            path: root,
            requires_description: true,
            includes_self: false,
        })
        .expect("the root enumerates");

        let reported: Vec<_> = entries
            .iter()
            .map(|entry| {
                (
                    entry.raw_name.to_string_lossy().into_owned(),
                    entry.scannable,
                )
            })
            .collect();
        assert_eq!(
            reported,
            [
                (".hidden".to_owned(), false),
                ("plain.txt".to_owned(), false),
                ("visible".to_owned(), true),
            ],
            "every child is reported for occupancy; only a non-dotted directory is scanned"
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
