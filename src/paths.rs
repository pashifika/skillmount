//! Invocation-relative path resolution and project-root discovery.

use std::ffi::{OsStr, OsString};
use std::fs;
use std::path::{Component, Path, PathBuf};

use crate::cli::SessionInput;
use crate::domain::{AgentId, RunContext, SourceOccurrence};
use crate::error::AppError;

pub(crate) fn resolve_session(
    input: SessionInput,
    invocation_cwd: &Path,
) -> Result<RunContext, AppError> {
    let invocation_cwd = canonical_directory(invocation_cwd)?;
    let launch_cwd = match input.cwd.as_deref() {
        Some(path) => canonical_directory(&absolute_from(&invocation_cwd, path)?)?,
        None => invocation_cwd.clone(),
    };

    let project_root = match input.project_root.as_deref() {
        Some(path) => canonical_directory(&absolute_from(&invocation_cwd, path)?)?,
        None => nearest_git_root(&launch_cwd)?.unwrap_or_else(|| launch_cwd.clone()),
    };

    if input.agent == AgentId::Codex && !launch_cwd.starts_with(&project_root) {
        return Err(AppError::Usage(format!(
            "Codex project root {} does not contain launch CWD {}",
            project_root.display(),
            launch_cwd.display()
        )));
    }

    let skill_sources = resolve_source_occurrences(&input.skills_dirs, &invocation_cwd)?;
    let resolve_codex_executable = input.agent == AgentId::Codex && !input.options.dry_run;
    let agent_bin = match input.agent_bin {
        Some(path) => {
            let resolved = absolute_from(&invocation_cwd, &path)?;
            if resolve_codex_executable {
                validate_explicit_executable(&resolved)?
            } else {
                resolved
            }
        }
        None if resolve_codex_executable => {
            resolve_path_executable(input.agent.executable_name(), &invocation_cwd)?
        }
        None => PathBuf::from(input.agent.executable_name()),
    };

    Ok(RunContext {
        agent: input.agent,
        invocation_cwd,
        launch_cwd,
        project_root,
        skill_sources,
        session_id: None,
        agent_bin,
        passthrough_args: input.passthrough_args,
        options: input.options,
    })
}

fn validate_explicit_executable(path: &Path) -> Result<PathBuf, AppError> {
    reject_implicit_shell(path)?;
    validate_runnable(path)?;
    fs::canonicalize(path).map_err(|error| AppError::MissingInput {
        path: path.to_path_buf(),
        reason: format!("cannot resolve agent executable: {error}"),
    })
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
            if reject_implicit_shell(&candidate).is_ok() && validate_runnable(&candidate).is_ok() {
                return fs::canonicalize(&candidate).map_err(|error| AppError::MissingInput {
                    path: candidate,
                    reason: format!("cannot resolve agent executable: {error}"),
                });
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
    let mut com = name.to_os_string();
    com.push(".com");
    vec![name.to_os_string(), exe, com]
}

#[cfg(unix)]
fn validate_runnable(path: &Path) -> Result<(), AppError> {
    use std::os::unix::fs::PermissionsExt;

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
    if metadata.permissions().mode() & 0o111 == 0 {
        return Err(AppError::MissingInput {
            path: path.to_path_buf(),
            reason: "agent executable has no execute permission".to_owned(),
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
    Ok(RunContext {
        agent,
        launch_cwd: invocation_cwd.clone(),
        skill_sources: resolve_source_occurrences(skills_dirs, &invocation_cwd)?,
        invocation_cwd,
        project_root,
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
        match fs::symlink_metadata(&marker) {
            Ok(metadata) if metadata.is_dir() || metadata.is_file() => {
                return Ok(Some(ancestor.to_path_buf()));
            }
            Ok(_) => {}
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
    use super::{absolute_from, nearest_git_root, resolve_session};
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

    #[test]
    fn every_relative_wrapper_path_uses_invocation_cwd() {
        let fixture = TestDir::new("relative-paths");
        fs::create_dir_all(fixture.0.join("launch")).expect("launch fixture");
        fs::create_dir_all(fixture.0.join("project")).expect("project fixture");
        let input = parsed_session(&[
            "claude",
            "--skills-dir=skills/one",
            "--skills-dir=skills/two",
            "--skills-dir=skills/three",
            "--cwd=launch",
            "--project-root=project",
            "--agent-bin=bin/claude",
        ]);

        let context = resolve_session(input, &fixture.0).expect("paths should resolve");

        assert_eq!(
            context.launch_cwd,
            fs::canonicalize(fixture.0.join("launch")).unwrap()
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
    fn launch_cwd_is_the_fallback_without_git() {
        let fixture = TestDir::new("no-git");
        let input = parsed_session(&["claude", "--skills-dir=skills"]);
        let context = resolve_session(input, &fixture.0).expect("paths should resolve");
        assert_eq!(context.project_root, context.launch_cwd);
    }

    #[test]
    fn codex_project_root_must_contain_launch_cwd() {
        let fixture = TestDir::new("containment");
        fs::create_dir_all(fixture.0.join("launch")).expect("launch fixture");
        fs::create_dir_all(fixture.0.join("other")).expect("project fixture");
        let input = parsed_session(&[
            "codex",
            "--skills-dir=skills",
            "--cwd=launch",
            "--project-root=other",
        ]);
        let error = resolve_session(input, &fixture.0).expect_err("containment should fail");
        assert_eq!(error.category(), ExitCategory::Usage);
    }

    #[test]
    fn a_missing_explicit_agent_fails_before_a_mutating_session_can_plan() {
        let fixture = TestDir::new("missing-agent");
        let input = parsed_session(&["codex", "--skills-dir=skills", "--agent-bin=missing-codex"]);

        let error = resolve_session(input, &fixture.0)
            .expect_err("an explicit missing executable must fail during context resolution");

        assert_eq!(error.category(), ExitCategory::MissingInput);
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
    fn drive_relative_wrapper_paths_are_rejected_as_ambiguous() {
        let fixture = TestDir::new("drive-relative");
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
