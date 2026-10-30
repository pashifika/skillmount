//! OMP-native directory discovery through a session-owned settings overlay.
//!
//! OMP owns Skill parsing, filtering, duplicates and provider precedence. `SkillMount` preserves the
//! initial configured custom-directory list, appends CLI roots in order, and supervises the child
//! while its private overlay remains available. It never edits project configuration or links Skills.

mod args;
mod settings;

use std::collections::BTreeMap;
use std::ffi::OsString;
use std::fs;
use std::path::{Path, PathBuf};

use crate::agent::version::VersionSpec;
use crate::agent::{AgentAdapter, DiscoverySnapshot};
use crate::diagnostic::Diagnostic;
use crate::domain::{
    AgentId, CatalogPolicy, ConflictPolicy, LinkMode, RunContext, SkillCatalog, ValidationLevel,
};
use crate::error::AppError;
use crate::lock::{LockAccess, LockResource, LockResourceKind};
use crate::mount::resolve::classify;
use crate::mount::{
    ActionSequence, DiscoveryPlan, LaunchPlan, MountAction, MountPlan, PathPrecondition,
};
use crate::state::{PENDING_SESSION, session_root_base};

/// Read-only OMP-native adapter.
#[derive(Debug, Clone, Copy, Default)]
pub struct OmpAdapter;

const LAST_TESTED_OMP_BANNER: &str = "omp/18.14.0";

fn layout(context: &RunContext) -> Result<(PathBuf, PathBuf), AppError> {
    let id = context
        .session_id
        .as_ref()
        .map_or_else(|| PENDING_SESSION.to_owned(), ToString::to_string);
    let directory = session_root_base()?.join(id);
    Ok((directory.clone(), directory.join("omp-config.yml")))
}

fn verify_environment() -> Result<(), AppError> {
    if std::env::var_os("PI_CONFIG_DIR").is_some_and(|value| {
        let path = Path::new(&value);
        path.has_root()
            || matches!(
                path.components().next(),
                Some(std::path::Component::Prefix(_))
            )
    }) {
        return Err(AppError::Usage("absolute PI_CONFIG_DIR or a drive prefix has incompatible native path-join semantics; use a relative config directory or run OMP directly".to_owned()));
    }

    for name in ["OMP_PROFILE", "PI_PROFILE", "PI_CONFIG_FILES"] {
        if std::env::var_os(name).is_some_and(|value| !value.is_empty()) {
            return Err(AppError::Usage(format!(
                "{name} relocates or overlays OMP configuration; unset it or run the agent directly"
            )));
        }
    }
    Ok(())
}

fn verify_options(context: &RunContext) -> Result<(), AppError> {
    if context.options.link_mode != LinkMode::Auto
        || context.options.keep_mounts
        || context.options.conflict != ConflictPolicy::Error
        || context.options.validation != ValidationLevel::Basic
    {
        return Err(AppError::Usage("OMP uses native directory discovery, not Skill links: --link-mode, --keep-mounts, --conflict=skip and non-default --validation policies are not supported; configure Skill policy in OMP".to_owned()));
    }
    Ok(())
}

fn names_home(cwd: &Path, home: &Path) -> bool {
    let comparable = crate::link::resolve::ComparablePath::new(cwd);
    comparable.names_same_path(&crate::link::resolve::ComparablePath::new(home))
        || fs::canonicalize(home).is_ok_and(|path| {
            comparable.names_same_path(&crate::link::resolve::ComparablePath::new(&path))
        })
}

/// Only directory accessibility is our contract. OMP selects and validates their entries itself.
fn source_directories(context: &RunContext) -> Result<Vec<String>, AppError> {
    context.skill_sources.iter().map(|source| {
        let path = &source.resolved_path;
        fs::read_dir(path).map_err(|error| AppError::MissingInput { path: path.clone(), reason: error.to_string() })?;
        if path.join("SKILL.md").is_file() {
            return Err(AppError::Usage(format!("OMP --skills-dir {} must name a collection containing Skill subdirectories, not an individual Skill directory; OMP scans <root>/<entry>/SKILL.md", path.display())));
        }
        let canonical = fs::canonicalize(path).map_err(|error| AppError::MissingInput { path: path.clone(), reason: error.to_string() })?;
        canonical.to_str().map(str::to_owned).ok_or_else(|| AppError::Usage(format!("OMP customDirectories requires a Unicode path: {}", path.display())))
    }).collect()
}

fn configuration(context: &RunContext) -> Result<settings::Configuration, AppError> {
    let omp = context.agent.omp()?;
    settings::load(&omp.agent_dir, &context.launch_cwd, &omp.user_home)
}

fn overlay(context: &RunContext) -> Result<String, AppError> {
    let mut directories = configuration(context)?.directories;
    directories.extend(source_directories(context)?);
    if directories.len() > 1_024 {
        return Err(AppError::Usage(
            "OMP customDirectories plus CLI sources exceeds 1024 directories".to_owned(),
        ));
    }
    // JSON is a YAML subset. Serialization, rather than interpolation, preserves quotes,
    // backslashes, newlines and platform paths without allowing another settings key injection.
    serde_json::to_string(&serde_json::json!({"skills": {"customDirectories": directories}}))
        .map_err(|error| {
            AppError::Internal(format!("cannot serialize OMP settings overlay: {error}"))
        })
}

