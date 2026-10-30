//! Bounded read-only projection of OMP's initial custom-directory settings.
//!
//! Skill discovery and name selection belong to OMP. Only array precedence is reproduced so a
//! session overlay can append roots without replacing the operator's effective configured roots.

use crate::error::AppError;
use crate::mount::resolve::{PathKind, classify};
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

const MAX_SETTINGS_BYTES: u64 = 1 << 20;
const MAX_DIRECTORIES: usize = 1_024;

#[derive(Debug, Clone, PartialEq)]
enum Value {
    Null,
    Bool(bool),
    String(String),
    Number(f64),
    Array(Vec<Value>),
    Object(BTreeMap<String, Value>),
}
#[derive(Clone, Copy)]
enum Syntax {
    Json,
    Yaml,
    Toml,
}

/// Inputs whose initial effective directory list is preserved by the session overlay.
pub(super) struct Configuration {
    pub(super) directories: Vec<String>,
    pub(super) inputs: Vec<PathBuf>,
}

/// Reads initial settings without launching OMP or migrating its global configuration.
pub(super) fn load(agent_dir: &Path, cwd: &Path, home: &Path) -> Result<Configuration, AppError> {
    let mut directories = Vec::new();
    let mut inputs = Vec::new();
    let globals = [agent_dir.join("config.yml"), agent_dir.join("config.yaml")];
    inputs.extend(globals.iter().cloned());
    let mut global_found = false;
    for path in globals {
        if let Some(text) = read_regular(&path)? {
            apply(
                &mut directories,
                parse_input(&path, &text, Syntax::Yaml)?,
                false,
                &path,
            )?;
            global_found = true;
            break;
        }
    }
    for name in ["settings.json", "agent.db"] {
        let legacy = agent_dir.join(name);
        inputs.push(legacy.clone());
        if !global_found && classify(&legacy)?.kind != PathKind::Missing {
            return Err(invalid_settings(
                &legacy,
                "legacy settings have not been migrated; start OMP directly once before using SkillMount",
            ));
        }
    }
    let project = [
        (".omp/settings.json", Syntax::Json),
        (".omp/config.yml", Syntax::Yaml),
        (".claude/settings.json", Syntax::Json),
        (".codex/config.toml", Syntax::Toml),
        (".gemini/settings.json", Syntax::Json),
        ("opencode.json", Syntax::Json),
        ("opencode.jsonc", Syntax::Json),
        (".opencode/opencode.json", Syntax::Json),
        (".opencode/opencode.jsonc", Syntax::Json),
        (".cursor/settings.json", Syntax::Json),
    ];
    let at_home = std::fs::canonicalize(home).is_ok_and(|path| path == cwd);
    for (relative, syntax) in project {
        let path = cwd.join(relative);
        inputs.push(path.clone());
        if at_home && relative.starts_with(".omp/") {
            continue;
        }
        if let Some(text) = read_regular(&path)? {
            if relative.contains("opencode") && (text.contains("{env:") || text.contains("{file:"))
            {
                return Err(invalid_settings(
                    &path,
                    "OpenCode substitutions cannot be projected safely; use plain directory values or run OMP directly",
                ));
            }
            apply(
                &mut directories,
                parse_input(&path, &text, syntax)?,
                true,
                &path,
            )?;
        }
    }
    Ok(Configuration {
        directories,
        inputs,
    })
}

fn parse_input(path: &Path, text: &str, syntax: Syntax) -> Result<Value, AppError> {
    parse(text, syntax).map_err(|reason| invalid_settings(path, &reason))
}

fn apply(
    directories: &mut Vec<String>,
    value: Value,
    project: bool,
    path: &Path,
) -> Result<(), AppError> {
    let Value::Object(mut root) = value else {
        return Ok(());
    };
    let Some(skills) = root.remove("skills") else {
        return Ok(());
    };
    let Value::Object(mut skills) = skills else {
        // OMP drops project-level scalar shadows of a settings group before deep merging.
        if !project {
            directories.clear();
        }
        return Ok(());
    };
    let Some(value) = skills.remove("customDirectories") else {
        return Ok(());
    };
    let Value::Array(values) = value else {
        return Err(invalid_settings(
            path,
            "skills.customDirectories must be an array of directory strings",
        ));
    };
    if values.len() > MAX_DIRECTORIES {
        return Err(invalid_settings(
            path,
            "skills.customDirectories exceeds the 1024-directory inspection bound",
        ));
    }
    *directories = values
        .into_iter()
        .map(|value| match value {
            Value::String(value) => Ok(value),
            _ => Err(invalid_settings(
                path,
                "skills.customDirectories must contain only strings",
            )),
        })
        .collect::<Result<_, _>>()?;
    Ok(())
}

fn invalid_settings(path: &Path, reason: &str) -> AppError {
    AppError::MissingInput {
        path: path.to_path_buf(),
        reason: format!("cannot preserve OMP customDirectories: {reason}"),
    }
}

pub(super) fn read_regular(path: &Path) -> Result<Option<String>, AppError> {
    let resolved = classify(path)?;
    match resolved.kind {
        PathKind::Missing => return Ok(None),
        PathKind::NotDirectory => {}
        other => {
            return Err(AppError::MissingInput {
                path: path.to_path_buf(),
                reason: format!(
                    "OMP settings input resolves as {} rather than a regular file",
                    other.label()
                ),
            });
        }
    }

    let bytes = crate::catalog::frontmatter::read_bounded_regular_file(
        path,
        "OMP settings input",
        MAX_SETTINGS_BYTES,
    )
    .map_err(|reason| AppError::MissingInput {
        path: path.to_path_buf(),
        reason,
    })?;
    let text = String::from_utf8(bytes).map_err(|_| AppError::MissingInput {
        path: path.to_path_buf(),
        reason: "OMP settings input is not valid UTF-8".to_owned(),
    })?;
    Ok(Some(text))
}

