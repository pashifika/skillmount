//! Invocation-relative path resolution and project-root discovery.

use std::ffi::{OsStr, OsString};
use std::fs;
use std::path::{Component, Path, PathBuf};

use crate::cli::SessionInput;
use crate::domain::{AgentId, RunContext, SourceOccurrence};
use crate::error::AppError;

#[cfg(windows)]
mod windows_ffi;

pub(crate) fn resolve_session(
    input: SessionInput,
    invocation_cwd: &Path,
) -> Result<RunContext, AppError> {
    let invocation_cwd = canonical_directory(invocation_cwd)?;
    let launch_cwd = match input.cwd.as_deref() {
        Some(path) => canonical_directory(&absolute_from(&invocation_cwd, path)?)?,
        None => invocation_cwd.clone(),
    };

    let inferred_project_root =
        nearest_git_root(&launch_cwd)?.unwrap_or_else(|| launch_cwd.clone());
    let project_root = match input.project_root.as_deref() {
        Some(path) => canonical_directory(&absolute_from(&invocation_cwd, path)?)?,
        None => inferred_project_root.clone(),
    };

    if !launch_cwd.starts_with(&project_root) {
        return Err(AppError::Usage(format!(
            "{} project root {} does not contain launch CWD {}",
            agent_label(input.agent),
            project_root.display(),
            launch_cwd.display()
        )));
    }
    if project_root != inferred_project_root {
        return Err(AppError::Usage(format!(
            "{} project root {} does not match the default root {} inferred from launch CWD {}; --project-root cannot change the root used by the child",
            agent_label(input.agent),
            project_root.display(),
            inferred_project_root.display(),
            launch_cwd.display()
        )));
    }

    let user_home = agent_user_home(input.agent)?;
    let (codex_home, codex_home_override) = if input.agent == AgentId::Codex {
        codex_home(&user_home, &invocation_cwd)?
    } else {
        (user_home.join(".codex"), None)
    };
    let claude_config_dir = claude_config_dir(&user_home, &launch_cwd)?;

    let skill_sources = resolve_source_occurrences(&input.skills_dirs, &invocation_cwd)?;
    let resolve_agent_executable = !input.options.dry_run;
    let agent_bin = match input.agent_bin {
        Some(path) => {
            let resolved = absolute_from(&invocation_cwd, &path)?;
            if resolve_agent_executable {
                validate_explicit_executable(&resolved)?
            } else {
                resolved
            }
        }
        None if resolve_agent_executable => {
            resolve_path_executable(input.agent.executable_name(), &invocation_cwd)?
        }
        None => PathBuf::from(input.agent.executable_name()),
    };

    Ok(RunContext {
        agent: input.agent,
        invocation_cwd,
        launch_cwd,
        project_root,
        user_home,
        codex_home,
        codex_home_override,
        codex_admin_skills: codex_admin_skills(),
        claude_config_dir,
        claude_managed_skills: claude_managed_skills(),
        skill_sources,
        session_id: None,
        agent_bin,
        passthrough_args: input.passthrough_args,
        options: input.options,
    })
}

const fn agent_label(agent: AgentId) -> &'static str {
    match agent {
        AgentId::Codex => "Codex",
        AgentId::Claude => "Claude",
    }
}

fn validate_explicit_executable(path: &Path) -> Result<PathBuf, AppError> {
    let canonical = fs::canonicalize(path).map_err(|error| AppError::MissingInput {
        path: path.to_path_buf(),
        reason: format!("cannot resolve agent executable: {error}"),
    })?;
    reject_implicit_shell(&canonical)?;
    validate_runnable(&canonical)?;
    Ok(canonical)
}

fn resolve_path_executable(name: &OsStr, invocation_cwd: &Path) -> Result<PathBuf, AppError> {
    let search_path = std::env::var_os("PATH").ok_or_else(|| AppError::MissingInput {
        path: PathBuf::from(name),
        reason: "PATH is not set, so the agent executable cannot be resolved".to_owned(),
    })?;

    for directory in std::env::split_paths(&search_path) {
        let directory = if directory.as_os_str().is_empty() {
            invocation_cwd.to_path_buf()
        } else if directory.is_absolute() {
            directory
        } else {
            invocation_cwd.join(directory)
        };
        for candidate_name in executable_names(name) {
            let candidate = directory.join(candidate_name);
            let Ok(canonical) = fs::canonicalize(&candidate) else {
                continue;
            };
            if reject_implicit_shell(&canonical).is_ok() && validate_runnable(&canonical).is_ok() {
                return Ok(canonical);
            }
        }
    }

    Err(AppError::MissingInput {
        path: PathBuf::from(name),
        reason: "no runnable shell-free executable was found on PATH".to_owned(),
    })
}

