//! Static enumeration of the OMP extension-package and marketplace-plugin Skill roots.
//!
//! Every input is JSON or a directory listing. No plugin, extension, or hook code is ever imported
//! or executed: in OMP 17.2.9 a package's complete Skill contribution is derivable from
//! `package.json`, `omp-plugins.lock.json`, `plugin-overrides.json`, `installed_plugins.json`,
//! `.claude-plugin/plugin.json`, `marketplace.json`, and one `readdir`. See ADR 0034.

use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};

use super::settings::read_regular;
use crate::error::AppError;
use crate::mount::resolve::{PathKind, classify};
use crate::paths::OMP_CONFIG_DIR_NAME;

/// Maximum install paths admitted from one plugin registry.
///
/// OMP is unbounded here, but the project-level registry lives inside the repository, so its length
/// would otherwise decide how many roots `SkillMount` classifies, reads, reports, and locks.
const MAX_REGISTRY_ROOTS: usize = 1_024;

/// Upper bound on the Skill directories one plugin manifest may declare.
///
/// The manifest is attacker-authored and [`MAX_REGISTRY_ROOTS`] distinct plugin ids may all name
/// the same `installPath`, so an unbounded `skills` array is multiplied by the registry fan-out.
/// The total bound in `discovery::provider_roots` is enforced only after every root exists, which
/// is too late to stop the allocation, so each manifest is bounded where it is parsed.
const MAX_MANIFEST_SKILL_DIRS: usize = 256;

/// Upper bound on the package-backed roots one run may accumulate across every registry.
///
/// Fails closed with the same incomplete-inventory reason before the roots reach discovery.
const MAX_PACKAGE_ROOTS: usize = 4_096;

/// One package root whose sibling `skills/` an OMP provider scans.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) struct PluginSkillRoot {
    /// Directory the provider scans for `<entry>/SKILL.md`.
    pub(super) skills_dir: PathBuf,
    /// Whether OMP labels this root project-level.
    pub(super) project_level: bool,
    /// Whether `<skills_dir>/SKILL.md` itself is also a Skill.
    pub(super) includes_self: bool,
}

/// Everything the adapter needs about OMP's package-backed Skill roots.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub(super) struct PluginRoots {
    /// Roots the `omp-plugins` provider scans, in OMP's own order.
    pub(super) omp: Vec<PluginSkillRoot>,
    /// Roots the `claude-plugins` provider scans, in OMP's own order.
    pub(super) claude: Vec<PluginSkillRoot>,
    /// Every declarative input that decided the result, for lock resources and diagnostics.
    pub(super) inputs: Vec<PathBuf>,
}

/// One `installed_plugins.json` entry that survived the enablement checks.
#[derive(Debug, Clone)]
struct MarketplaceRoot {
    id: String,
    plugin: String,
    path: PathBuf,
    project_level: bool,
}

/// Resolves every package-backed Skill root OMP would scan.
///
/// # Errors
///
/// Returns [`AppError::MissingInput`] when a declared root exists but cannot be inspected, and
/// [`AppError::Internal`] when an enabled package's contribution cannot be proven from declarative
/// state — an unsupported environment rather than a guess.
pub(super) fn resolve(
    user_agent_dir: &Path,
    user_plugins_dir: &Path,
    user_home: &Path,
    launch_cwd: &Path,
) -> Result<PluginRoots, AppError> {
    let mut roots = PluginRoots::default();
    let project_plugins_dir = project_plugins_dir(launch_cwd, user_home)?;

    let claude = marketplace_roots(
        user_home,
        user_plugins_dir,
        project_plugins_dir.as_deref(),
        &mut roots.inputs,
    )?;
    let mut excluded = BTreeSet::new();
    for root in &claude {
        excluded.insert(canonical_identity(&root.path));
        for skills_dir in claude_skills_dirs(root, &mut roots.inputs)? {
            roots.claude.push(skills_dir);
            // Bound while accumulating, not after: the registry fan-out multiplies each
            // manifest's declarations, so a check that runs once at the end allocates first.
            if roots.claude.len() > MAX_PACKAGE_ROOTS {
                return Err(too_many_package_roots(&root.path));
            }
        }
    }

    let mut seen = BTreeSet::new();
    for declared in declared_extension_roots(user_agent_dir, launch_cwd, &mut roots.inputs)? {
        push_omp_root(&mut roots.omp, &mut seen, &excluded, declared)?;
    }
    for installed in installed_package_roots(
        user_plugins_dir,
        project_plugins_dir.as_deref(),
        launch_cwd,
        &mut roots.inputs,
    )? {
        push_omp_root(&mut roots.omp, &mut seen, &excluded, installed)?;
    }
    if roots.omp.len() > MAX_PACKAGE_ROOTS {
        return Err(too_many_package_roots(user_plugins_dir));
    }

    Ok(roots)
}

