//! Invocation-relative path resolution and project-root discovery.

use std::ffi::OsStr;
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
    let agent_bin = match input.agent_bin {
        Some(path) => absolute_from(&invocation_cwd, &path)?,
        None => PathBuf::from(input.agent.executable_name()),
    };

    Ok(RunContext {
        agent: input.agent,
        invocation_cwd,
        launch_cwd,
        project_root,
        skill_sources,
        agent_bin,
        passthrough_args: input.passthrough_args,
        options: input.options,
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