#[cfg(unix)]
fn executable_names(name: &OsStr) -> Vec<OsString> {
    vec![name.to_os_string()]
}

#[cfg(windows)]
fn executable_names(name: &OsStr) -> Vec<OsString> {
    if Path::new(name).extension().is_some() {
        return vec![name.to_os_string()];
    }
    let mut exe = name.to_os_string();
    exe.push(".exe");
    vec![exe]
}

#[cfg(unix)]
fn validate_runnable(path: &Path) -> Result<(), AppError> {
    use nix::fcntl::{AT_FDCWD, AtFlags};
    use nix::unistd::{AccessFlags, faccessat};

    let metadata = fs::metadata(path).map_err(|error| AppError::MissingInput {
        path: path.to_path_buf(),
        reason: format!("agent executable is unavailable: {error}"),
    })?;
    if !metadata.is_file() {
        return Err(AppError::MissingInput {
            path: path.to_path_buf(),
            reason: "agent executable is not a regular file".to_owned(),
        });
    }
    if faccessat(AT_FDCWD, path, AccessFlags::X_OK, AtFlags::AT_EACCESS).is_err() {
        return Err(AppError::MissingInput {
            path: path.to_path_buf(),
            reason: "agent executable is not executable by the current effective user".to_owned(),
        });
    }
    Ok(())
}

#[cfg(windows)]
fn validate_runnable(path: &Path) -> Result<(), AppError> {
    let metadata = fs::metadata(path).map_err(|error| AppError::MissingInput {
        path: path.to_path_buf(),
        reason: format!("agent executable is unavailable: {error}"),
    })?;
    if metadata.is_file() {
        Ok(())
    } else {
        Err(AppError::MissingInput {
            path: path.to_path_buf(),
            reason: "agent executable is not a regular file".to_owned(),
        })
    }
}

fn reject_implicit_shell(path: &Path) -> Result<(), AppError> {
    if requires_implicit_shell(path) {
        Err(AppError::Usage(format!(
            "agent executable {} is a batch file and would require implicit cmd.exe execution; use a native executable",
            path.display()
        )))
    } else {
        Ok(())
    }
}

#[cfg(unix)]
fn requires_implicit_shell(_path: &Path) -> bool {
    false
}

#[cfg(windows)]
fn requires_implicit_shell(path: &Path) -> bool {
    path.extension().is_some_and(|extension| {
        extension.eq_ignore_ascii_case("cmd") || extension.eq_ignore_ascii_case("bat")
    })
}

/// Builds a run context for `inspect`, which has no session options to resolve.
///
/// Inspection still needs a launch CWD and a project root, because a discovery namespace is only
/// meaningful relative to them. Defaults match a session started in the invocation directory.
pub(crate) fn resolve_inspection(
    agent: AgentId,
    skills_dirs: &[PathBuf],
    validation: crate::domain::ValidationLevel,
    invocation_cwd: &Path,
) -> Result<RunContext, AppError> {
    let invocation_cwd = canonical_directory(invocation_cwd)?;
    let project_root = nearest_git_root(&invocation_cwd)?.unwrap_or_else(|| invocation_cwd.clone());
    let user_home = agent_user_home(agent)?;
    let (codex_home, codex_home_override) = if agent == AgentId::Codex {
        codex_home(&user_home, &invocation_cwd)?
    } else {
        (user_home.join(".codex"), None)
    };
    let claude_config_dir = claude_config_dir(&user_home, &invocation_cwd)?;
    Ok(RunContext {
        agent,
        launch_cwd: invocation_cwd.clone(),
        skill_sources: resolve_source_occurrences(skills_dirs, &invocation_cwd)?,
        invocation_cwd,
        project_root,
        user_home,
        codex_home,
        codex_home_override,
        codex_admin_skills: codex_admin_skills(),
        claude_config_dir,
        claude_managed_skills: claude_managed_skills(),
        session_id: None,
        agent_bin: PathBuf::from(agent.executable_name()),
        passthrough_args: Vec::new(),
        options: crate::domain::RunOptions {
            link_mode: crate::domain::LinkMode::Auto,
            mount_mode: match agent {
                AgentId::Codex => crate::domain::MountMode::Project,
                AgentId::Claude => crate::domain::MountMode::Staging,
            },
            conflict: crate::domain::ConflictPolicy::Error,
            validation,
            dry_run: true,
            keep_mounts: false,
            no_recover: false,
            verbosity: 0,
        },
    })
}