/// The shared incomplete-inventory refusal for a package-root bound.
fn too_many_package_roots(path: &Path) -> AppError {
    AppError::MissingInput {
        path: path.to_path_buf(),
        reason: format!(
            "OMP plugin packages declare more than {MAX_PACKAGE_ROOTS} Skill roots, so this \
             release cannot prove a complete conflict inventory"
        ),
    }
}

/// Adds one `omp-plugins` root, keeping OMP's first-seen-wins and marketplace-exclusion rules.
fn push_omp_root(
    roots: &mut Vec<PluginSkillRoot>,
    seen: &mut BTreeSet<PathBuf>,
    excluded: &BTreeSet<PathBuf>,
    candidate: (PathBuf, bool),
) -> Result<(), AppError> {
    let (root, project_level) = candidate;
    if !seen.insert(root.clone()) {
        return Ok(());
    }
    // OMP drops any declared root that is not a directory: a file entry point has no package
    // sub-tree to scan and never falls back to its parent.
    if !matches!(
        classify(&root)?.kind,
        PathKind::Directory | PathKind::DirectoryLink
    ) {
        return Ok(());
    }
    // A marketplace-backed package is already reached through the `claude-plugins` provider, so
    // OMP deliberately excludes it here to avoid listing one Skill twice.
    if excluded.contains(&canonical_identity(&root)) {
        return Ok(());
    }
    roots.push(PluginSkillRoot {
        skills_dir: root.join("skills"),
        project_level,
        includes_self: false,
    });
    Ok(())
}

/// Returns the nearest project extension-package root, using OMP's own anchor walk.
fn project_plugins_dir(launch_cwd: &Path, user_home: &Path) -> Result<Option<PathBuf>, AppError> {
    let mut with_git = None;
    for ancestor in launch_cwd.ancestors() {
        if ancestor == user_home {
            break;
        }
        if matches!(
            classify(&ancestor.join(OMP_CONFIG_DIR_NAME))?.kind,
            PathKind::Directory | PathKind::DirectoryLink
        ) {
            return Ok(Some(ancestor.join(OMP_CONFIG_DIR_NAME).join("plugins")));
        }
        if with_git.is_none() && classify(&ancestor.join(".git"))?.kind != PathKind::Missing {
            with_git = Some(ancestor.to_path_buf());
        }
    }
    Ok(with_git.map(|anchor| anchor.join(OMP_CONFIG_DIR_NAME).join("plugins")))
}

/// Reads the `extensions:` arrays OMP accepts from its own settings files.
fn declared_extension_roots(
    user_agent_dir: &Path,
    launch_cwd: &Path,
    inputs: &mut Vec<PathBuf>,
) -> Result<Vec<(PathBuf, bool)>, AppError> {
    let mut declared = Vec::new();
    for (settings_path, base, project_level) in [
        (
            launch_cwd.join(OMP_CONFIG_DIR_NAME).join("settings.json"),
            launch_cwd,
            true,
        ),
        (user_agent_dir.join("settings.json"), launch_cwd, false),
    ] {
        let Some(text) = read_regular(&settings_path)? else {
            continue;
        };
        inputs.push(settings_path);
        // OMP only warns about a malformed provider settings file, so an unreadable declaration
        // contributes nothing rather than failing the session.
        let Ok(value) = serde_json::from_str::<serde_json::Value>(&text) else {
            continue;
        };
        let Some(entries) = value
            .get("extensions")
            .and_then(serde_json::Value::as_array)
        else {
            continue;
        };
        for entry in entries {
            let Some(entry) = entry.as_str() else {
                continue;
            };
            declared.push((absolute_under(base, entry), project_level));
        }
    }
    Ok(declared)
}