impl AgentAdapter for OmpAdapter {
    fn version_spec(&self) -> VersionSpec {
        VersionSpec::new(LAST_TESTED_OMP_BANNER, "SKILLMOUNT_TEST_OMP_VERSION")
    }

    fn catalog_policy(&self) -> CatalogPolicy {
        CatalogPolicy {
            requires_exact_skill_md_entry: false,
            always_parses_metadata: false,
            requires_name: false,
            requires_description: false,
            requires_matching_name: false,
        }
    }

    fn resolve_catalog(&self, context: &RunContext) -> Result<SkillCatalog, AppError> {
        source_directories(context)?;
        Ok(SkillCatalog {
            resolutions: Vec::new(),
            warnings: Vec::new(),
        })
    }

    fn destination_stores(&self, _context: &RunContext) -> Vec<PathBuf> {
        Vec::new()
    }

    fn validate_passthrough_args(&self, args: &[OsString]) -> Result<Vec<Diagnostic>, AppError> {
        args::validate(args)?;
        Ok(Vec::new())
    }

    fn validate_launch_invariants(&self, context: &RunContext) -> Result<(), AppError> {
        verify_environment()?;
        verify_options(context)?;
        let parsed = args::validate(&context.passthrough_args)?;
        if !parsed.allows_home && names_home(&context.launch_cwd, &context.agent.omp()?.user_home) {
            return Err(AppError::Usage("OMP changes its launch CWD at the user home without --allow-home; pass OMP's --allow-home explicitly or launch in a project".to_owned()));
        }
        configuration(context)?;
        source_directories(context)?;
        Ok(())
    }

    fn inspect_discovery(&self, context: &RunContext) -> Result<DiscoverySnapshot, AppError> {
        verify_environment()?;
        let config = configuration(context)?;
        let (directory, _) = layout(context)?;
        let state = classify(&directory)?;
        let mut resources = vec![
            LockResource::describe_shared(
                LockResourceKind::DiscoveryEntry,
                LockAccess::Mutate,
                &directory,
            )?,
            LockResource::describe_shared(
                LockResourceKind::BackingStore,
                LockAccess::Mutate,
                &directory,
            )?,
        ];
        for path in config.inputs.iter().chain(
            context
                .skill_sources
                .iter()
                .map(|source| &source.resolved_path),
        ) {
            resources.push(LockResource::describe_shared(
                LockResourceKind::DiscoveryEntry,
                LockAccess::Observe,
                path,
            )?);
        }
        Ok(DiscoverySnapshot {
            agent: AgentId::Omp, scopes: Vec::new(), visible_skills: BTreeMap::new(), mount_entries: BTreeMap::new(),
            discovery_entry: directory.clone(), backing_store: directory, backing_store_state: state.kind,
            backing_store_canonical: None, lock_resources: resources,
            warnings: vec![Diagnostic::warning("OMP owns Skill discovery, filtering and duplicates. Existing customDirectories precede CLI roots; first custom occurrence keeps the bare name and differing duplicates may remain namespaced. No Skill links or project settings are written.".to_owned(), context.launch_cwd.clone())],
        })
    }

    fn build_mount_plan(
        &self,
        context: &RunContext,
        _catalog: &SkillCatalog,
        discovery: &DiscoverySnapshot,
    ) -> Result<MountPlan, AppError> {
        let (directory, config_path) = layout(context)?;
        let mut actions = ActionSequence::default();
        actions.push(
            MountAction::CreateDirectory { path: directory },
            PathPrecondition::Missing,
        );
        actions.push(
            MountAction::CreateConfigFile {
                path: config_path.clone(),
                contents: overlay(context)?,
            },
            PathPrecondition::Missing,
        );
        Ok(MountPlan {
            agent: AgentId::Omp,
            discovery: DiscoveryPlan {
                entry: discovery.discovery_entry.clone(),
                backing_store: discovery.backing_store.clone(),
            },
            actions: actions.into_actions(),
            preserved: Vec::new(),
            launch: LaunchPlan {
                executable: context.executable().to_path_buf(),
                cwd: context.launch_cwd.clone(),
                injected_args: vec![OsString::from("--config"), config_path.into_os_string()],
                passthrough_args: context.passthrough_args.clone(),
                environment_overrides: Vec::new(),
            },
        })
    }

    fn validate_spawn_boundary(
        &self,
        context: &RunContext,
        _catalog: &SkillCatalog,
        _discovery: &DiscoverySnapshot,
        plan: &MountPlan,
    ) -> Result<(), AppError> {
        self.validate_launch_invariants(context)?;
        let current = overlay(context)?;
        let unchanged = plan.actions.iter().any(|action| {
            matches!(&action.operation,
            MountAction::CreateConfigFile { contents, .. } if contents == &current)
        });
        let (_, config_path) = layout(context)?;
        let actual = settings::read_regular(&config_path)?;
        if !unchanged || actual.as_deref() != Some(current.as_str()) {
            return Err(AppError::Temporary(
                "OMP customDirectories or source paths changed before launch; nothing was launched"
                    .to_owned(),
            ));
        }
        Ok(())
    }
}