fn agent_user_home(agent: AgentId) -> Result<PathBuf, AppError> {
    match agent {
        AgentId::Codex => codex_user_home(),
        AgentId::Claude => crate::state::user_home(),
    }
}

/// Mirrors the home resolver used by Codex's user-wide `.agents/skills` root.
fn codex_user_home() -> Result<PathBuf, AppError> {
    // Native integration tests must not inspect the developer's actual user root on Windows,
    // where Codex uses FOLDERID_Profile instead of the overridden USERPROFILE value.
    // The test-only override is absent from release builds, like failure checkpoints.
    #[cfg(debug_assertions)]
    if let Some(path) =
        std::env::var_os("SKILLMOUNT_TEST_CODEX_USER_HOME").filter(|value| !value.is_empty())
    {
        return absolute_codex_user_home(Some(PathBuf::from(path)));
    }

    absolute_codex_user_home(platform_codex_user_home())
}

fn platform_codex_user_home() -> Option<PathBuf> {
    #[cfg(windows)]
    {
        windows_ffi::profile_directory()
    }
    #[cfg(unix)]
    {
        std::env::var_os("HOME")
            .filter(|value| !value.is_empty())
            .map(PathBuf::from)
            .or_else(|| {
                nix::unistd::User::from_uid(nix::unistd::Uid::current())
                    .ok()
                    .flatten()
                    .map(|user| user.dir)
            })
    }
    #[cfg(not(any(unix, windows)))]
    {
        None
    }
}

fn absolute_codex_user_home(home: Option<PathBuf>) -> Result<PathBuf, AppError> {
    let Some(home) = home.filter(|path| path.is_absolute()) else {
        return Err(AppError::MissingInput {
            path: PathBuf::from("<Codex user home>"),
            reason: "Codex could not resolve an absolute user home directory".to_owned(),
        });
    };
    Ok(home)
}

/// Mirrors Codex's configuration-home resolution and retains an explicit child override.
fn codex_home(
    user_home: &Path,
    invocation_cwd: &Path,
) -> Result<(PathBuf, Option<PathBuf>), AppError> {
    let value = unicode_codex_home(std::env::var("CODEX_HOME"));
    codex_home_from_value(user_home, invocation_cwd, value.as_deref())
}

fn codex_home_from_value(
    user_home: &Path,
    invocation_cwd: &Path,
    value: Option<&str>,
) -> Result<(PathBuf, Option<PathBuf>), AppError> {
    let Some(value) = value.filter(|value| !value.is_empty()) else {
        return Ok((user_home.join(".codex"), None));
    };
    let supplied = PathBuf::from(value);
    let resolved = if supplied.is_absolute() {
        supplied.clone()
    } else {
        invocation_cwd.join(&supplied)
    };
    let metadata = fs::metadata(&resolved).map_err(|error| AppError::MissingInput {
        path: supplied.clone(),
        reason: format!("CODEX_HOME does not name an existing directory: {error}"),
    })?;
    if !metadata.is_dir() {
        return Err(AppError::MissingInput {
            path: supplied,
            reason: "CODEX_HOME is not a directory".to_owned(),
        });
    }
    let canonical = fs::canonicalize(&resolved).map_err(|error| AppError::MissingInput {
        path: resolved,
        reason: format!("cannot canonicalize CODEX_HOME: {error}"),
    })?;
    if canonical.to_str().is_none() {
        return Err(AppError::MissingInput {
            path: canonical,
            reason: "canonical CODEX_HOME is not Unicode and cannot be propagated to Codex"
                .to_owned(),
        });
    }
    Ok((canonical.clone(), Some(canonical)))
}