/// Enumerates every enabled installed package, project shadowing user by package name.
fn installed_package_roots(
    user_plugins_dir: &Path,
    project_plugins_dir: Option<&Path>,
    launch_cwd: &Path,
    inputs: &mut Vec<PathBuf>,
) -> Result<Vec<(PathBuf, bool)>, AppError> {
    let overrides = project_overrides(launch_cwd, inputs)?;
    let mut merged: BTreeMap<String, (PathBuf, bool)> = BTreeMap::new();

    for (root, project_level) in [(Some(user_plugins_dir), false)]
        .into_iter()
        .chain(std::iter::once((project_plugins_dir, true)))
    {
        let Some(root) = root else {
            continue;
        };
        for (name, path) in packages_at(root, &overrides, inputs)? {
            merged.insert(name, (path, project_level));
        }
    }
    Ok(merged.into_values().collect())
}

/// Applies OMP's complete enablement predicate at one extension-package root.
fn packages_at(
    root: &Path,
    disabled: &BTreeSet<String>,
    inputs: &mut Vec<PathBuf>,
) -> Result<Vec<(String, PathBuf)>, AppError> {
    let node_modules = root.join("node_modules");
    if !matches!(
        classify(&node_modules)?.kind,
        PathKind::Directory | PathKind::DirectoryLink
    ) {
        return Ok(Vec::new());
    }

    let mut names = BTreeSet::new();
    let package_json = root.join("package.json");
    if let Some(text) = read_regular(&package_json)? {
        inputs.push(package_json);
        if let Ok(value) = serde_json::from_str::<serde_json::Value>(&text) {
            if let Some(dependencies) = value
                .get("dependencies")
                .and_then(|value| value.as_object())
            {
                names.extend(dependencies.keys().cloned());
            }
        }
    }

    let lock_path = root.join("omp-plugins.lock.json");
    let mut lock_states: BTreeMap<String, bool> = BTreeMap::new();
    if let Some(text) = read_regular(&lock_path)? {
        inputs.push(lock_path.clone());
        // OMP throws on a malformed lockfile and then loses every installed root, so SkillMount
        // cannot model the namespace from it. Failing closed is the only sound answer.
        let value = serde_json::from_str::<serde_json::Value>(&text).map_err(|error| {
            AppError::Internal(format!(
                "OMP extension-package lockfile {} cannot be interpreted ({error}), so this \
                 release cannot prove which packages OMP will enable; repair or remove it and \
                 retry",
                lock_path.display()
            ))
        })?;
        if let Some(plugins) = value.get("plugins").and_then(|value| value.as_object()) {
            for (name, state) in plugins {
                names.insert(name.clone());
                let enabled = state
                    .get("enabled")
                    .and_then(serde_json::Value::as_bool)
                    .unwrap_or(true);
                lock_states.insert(name.clone(), enabled);
            }
        }
    }

    let mut packages = Vec::new();
    for name in names {
        // A missing lock entry means enabled.
        if !lock_states.get(&name).copied().unwrap_or(true) || disabled.contains(&name) {
            continue;
        }
        let package_root = node_modules.join(&name);
        let manifest_path = package_root.join("package.json");
        let Some(text) = read_regular(&manifest_path)? else {
            continue;
        };
        inputs.push(manifest_path.clone());
        let value = serde_json::from_str::<serde_json::Value>(&text).map_err(|error| {
            AppError::Internal(format!(
                "OMP extension package manifest {} cannot be interpreted ({error}), so this \
                 release cannot prove whether it contributes Skills; repair or remove it and retry",
                manifest_path.display()
            ))
        })?;
        // A package without an `omp` or legacy `pi` manifest object contributes nothing.
        if value.get("omp").is_none() && value.get("pi").is_none() {
            continue;
        }
        packages.push((name, package_root));
    }
    Ok(packages)
}

/// Reads the first project override file that parses, exactly as OMP resolves it.
fn project_overrides(
    launch_cwd: &Path,
    inputs: &mut Vec<PathBuf>,
) -> Result<BTreeSet<String>, AppError> {
    for scope in [OMP_CONFIG_DIR_NAME, ".claude", ".codex", ".gemini"] {
        let path = launch_cwd.join(scope).join("plugin-overrides.json");
        let Some(text) = read_regular(&path)? else {
            continue;
        };
        inputs.push(path);
        // A parse failure falls through to the next candidate; the first file that parses wins
        // entirely, with no merge.
        let Ok(value) = serde_json::from_str::<serde_json::Value>(&text) else {
            continue;
        };
        let disabled = value
            .get("disabled")
            .and_then(serde_json::Value::as_array)
            .map(|entries| {
                entries
                    .iter()
                    .filter_map(|entry| entry.as_str().map(str::to_owned))
                    .collect()
            })
            .unwrap_or_default();
        return Ok(disabled);
    }
    Ok(BTreeSet::new())
}