fn parse(text: &str, syntax: Syntax) -> Result<Value, String> {
    let value = match syntax {
        Syntax::Json => serde_json::from_str::<serde_json::Value>(text)
            .map(from_json)
            .map_err(|error| error.to_string())?,
        Syntax::Yaml => serde_yaml_ng::from_str::<serde_yaml_ng::Value>(text)
            .map(from_yaml)
            .map_err(|error| error.to_string())?,
        // `toml::Value: FromStr` parses a bare value expression, so a whole document has to be
        // requested as a table.
        Syntax::Toml => toml::from_str::<toml::Table>(text)
            .map(|table| from_toml(toml::Value::Table(table)))
            .map_err(|error| error.to_string())?,
    };
    // An empty document is an empty mapping in OMP; anything else at the root is rejected.
    match value {
        Value::Null => Ok(Value::Object(BTreeMap::new())),
        Value::Object(_) => Ok(value),
        _ => Err("settings document must contain a mapping at its root".to_owned()),
    }
}

fn from_json(value: serde_json::Value) -> Value {
    match value {
        serde_json::Value::Null => Value::Null,
        serde_json::Value::Bool(value) => Value::Bool(value),
        serde_json::Value::Number(value) => Value::Number(value.as_f64().unwrap_or(f64::NAN)),
        serde_json::Value::String(value) => Value::String(value),
        serde_json::Value::Array(items) => Value::Array(items.into_iter().map(from_json).collect()),
        serde_json::Value::Object(entries) => Value::Object(
            entries
                .into_iter()
                .map(|(key, value)| (key, from_json(value)))
                .collect(),
        ),
    }
}

fn from_yaml(value: serde_yaml_ng::Value) -> Value {
    match value {
        serde_yaml_ng::Value::Null => Value::Null,
        serde_yaml_ng::Value::Bool(value) => Value::Bool(value),
        serde_yaml_ng::Value::Number(value) => Value::Number(value.as_f64().unwrap_or(f64::NAN)),
        serde_yaml_ng::Value::String(value) => Value::String(value),
        serde_yaml_ng::Value::Sequence(items) => {
            Value::Array(items.into_iter().map(from_yaml).collect())
        }
        serde_yaml_ng::Value::Mapping(entries) => Value::Object(
            entries
                .into_iter()
                .filter_map(|(key, value)| {
                    key.as_str()
                        .map(|key| (key.to_owned(), from_yaml(value.clone())))
                })
                .collect(),
        ),
        serde_yaml_ng::Value::Tagged(tagged) => from_yaml(tagged.value),
    }
}

fn from_toml(value: toml::Value) -> Value {
    match value {
        toml::Value::String(value) => Value::String(value),
        toml::Value::Integer(value) => Value::Number(if let Ok(value) = i32::try_from(value) {
            f64::from(value)
        } else {
            f64::NAN
        }),
        toml::Value::Float(value) => Value::Number(value),
        toml::Value::Boolean(value) => Value::Bool(value),
        toml::Value::Datetime(value) => Value::String(value.to_string()),
        toml::Value::Array(items) => Value::Array(items.into_iter().map(from_toml).collect()),
        toml::Value::Table(entries) => Value::Object(
            entries
                .into_iter()
                .map(|(key, value)| (key, from_toml(value)))
                .collect(),
        ),
    }
}

#[cfg(test)]
mod tests {
    use super::{Syntax, apply, parse};
    use std::path::Path;

    #[test]
    fn arrays_replace_and_unrelated_settings_preserve_the_current_directories() {
        let mut dirs = vec!["old".to_owned()];
        for (input, expected) in [
            (
                r#"{"skills":{"customDirectories":["first","second"]}}"#,
                vec!["first", "second"],
            ),
            (r#"{"skills":{"enabled":false}}"#, vec!["first", "second"]),
            (r#"{"skills":{"customDirectories":[]}}"#, vec![]),
        ] {
            apply(
                &mut dirs,
                parse(input, Syntax::Json).unwrap(),
                true,
                Path::new("fixture"),
            )
            .unwrap();
            assert_eq!(dirs, expected);
        }
    }

    #[test]
    fn project_group_shadows_are_ignored_but_global_shadows_replace() {
        for input in [
            "skills: null",
            "skills: false",
            "skills: []",
            "skills: text",
        ] {
            let mut dirs = vec!["existing".to_owned()];
            apply(
                &mut dirs,
                parse(input, Syntax::Yaml).unwrap(),
                true,
                Path::new("fixture"),
            )
            .unwrap();
            assert_eq!(dirs, ["existing"]);
            apply(
                &mut dirs,
                parse(input, Syntax::Yaml).unwrap(),
                false,
                Path::new("fixture"),
            )
            .unwrap();
            assert!(dirs.is_empty());
        }
    }

    #[test]
    fn paths_remain_literal_and_invalid_directory_arrays_fail_closed() {
        let mut dirs = Vec::new();
        apply(
            &mut dirs,
            parse(
                "[skills]\ncustomDirectories = ['~/space path', 'C:\\quoted']",
                Syntax::Toml,
            )
            .unwrap(),
            true,
            Path::new("fixture"),
        )
        .unwrap();
        assert_eq!(dirs, ["~/space path", "C:\\quoted"]);
        for input in [
            "skills: {customDirectories: null}",
            "skills: {customDirectories: false}",
            "skills: {customDirectories: [3]}",
        ] {
            assert!(
                apply(
                    &mut dirs,
                    parse(input, Syntax::Yaml).unwrap(),
                    true,
                    Path::new("fixture")
                )
                .is_err()
            );
        }
    }
}