fn unicode_codex_home(value: Result<String, std::env::VarError>) -> Option<String> {
    value.ok().filter(|value| !value.is_empty())
}

/// Mirrors Claude Code's relocation of every user `~/.claude` path.
fn claude_config_dir(user_home: &Path, launch_cwd: &Path) -> Result<PathBuf, AppError> {
    claude_config_dir_from_value(user_home, launch_cwd, std::env::var_os("CLAUDE_CONFIG_DIR"))
}

fn claude_config_dir_from_value(
    user_home: &Path,
    launch_cwd: &Path,
    value: Option<OsString>,
) -> Result<PathBuf, AppError> {
    let Some(configured) = value.filter(|value| !value.is_empty()) else {
        return Ok(user_home.join(".claude"));
    };
    absolute_from(launch_cwd, Path::new(&configured))
}

/// Returns the host-wide enterprise Claude Code Skill root supported by 2.1.220.
fn claude_managed_skills() -> PathBuf {
    #[cfg(debug_assertions)]
    if let Some(path) =
        std::env::var_os("SKILLMOUNT_CLAUDE_MANAGED_SKILLS_DIR").filter(|value| !value.is_empty())
    {
        return PathBuf::from(path);
    }

    #[cfg(windows)]
    {
        let program_files = windows_ffi::program_files_directory()
            .unwrap_or_else(|| PathBuf::from(r"C:\Program Files"));
        program_files.join("ClaudeCode/.claude/skills")
    }
    #[cfg(not(windows))]
    {
        PathBuf::from("/Library/Application Support/ClaudeCode/.claude/skills")
    }
}

/// Returns the host-wide Codex Skill root supported by the current loader.
#[allow(clippy::unnecessary_wraps)]
fn codex_admin_skills() -> Option<PathBuf> {
    // Integration tests need to isolate discovery from the developer host just as they isolate
    // HOME and CODEX_HOME. The override is absent from release builds, like failure checkpoints.
    #[cfg(debug_assertions)]
    if let Some(path) =
        std::env::var_os("SKILLMOUNT_CODEX_ADMIN_SKILLS_DIR").filter(|value| !value.is_empty())
    {
        return Some(PathBuf::from(path));
    }

    #[cfg(unix)]
    {
        Some(PathBuf::from("/etc/codex/skills"))
    }
    #[cfg(windows)]
    {
        let program_data = windows_ffi::program_data_directory()
            .unwrap_or_else(|| PathBuf::from(r"C:\ProgramData"));
        Some(program_data.join("OpenAI/Codex/skills"))
    }
}

pub(crate) fn resolve_source_occurrences(
    inputs: &[PathBuf],
    invocation_cwd: &Path,
) -> Result<Vec<SourceOccurrence>, AppError> {
    inputs
        .iter()
        .enumerate()
        .map(|(ordinal, input_path)| {
            Ok(SourceOccurrence {
                ordinal,
                input_path: input_path.clone(),
                resolved_path: absolute_from(invocation_cwd, input_path)?,
            })
        })
        .collect()
}

pub(crate) fn absolute_from(base: &Path, value: &Path) -> Result<PathBuf, AppError> {
    if !value.is_absolute() && matches!(value.components().next(), Some(Component::Prefix(_))) {
        return Err(AppError::Usage(format!(
            "drive-relative Windows path {} is ambiguous; use an absolute path or a path relative to the invocation directory",
            value.display()
        )));
    }

    let path = if value.is_absolute() {
        value.to_path_buf()
    } else {
        base.join(value)
    };
    Ok(lexical_normalize(&path))
}

pub(crate) fn lexical_normalize(path: &Path) -> PathBuf {
    let mut normalized = PathBuf::new();
    for component in path.components() {
        match component {
            Component::CurDir => {}
            Component::ParentDir => {
                if !matches!(
                    normalized.components().next_back(),
                    Some(Component::RootDir | Component::Prefix(_))
                ) {
                    normalized.pop();
                }
            }
            other => normalized.push(other.as_os_str()),
        }
    }
    normalized
}