/// Collects enabled marketplace plugin roots from the three registries OMP reads.
fn marketplace_roots(
    user_home: &Path,
    user_plugins_dir: &Path,
    project_plugins_dir: Option<&Path>,
    inputs: &mut Vec<PathBuf>,
) -> Result<Vec<MarketplaceRoot>, AppError> {
    let mut user: Vec<MarketplaceRoot> = Vec::new();
    let claude_registry = user_home.join(".claude/plugins/installed_plugins.json");
    for root in registry_entries(&claude_registry, false, inputs)? {
        user.push(root);
    }

    let omp_registry = user_plugins_dir.join("installed_plugins.json");
    let omp_entries = registry_entries(&omp_registry, false, inputs)?;
    // The OMP registry is authoritative: its entries replace Claude's for the same plugin id.
    let authoritative: BTreeSet<String> = omp_entries.iter().map(|root| root.id.clone()).collect();
    user.retain(|root| !authoritative.contains(&root.id));
    user.extend(omp_entries);

    let mut project = Vec::new();
    if let Some(directory) = project_plugins_dir {
        project = registry_entries(&directory.join("installed_plugins.json"), true, inputs)?;
    }
    if !project.is_empty() {
        let shadowed: BTreeSet<String> = project.iter().map(|root| root.id.clone()).collect();
        user.retain(|root| !shadowed.contains(&root.id));
        project.extend(user);
        return Ok(project);
    }
    Ok(user)
}

/// Parses one `installed_plugins.json` registry.
fn registry_entries(
    path: &Path,
    project_level: bool,
    inputs: &mut Vec<PathBuf>,
) -> Result<Vec<MarketplaceRoot>, AppError> {
    let Some(text) = read_regular(path)? else {
        return Ok(Vec::new());
    };
    inputs.push(path.to_path_buf());
    // OMP only warns about an unparseable registry and contributes nothing from it.
    let Ok(value) = serde_json::from_str::<serde_json::Value>(&text) else {
        return Ok(Vec::new());
    };
    // `parseClaudePluginsRegistry` discards the whole document unless `version` is a JSON number
    // (`discovery/helpers.ts:798-804`). Accepting a registry OMP rejects is not a harmless
    // over-read: a non-empty project registry drops every user root sharing a plugin id, so the
    // user's real roots would vanish from the inventory while OMP still loads them.
    if !value
        .get("version")
        .is_some_and(serde_json::Value::is_number)
    {
        return Ok(Vec::new());
    }
    let Some(plugins) = value.get("plugins").and_then(|value| value.as_object()) else {
        return Ok(Vec::new());
    };

    let mut roots = Vec::new();
    let mut seen = BTreeSet::new();
    for (id, entries) in plugins {
        let Some(entries) = entries.as_array() else {
            continue;
        };
        // An id without a `@marketplace` suffix is rejected by OMP.
        let Some(separator) = id.rfind('@') else {
            continue;
        };
        let plugin = id[..separator].to_owned();
        for entry in entries {
            let Some(install_path) = entry.get("installPath").and_then(serde_json::Value::as_str)
            else {
                continue;
            };
            if entry.get("enabled").and_then(serde_json::Value::as_bool) == Some(false) {
                continue;
            }
            // A `local`-scope entry is admitted without reproducing Claude Code's project-path
            // encoding. That can only over-report a pre-existing Skill, which fails closed, never
            // under-report one, which would let a mount silently shadow it.
            let scoped_project = project_level
                || entry.get("scope").and_then(serde_json::Value::as_str) == Some("local");
            // OMP joins this value with `path.join`, which folds `.` and `..`, so the stored form
            // has to be normalized too. Leaving it raw would make the containment check below
            // compare a normalized child against an unnormalized root and silently drop every
            // manifest-declared directory.
            let path = crate::paths::lexical_normalize(Path::new(install_path));
            if !seen.insert((id.clone(), path.clone())) {
                continue;
            }
            roots.push(MarketplaceRoot {
                id: id.clone(),
                plugin: plugin.clone(),
                path,
                project_level: scoped_project,
            });
        }
    }
    if roots.len() > MAX_REGISTRY_ROOTS {
        return Err(AppError::MissingInput {
            path: path.to_path_buf(),
            reason: format!(
                "OMP plugin registry names more than {MAX_REGISTRY_ROOTS} install paths, so this \
                 release cannot prove a complete conflict inventory"
            ),
        });
    }
    Ok(roots)
}