/// Splits `path` into its deepest existing canonical ancestor and the components below it.
///
/// A resource that does not exist yet still needs a stable identity. Returning the two halves
/// separately lets a caller persist them, which is what lock-resource identity requires: recomputing
/// the split after intermediate directories appear would move the anchor deeper and change the key.
///
/// The anchor is empty only when no ancestor exists, which on both supported platforms means the
/// path had no root to begin with.
pub(crate) fn split_existing_anchor(path: &Path) -> (PathBuf, PathBuf) {
    let mut cursor = path;
    let mut tail = Vec::new();
    loop {
        if let Ok(canonical) = fs::canonicalize(cursor) {
            let mut suffix = PathBuf::new();
            for component in tail.iter().rev() {
                suffix.push(component);
            }
            return (canonical, suffix);
        }
        let Some(name) = cursor.file_name() else {
            return (PathBuf::new(), lexical_normalize(path));
        };
        tail.push(name.to_os_string());
        let Some(parent) = cursor.parent() else {
            return (PathBuf::new(), lexical_normalize(path));
        };
        cursor = parent;
    }
}

/// Canonicalizes the longest existing prefix of `path` and re-appends the remaining components.
pub(crate) fn canonical_anchor(path: &Path) -> PathBuf {
    let (anchor, suffix) = split_existing_anchor(path);
    lexical_normalize(&anchor.join(suffix))
}

fn canonical_directory(path: &Path) -> Result<PathBuf, AppError> {
    let metadata = fs::metadata(path).map_err(|error| AppError::MissingInput {
        path: path.to_path_buf(),
        reason: error.to_string(),
    })?;
    if !metadata.is_dir() {
        return Err(AppError::MissingInput {
            path: path.to_path_buf(),
            reason: "expected a directory".to_owned(),
        });
    }
    fs::canonicalize(path).map_err(|error| AppError::MissingInput {
        path: path.to_path_buf(),
        reason: error.to_string(),
    })
}

fn nearest_git_root(start: &Path) -> Result<Option<PathBuf>, AppError> {
    for ancestor in start.ancestors() {
        let marker = ancestor.join(OsStr::new(".git"));
        match fs::metadata(&marker) {
            Ok(_) => {
                return Ok(Some(ancestor.to_path_buf()));
            }
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
            Err(error) => {
                return Err(AppError::MissingInput {
                    path: marker,
                    reason: error.to_string(),
                });
            }
        }
    }
    Ok(None)
}

#[cfg(test)]
mod tests {
    #[cfg(unix)]
    use super::unicode_codex_home;
    #[cfg(unix)]
    use super::validate_runnable;
    use super::{
        absolute_codex_user_home, absolute_from, claude_config_dir_from_value,
        claude_managed_skills, codex_home_from_value, nearest_git_root, resolve_session,
    };
    #[cfg(windows)]
    use super::{codex_admin_skills, executable_names, validate_explicit_executable};
    use crate::cli::{ParsedCommand, parse_command_from};
    use crate::error::ExitCategory;
    use std::ffi::OsString;
    use std::fs;
    use std::path::{Path, PathBuf};
    use std::time::{SystemTime, UNIX_EPOCH};

    struct TestDir(PathBuf);

    impl TestDir {
        fn new(label: &str) -> Self {
            let nonce = SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .expect("clock should be after epoch")
                .as_nanos();
            let path = std::env::temp_dir()
                .join(format!("skillmount-{label}-{}-{nonce}", std::process::id()));
            fs::create_dir_all(&path).expect("fixture should be created");
            Self(path)
        }
    }