/// Resolves the Skill directories one marketplace plugin root contributes.
fn claude_skills_dirs(
    root: &MarketplaceRoot,
    inputs: &mut Vec<PathBuf>,
) -> Result<Vec<PluginSkillRoot>, AppError> {
    let include_fallback = !manifest_replaces_fallback(root, inputs)?;
    let mut configured = Vec::new();
    let manifest_path = root.path.join(".claude-plugin/plugin.json");
    if let Some(text) = read_regular(&manifest_path)? {
        inputs.push(manifest_path);
        if let Ok(value) = serde_json::from_str::<serde_json::Value>(&text) {
            match value.get("skills") {
                Some(serde_json::Value::String(entry)) if !entry.trim().is_empty() => {
                    configured.push(entry.trim().to_owned());
                }
                Some(serde_json::Value::Array(entries)) => {
                    if entries.len() > MAX_MANIFEST_SKILL_DIRS {
                        return Err(AppError::MissingInput {
                            path: root.path.join(".claude-plugin/plugin.json"),
                            reason: format!(
                                "OMP plugin manifest declares more than \
                                 {MAX_MANIFEST_SKILL_DIRS} Skill directories, so this release \
                                 cannot prove a complete conflict inventory"
                            ),
                        });
                    }
                    for entry in entries {
                        if let Some(entry) = entry.as_str() {
                            if !entry.trim().is_empty() {
                                configured.push(entry.trim().to_owned());
                            }
                        }
                    }
                }
                _ => {}
            }
        }
    }

    let fallback = root.path.join("skills");
    if configured.is_empty() {
        return Ok(vec![PluginSkillRoot {
            skills_dir: fallback,
            project_level: root.project_level,
            includes_self: true,
        }]);
    }

    let mut dirs = Vec::new();
    let mut seen = BTreeSet::new();
    if include_fallback {
        seen.insert(fallback.clone());
        dirs.push(fallback);
    }
    for entry in configured {
        let resolved = absolute_under(&root.path, &entry);
        // A declared directory outside the plugin root is ignored by OMP.
        if !resolved.starts_with(&root.path) || !seen.insert(resolved.clone()) {
            continue;
        }
        dirs.push(resolved);
    }
    Ok(dirs
        .into_iter()
        .map(|skills_dir| PluginSkillRoot {
            skills_dir,
            project_level: root.project_level,
            includes_self: true,
        })
        .collect())
}

/// Returns whether a marketplace manifest replaces the default `skills/` directory.
fn manifest_replaces_fallback(
    root: &MarketplaceRoot,
    inputs: &mut Vec<PathBuf>,
) -> Result<bool, AppError> {
    let path = root.path.join("marketplace.json");
    let Some(text) = read_regular(&path)? else {
        return Ok(false);
    };
    inputs.push(path);
    let Ok(value) = serde_json::from_str::<serde_json::Value>(&text) else {
        return Ok(false);
    };
    let Some(plugins) = value.get("plugins").and_then(serde_json::Value::as_array) else {
        return Ok(false);
    };
    Ok(plugins.iter().any(|entry| {
        entry.get("name").and_then(serde_json::Value::as_str) == Some(root.plugin.as_str())
            && entry.get("source").and_then(serde_json::Value::as_str) == Some("./")
    }))
}

/// Resolves one declared path against a base, matching OMP's absolute-or-relative rule.
fn absolute_under(base: &Path, entry: &str) -> PathBuf {
    let candidate = Path::new(entry);
    if candidate.is_absolute() {
        crate::paths::lexical_normalize(candidate)
    } else {
        crate::paths::lexical_normalize(&base.join(candidate))
    }
}

/// Returns the identity used to compare two roots that may be reached through a link.
fn canonical_identity(path: &Path) -> PathBuf {
    crate::paths::canonical_anchor(path)
}

#[cfg(test)]
mod tests {
    use super::resolve;
    use crate::error::ExitCategory;
    use crate::test_support::TestDir;
    use std::fs;
    use std::path::Path;

    fn write(path: &Path, contents: &str) {
        fs::create_dir_all(path.parent().expect("parent")).expect("directory");
        fs::write(path, contents).expect("file");
    }

    struct Fixture {
        _dir: TestDir,
        home: std::path::PathBuf,
        agent_dir: std::path::PathBuf,
        plugins_dir: std::path::PathBuf,
        project: std::path::PathBuf,
    }

    impl Fixture {
        fn new(label: &str) -> Self {
            let dir = TestDir::new(label);
            let root = fs::canonicalize(dir.path()).expect("canonical fixture root");
            let home = root.join("home");
            let project = root.join("project");
            for path in [&home, &project] {
                fs::create_dir_all(path).expect("fixture directory");
            }
            Self {
                _dir: dir,
                agent_dir: home.join(".omp/agent"),
                plugins_dir: home.join(".omp/plugins"),
                home,
                project,
            }
        }

        /// Installs one package with an `omp` manifest under a plugins root.
        fn package(root: &Path, name: &str) {
            write(
                &root.join("node_modules").join(name).join("package.json"),
                &format!("{{\"name\":\"{name}\",\"version\":\"1.0.0\",\"omp\":{{}}}}"),
            );
            fs::create_dir_all(root.join("node_modules").join(name).join("skills"))
                .expect("package skills directory");
        }

        fn resolve(&self) -> Result<super::PluginRoots, crate::error::AppError> {
            resolve(
                &self.agent_dir,
                &self.plugins_dir,
                &self.home,
                &self.project,
            )
        }
    }

    #[test]
    fn a_package_without_a_lock_entry_is_enabled() {
        let fixture = Fixture::new("omp-plugins-default-enabled");
        Fixture::package(&fixture.plugins_dir, "demo");
        write(
            &fixture.plugins_dir.join("package.json"),
            "{\"dependencies\":{\"demo\":\"1.0.0\"}}",
        );

        let roots = fixture.resolve().expect("roots resolve");
        assert_eq!(roots.omp.len(), 1);
        assert!(roots.omp[0].skills_dir.ends_with("demo/skills"));
        assert!(!roots.omp[0].includes_self);
    }

    #[test]
    fn a_lock_entry_and_a_project_override_each_disable_a_package() {
        let fixture = Fixture::new("omp-plugins-disabled");
        Fixture::package(&fixture.plugins_dir, "demo");
        write(
            &fixture.plugins_dir.join("package.json"),
            "{\"dependencies\":{\"demo\":\"1.0.0\"}}",
        );
        write(
            &fixture.plugins_dir.join("omp-plugins.lock.json"),
            "{\"plugins\":{\"demo\":{\"version\":\"1.0.0\",\"enabledFeatures\":null,\"enabled\":false}}}",
        );
        assert!(fixture.resolve().expect("roots resolve").omp.is_empty());

        write(
            &fixture.plugins_dir.join("omp-plugins.lock.json"),
            "{\"plugins\":{\"demo\":{\"version\":\"1.0.0\",\"enabledFeatures\":null,\"enabled\":true}}}",
        );
        write(
            &fixture.project.join(".omp/plugin-overrides.json"),
            "{\"disabled\":[\"demo\"]}",
        );
        assert!(fixture.resolve().expect("roots resolve").omp.is_empty());
    }

    #[test]
    fn a_package_without_an_omp_manifest_contributes_nothing() {
        let fixture = Fixture::new("omp-plugins-no-manifest");
        write(
            &fixture.plugins_dir.join("node_modules/plain/package.json"),
            "{\"name\":\"plain\",\"version\":\"1.0.0\"}",
        );
        write(
            &fixture.plugins_dir.join("package.json"),
            "{\"dependencies\":{\"plain\":\"1.0.0\"}}",
        );

        assert!(fixture.resolve().expect("roots resolve").omp.is_empty());
    }