    impl Drop for TestDir {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.0);
        }
    }

    fn parsed_session(arguments: &[&str]) -> crate::cli::SessionInput {
        let args =
            std::iter::once(OsString::from("asm")).chain(arguments.iter().map(OsString::from));
        let ParsedCommand::Session(input) = parse_command_from(args).expect("valid CLI") else {
            panic!("expected session");
        };
        input
    }

    #[cfg(windows)]
    #[test]
    fn windows_codex_admin_root_uses_program_data_layout() {
        let root = codex_admin_skills().expect("Windows Codex has an administrator Skill root");
        assert!(root.is_absolute());
        assert!(root.ends_with(Path::new("OpenAI/Codex/skills")));
    }

    #[cfg(unix)]
    fn create_directory_link(target: &Path, link: &Path) -> bool {
        std::os::unix::fs::symlink(target, link).expect("directory-link fixture");
        true
    }

    #[cfg(windows)]
    fn create_directory_link(target: &Path, link: &Path) -> bool {
        match std::os::windows::fs::symlink_dir(target, link) {
            Ok(()) => true,
            Err(error) => {
                if std::env::var_os("SKILLMOUNT_REQUIRE_LINKS").is_some() {
                    panic!("directory-link fixture is required: {error}");
                }
                false
            }
        }
    }

    #[test]
    fn every_relative_wrapper_path_uses_invocation_cwd() {
        let fixture = TestDir::new("relative-paths");
        fs::create_dir_all(fixture.0.join("project/launch")).expect("launch fixture");
        fs::create_dir(fixture.0.join("project/.git")).expect("project marker");
        let input = parsed_session(&[
            "claude",
            "--skills-dir=skills/one",
            "--skills-dir=skills/two",
            "--skills-dir=skills/three",
            "--cwd=project/launch",
            "--project-root=project",
            "--agent-bin=bin/claude",
            "--dry-run",
        ]);

        let context = resolve_session(input, &fixture.0).expect("paths should resolve");

        assert_eq!(
            context.launch_cwd,
            fs::canonicalize(fixture.0.join("project/launch")).unwrap()
        );
        assert_eq!(
            context.project_root,
            fs::canonicalize(fixture.0.join("project")).unwrap()
        );
        assert_eq!(
            context.agent_bin,
            absolute_from(&context.invocation_cwd, Path::new("bin/claude")).unwrap()
        );
        assert_eq!(
            context.skill_sources[0].resolved_path,
            context.invocation_cwd.join("skills/one")
        );
        assert_eq!(
            context.skill_sources[1].resolved_path,
            context.invocation_cwd.join("skills/two")
        );
        assert_eq!(
            context
                .skill_sources
                .iter()
                .map(|source| source.ordinal)
                .collect::<Vec<_>>(),
            [0, 1, 2]
        );
        assert_eq!(
            context.skill_sources[2].resolved_path,
            context.invocation_cwd.join("skills/three")
        );
    }

    #[test]
    fn nearest_git_file_is_a_project_root() {
        let fixture = TestDir::new("git-file");
        fs::write(fixture.0.join(".git"), "gitdir: elsewhere").expect("git marker");
        let nested = fixture.0.join("a/b");
        fs::create_dir_all(&nested).expect("nested fixture");

        assert_eq!(nearest_git_root(&nested).unwrap(), Some(fixture.0.clone()));
    }

    #[test]
    fn nearest_git_directory_is_a_project_root() {
        let fixture = TestDir::new("git-directory");
        fs::create_dir(fixture.0.join(".git")).expect("git marker");
        let nested = fixture.0.join("a/b");
        fs::create_dir_all(&nested).expect("nested fixture");

        assert_eq!(nearest_git_root(&nested).unwrap(), Some(fixture.0.clone()));
    }

    #[test]
    fn linked_git_marker_is_a_project_root() {
        let fixture = TestDir::new("git-link");
        let target = fixture.0.join("git-target");
        fs::create_dir(&target).expect("Git marker target");
        if !create_directory_link(&target, &fixture.0.join(".git")) {
            return;
        }
        let nested = fixture.0.join("a/b");
        fs::create_dir_all(&nested).expect("nested fixture");

        assert_eq!(nearest_git_root(&nested).unwrap(), Some(fixture.0.clone()));
    }

    #[test]
    fn explicit_relative_codex_home_is_canonicalized_for_the_child() {
        let fixture = TestDir::new("relative-codex-home");
        let user_home = fixture.0.join("home");
        let configured = fixture.0.join("configured");
        fs::create_dir(&configured).expect("Codex home fixture");

        let (effective, child_override) =
            codex_home_from_value(&user_home, &fixture.0, Some("configured"))
                .expect("existing relative CODEX_HOME");
        let expected = fs::canonicalize(configured).expect("canonical fixture");

        assert_eq!(effective, expected);
        assert_eq!(child_override, Some(expected));
    }

    #[test]
    fn relative_claude_config_dir_resolves_from_the_child_launch_cwd() {
        let fixture = TestDir::new("relative-claude-config");
        let user_home = fixture.0.join("home");
        let launch_cwd = fixture.0.join("launch");

        assert_eq!(
            claude_config_dir_from_value(
                &user_home,
                &launch_cwd,
                Some(OsString::from("custom-claude")),
            )
            .expect("relative Claude config directory"),
            launch_cwd.join("custom-claude")
        );
        assert_eq!(
            claude_config_dir_from_value(&user_home, &launch_cwd, None)
                .expect("default Claude config directory"),
            user_home.join(".claude")
        );
    }

    #[test]
    fn claude_managed_root_uses_the_documented_platform_layout() {
        let root = claude_managed_skills();
        assert!(root.is_absolute());
        assert!(root.ends_with(Path::new("ClaudeCode/.claude/skills")));
    }

    #[test]
    fn codex_user_home_must_be_absolute_like_the_supported_loader_requires() {
        assert_eq!(
            absolute_codex_user_home(Some(PathBuf::from("relative-home")))
                .expect_err("a relative home is not a Codex discovery root")
                .category(),
            ExitCategory::MissingInput
        );
        assert_eq!(
            absolute_codex_user_home(None)
                .expect_err("an absent home is not a Codex discovery root")
                .category(),
            ExitCategory::MissingInput
        );
    }

    #[test]
    fn missing_explicit_codex_home_is_rejected() {
        let fixture = TestDir::new("missing-codex-home");
        let error = codex_home_from_value(&fixture.0, &fixture.0, Some("missing"))
            .expect_err("Codex rejects a configured home that does not exist");

        assert_eq!(error.category(), ExitCategory::MissingInput);
        assert!(error.to_string().contains("CODEX_HOME"));
    }

    #[cfg(unix)]
    #[test]
    fn non_unicode_codex_home_is_ignored_like_codex() {
        use std::os::unix::ffi::OsStringExt;

        let value = Err(std::env::VarError::NotUnicode(OsString::from_vec(vec![
            0xff,
        ])));
        assert_eq!(unicode_codex_home(value), None);
    }

    #[test]
    fn launch_cwd_is_the_fallback_without_git() {
        let fixture = TestDir::new("no-git");
        let input = parsed_session(&["claude", "--skills-dir=skills", "--dry-run"]);
        let context = resolve_session(input, &fixture.0).expect("paths should resolve");
        assert_eq!(context.project_root, context.launch_cwd);
    }

    #[test]
    fn every_agent_project_root_must_contain_launch_cwd() {
        for agent in ["codex", "claude"] {
            let fixture = TestDir::new(&format!("{agent}-containment"));
            fs::create_dir_all(fixture.0.join("launch")).expect("launch fixture");
            fs::create_dir_all(fixture.0.join("other")).expect("project fixture");
            let input = parsed_session(&[
                agent,
                "--skills-dir=skills",
                "--cwd=launch",
                "--project-root=other",
            ]);
            let error = resolve_session(input, &fixture.0).expect_err("containment should fail");
            assert_eq!(error.category(), ExitCategory::Usage);
            assert!(error.to_string().contains(if agent == "codex" {
                "Codex project root"
            } else {
                "Claude project root"
            }));
        }
    }

    #[test]
    fn every_agent_explicit_project_root_must_match_the_discovered_root() {
        for agent in ["codex", "claude"] {
            let fixture = TestDir::new(&format!("{agent}-project-root-match"));
            fs::create_dir(fixture.0.join(".git")).expect("Git root marker");
            fs::create_dir_all(fixture.0.join("nested/deep")).expect("nested fixture");
            let input = parsed_session(&[
                agent,
                "--skills-dir=skills",
                "--cwd=nested/deep",
                "--project-root=nested",
            ]);

            let error = resolve_session(input, &fixture.0)
                .expect_err("the wrapper and child must use the same project root");

            assert_eq!(error.category(), ExitCategory::Usage);
            assert!(
                error
                    .to_string()
                    .contains("does not match the default root")
            );
        }
    }

    #[test]
    fn a_missing_explicit_agent_fails_before_a_mutating_session_can_plan() {
        for agent in ["codex", "claude"] {
            let fixture = TestDir::new(&format!("{agent}-missing-agent"));
            let input =
                parsed_session(&[agent, "--skills-dir=skills", "--agent-bin=missing-agent"]);

            let error = resolve_session(input, &fixture.0)
                .expect_err("an explicit missing executable must fail during context resolution");

            assert_eq!(error.category(), ExitCategory::MissingInput);
        }
    }

    #[cfg(unix)]
    #[test]
    fn execute_permission_is_checked_for_the_effective_user_not_any_mode_bit() {
        use std::os::unix::fs::PermissionsExt;

        let fixture = TestDir::new("effective-execute-access");
        let executable = fixture.0.join("owner-cannot-execute");
        fs::write(&executable, "#!/bin/sh\nexit 0\n").expect("executable fixture");
        fs::set_permissions(&executable, fs::Permissions::from_mode(0o010))
            .expect("group-only execute mode");

        let error = validate_runnable(&executable)
            .expect_err("an owner cannot use the group's execute bit on their own file");

        assert_eq!(error.category(), ExitCategory::MissingInput);
        assert!(error.to_string().contains("effective user"));
    }

    #[cfg(windows)]
    #[test]
    fn an_explicit_batch_agent_is_rejected_as_implicit_shell_execution() {
        let fixture = TestDir::new("batch-agent");
        fs::write(fixture.0.join("codex.cmd"), "@exit /b 0\r\n").expect("batch fixture");
        let input = parsed_session(&["codex", "--skills-dir=skills", "--agent-bin=codex.cmd"]);

        let error = resolve_session(input, &fixture.0)
            .expect_err("a batch file would require an implicit command shell");

        assert_eq!(error.category(), ExitCategory::Usage);
        assert!(error.to_string().contains("implicit cmd.exe"));
    }

    #[cfg(windows)]
    #[test]
    fn an_exe_named_symlink_to_a_batch_file_is_rejected_after_canonicalization() {
        let fixture = TestDir::new("batch-agent-alias");
        let batch = fixture.0.join("codex.cmd");
        let alias = fixture.0.join("codex.exe");
        fs::write(&batch, "@exit /b 0\r\n").expect("batch fixture");
        if let Err(error) = std::os::windows::fs::symlink_file(&batch, &alias) {
            if std::env::var_os("SKILLMOUNT_REQUIRE_LINKS").is_some() {
                panic!("file-link fixture is required: {error}");
            }
            return;
        }

        let error = validate_explicit_executable(&alias)
            .expect_err("canonical batch targets still require an implicit shell");

        assert_eq!(error.category(), ExitCategory::Usage);
        assert!(error.to_string().contains("implicit cmd.exe"));
    }

    #[cfg(windows)]
    #[test]
    fn path_resolution_uses_the_native_exe_candidate_only() {
        assert_eq!(
            executable_names(std::ffi::OsStr::new("codex")),
            [OsString::from("codex.exe")]
        );
    }

    #[cfg(windows)]
    #[test]
    fn drive_relative_wrapper_paths_are_rejected_as_ambiguous() {
        let fixture = TestDir::new("drive-relative");
        let error = claude_config_dir_from_value(
            &fixture.0.join("home"),
            &fixture.0,
            Some(OsString::from("C:config")),
        )
        .expect_err("drive-relative CLAUDE_CONFIG_DIR must fail closed");
        assert_eq!(error.category(), ExitCategory::Usage);
        assert!(error.to_string().contains("drive-relative Windows path"));

        for arguments in [
            &["claude", "--skills-dir=skills", "--cwd=C:launch"][..],
            &["claude", "--skills-dir=skills", "--project-root=C:project"][..],
            &["claude", "--skills-dir=skills", "--agent-bin=C:bin/claude"][..],
            &["claude", "--skills-dir=C:skills"][..],
        ] {
            let error = resolve_session(parsed_session(arguments), &fixture.0)
                .expect_err("drive-relative paths must fail closed");
            assert_eq!(error.category(), ExitCategory::Usage);
            assert!(error.to_string().contains("drive-relative Windows path"));
        }
    }
}