    #[test]
    fn a_malformed_lockfile_fails_closed_as_an_unsupported_environment() {
        let fixture = Fixture::new("omp-plugins-bad-lock");
        Fixture::package(&fixture.plugins_dir, "demo");
        write(&fixture.plugins_dir.join("omp-plugins.lock.json"), "{ nope");

        let error = fixture
            .resolve()
            .expect_err("an unreadable lockfile must fail closed");
        assert_eq!(error.category(), ExitCategory::Internal);
        assert!(error.to_string().contains("omp-plugins.lock.json"));
    }

    #[test]
    fn a_project_package_shadows_a_user_package_of_the_same_name() {
        let fixture = Fixture::new("omp-plugins-shadow");
        let project_plugins = fixture.project.join(".omp/plugins");
        Fixture::package(&fixture.plugins_dir, "demo");
        Fixture::package(&project_plugins, "demo");
        for root in [&fixture.plugins_dir, &project_plugins] {
            write(
                &root.join("package.json"),
                "{\"dependencies\":{\"demo\":\"1.0.0\"}}",
            );
        }

        let roots = fixture.resolve().expect("roots resolve");
        assert_eq!(roots.omp.len(), 1);
        assert!(roots.omp[0].project_level);
        assert!(roots.omp[0].skills_dir.starts_with(&fixture.project));
    }

    #[test]
    fn a_declared_extension_directory_contributes_and_a_file_entry_point_does_not() {
        let fixture = Fixture::new("omp-plugins-declared");
        fs::create_dir_all(fixture.project.join("pack/skills")).expect("declared package");
        write(fixture.project.join("entry.ts").as_path(), "export {};\n");
        write(
            &fixture.project.join(".omp/settings.json"),
            "{\"extensions\":[\"pack\",\"entry.ts\"]}",
        );

        let roots = fixture.resolve().expect("roots resolve");
        assert_eq!(roots.omp.len(), 1, "{:?}", roots.omp);
        assert!(roots.omp[0].skills_dir.ends_with("pack/skills"));
        assert!(roots.omp[0].project_level);
    }

    #[test]
    fn a_marketplace_root_reaches_claude_plugins_and_is_excluded_from_omp_plugins() {
        let fixture = Fixture::new("omp-plugins-marketplace");
        let cache = fixture
            .plugins_dir
            .join("cache/plugins/shop___demo___1.0.0");
        fs::create_dir_all(cache.join("skills")).expect("cached plugin");
        write(
            &fixture.plugins_dir.join("installed_plugins.json"),
            &format!(
                "{{\"version\":1,\"plugins\":{{\"demo@shop\":[{{\"installPath\":{:?},\"enabled\":true}}]}}}}",
                cache.to_string_lossy()
            ),
        );
        // The same package is also linked into `node_modules`, which OMP must not list twice.
        write(
            &fixture.plugins_dir.join("node_modules/demo/package.json"),
            "{\"name\":\"demo\",\"version\":\"1.0.0\",\"omp\":{}}",
        );
        write(
            &fixture.plugins_dir.join("package.json"),
            "{\"dependencies\":{\"demo\":\"1.0.0\"}}",
        );

        let roots = fixture.resolve().expect("roots resolve");
        assert_eq!(roots.claude.len(), 1);
        assert!(roots.claude[0].includes_self);
        assert!(roots.claude[0].skills_dir.starts_with(&cache));
    }

    #[test]
    fn a_disabled_registry_entry_contributes_nothing() {
        let fixture = Fixture::new("omp-plugins-registry-disabled");
        let cache = fixture
            .plugins_dir
            .join("cache/plugins/shop___demo___1.0.0");
        fs::create_dir_all(cache.join("skills")).expect("cached plugin");
        write(
            &fixture.plugins_dir.join("installed_plugins.json"),
            &format!(
                "{{\"version\":1,\"plugins\":{{\"demo@shop\":[{{\"installPath\":{:?},\"enabled\":false}}]}}}}",
                cache.to_string_lossy()
            ),
        );

        assert!(fixture.resolve().expect("roots resolve").claude.is_empty());
    }

    #[test]
    fn a_registry_without_a_numeric_version_contributes_nothing() {
        // `parseClaudePluginsRegistry` discards the whole document unless `version` is a JSON
        // number (`discovery/helpers.ts:798-804`). Reading one OMP rejects would let an in-repo
        // registry delete the operator's real user-scope roots from the inventory, because a
        // non-empty project registry shadows every user root sharing a plugin id.
        let fixture = Fixture::new("omp-plugins-registry-version");
        let cache = fixture
            .plugins_dir
            .join("cache/plugins/shop___demo___1.0.0");
        fs::create_dir_all(cache.join("skills")).expect("cached plugin");
        for registry in [
            "{{\"plugins\":{{\"demo@shop\":[{{\"installPath\":{:?},\"enabled\":true}}]}}}}",
            "{{\"version\":\"1\",\"plugins\":{{\"demo@shop\":[{{\"installPath\":{:?},\"enabled\":true}}]}}}}",
        ] {
            write(
                &fixture.plugins_dir.join("installed_plugins.json"),
                &registry.replace("{:?}", &format!("{:?}", cache.to_string_lossy())),
            );
            assert!(
                fixture.resolve().expect("roots resolve").claude.is_empty(),
                "a registry OMP discards must contribute nothing: {registry}"
            );
        }

        write(
            &fixture.plugins_dir.join("installed_plugins.json"),
            &format!(
                "{{\"version\":1,\"plugins\":{{\"demo@shop\":[{{\"installPath\":{:?},\"enabled\":true}}]}}}}",
                cache.to_string_lossy()
            ),
        );
        assert_eq!(fixture.resolve().expect("roots resolve").claude.len(), 1);
    }

    #[test]
    fn an_oversized_manifest_skills_array_fails_closed() {
        // The manifest is attacker-authored and the registry admits many ids naming one install
        // path, so an unbounded array is multiplied by that fan-out. Bounding it only after every
        // root exists would allocate first, which is what the total provider-root bound does.
        let fixture = Fixture::new("omp-plugins-manifest-bound");
        let cache = fixture
            .plugins_dir
            .join("cache/plugins/shop___demo___1.0.0");
        fs::create_dir_all(cache.join("skills")).expect("cached plugin");
        let entries = (0..=super::MAX_MANIFEST_SKILL_DIRS)
            .map(|index| format!("\"./d{index}\""))
            .collect::<Vec<_>>()
            .join(",");
        write(
            &cache.join(".claude-plugin/plugin.json"),
            &format!("{{\"name\":\"demo\",\"skills\":[{entries}]}}"),
        );
        write(
            &fixture.plugins_dir.join("installed_plugins.json"),
            &format!(
                "{{\"version\":1,\"plugins\":{{\"demo@shop\":[{{\"installPath\":{:?},\"enabled\":true}}]}}}}",
                cache.to_string_lossy()
            ),
        );

        let error = fixture
            .resolve()
            .expect_err("an unbounded manifest must fail closed");
        assert_eq!(error.category(), ExitCategory::MissingInput);
        assert!(
            format!("{error}").contains("complete conflict inventory"),
            "{error}"
        );
    }

    #[test]
    fn a_manifest_skills_override_replaces_or_extends_the_default_directory() {
        let fixture = Fixture::new("omp-plugins-manifest-skills");
        let cache = fixture
            .plugins_dir
            .join("cache/plugins/shop___demo___1.0.0");
        fs::create_dir_all(cache.join("extra")).expect("declared skills directory");
        write(
            &cache.join(".claude-plugin/plugin.json"),
            "{\"name\":\"demo\",\"skills\":[\"./extra\",\"../escape\"]}",
        );
        write(
            &fixture.plugins_dir.join("installed_plugins.json"),
            &format!(
                "{{\"version\":1,\"plugins\":{{\"demo@shop\":[{{\"installPath\":{:?}}}]}}}}",
                cache.to_string_lossy()
            ),
        );

        let roots = fixture.resolve().expect("roots resolve");
        let dirs: Vec<_> = roots
            .claude
            .iter()
            .map(|root| root.skills_dir.clone())
            .collect();
        assert_eq!(dirs, [cache.join("skills"), cache.join("extra")]);

        // A `marketplace.json` self-source entry replaces the default directory instead.
        write(
            &cache.join("marketplace.json"),
            "{\"plugins\":[{\"name\":\"demo\",\"source\":\"./\"}]}",
        );
        let roots = fixture.resolve().expect("roots resolve");
        let dirs: Vec<_> = roots
            .claude
            .iter()
            .map(|root| root.skills_dir.clone())
            .collect();
        assert_eq!(dirs, [cache.join("extra")]);
    }
}
