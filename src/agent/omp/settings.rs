//! Bounded reproduction of the OMP 17.2.9 settings layers that can hide a Skill.
//!
//! Only the fields that decide whether a selected Skill is discoverable are projected out, but the
//! layers, their order, and the merge semantics are OMP's: plain objects merge recursively, arrays
//! and scalars replace wholesale, and an `undefined` key is a no-op. Every read is no-follow and
//! byte-bounded, and nothing here writes, quarantines, or migrates an operator's configuration.

use std::collections::BTreeMap;
use std::fs;
use std::path::{Path, PathBuf};

use crate::error::{AppError, CatalogError};
use crate::mount::resolve::{PathKind, classify};

/// Maximum bytes read from one settings input.
///
/// A settings file is hand-written configuration. A larger input is refused rather than streamed,
/// so a hostile or corrupt file cannot make planning unbounded.
const MAX_SETTINGS_BYTES: u64 = 1 << 20;
/// Maximum entries read from one untrusted string array.
const MAX_LIST_ENTRIES: usize = 1_024;

/// The two global configuration filenames OMP tries, first existing wins.
const MAIN_CONFIG_FILENAMES: [&str; 2] = ["config.yml", "config.yaml"];

/// A generic settings tree, independent of the syntax it was parsed from.
#[derive(Debug, Clone, PartialEq)]
pub(super) enum Value {
    Null,
    Bool(bool),
    String(String),
    Number(f64),
    Array(Vec<Value>),
    Object(BTreeMap<String, Value>),
}

impl Value {
    fn as_object(&self) -> Option<&BTreeMap<String, Value>> {
        match self {
            Self::Object(map) => Some(map),
            _ => None,
        }
    }

    fn is_object(&self) -> bool {
        matches!(self, Self::Object(_))
    }

    /// Walks a dotted settings path.
    fn get(&self, path: &str) -> Option<&Self> {
        let mut cursor = self;
        for segment in path.split('.') {
            cursor = cursor.as_object()?.get(segment)?;
        }
        Some(cursor)
    }

    fn bool_at(&self, path: &str) -> Option<bool> {
        match self.get(path)? {
            Self::Bool(value) => Some(*value),
            _ => None,
        }
    }

    fn strings_at(&self, path: &str) -> Vec<String> {
        match self.get(path) {
            Some(Self::Array(items)) => items
                .iter()
                .filter_map(|item| match item {
                    Self::String(value) => Some(value.clone()),
                    _ => None,
                })
                .collect(),
            _ => Vec::new(),
        }
    }

    /// Reads a string array, refusing one longer than this release will process.
    ///
    /// OMP itself is unbounded here, but every entry is either a Skill root to scan or a glob
    /// matched against every discovered name, so an untrusted document would otherwise decide the
    /// cost of planning. Crossing the bound fails closed rather than truncating, because a
    /// truncated filter list would model a namespace OMP does not have.
    fn bounded_strings_at(&self, path: &str) -> Result<Vec<String>, AppError> {
        let values = self.strings_at(path);
        if values.len() > MAX_LIST_ENTRIES {
            return Err(AppError::MissingInput {
                path: PathBuf::from(path),
                reason: format!(
                    "OMP setting {path} names {} entries, which exceeds the \
                     {MAX_LIST_ENTRIES}-entry inspection bound",
                    values.len()
                ),
            });
        }
        Ok(values)
    }
}

/// Merges `overlay` onto `base` with OMP's own semantics.
///
/// Plain objects merge recursively. Arrays, scalars, and `null` replace wholesale, which is why an
/// overlay can never append to an operator's `skills.customDirectories`.
fn merge(base: &mut Value, overlay: Value) {
    if !base.is_object() || !overlay.is_object() {
        *base = overlay;
        return;
    }
    let (Value::Object(base_map), Value::Object(overlay_map)) = (base, overlay) else {
        unreachable!("both values were just proven to be objects");
    };
    for (key, value) in overlay_map {
        match base_map.get_mut(&key) {
            Some(existing) if existing.is_object() && value.is_object() => {
                merge(existing, value);
            }
            _ => {
                base_map.insert(key, value);
            }
        }
    }
}

/// One project settings provider, in OMP's descending priority order.
///
/// The fold is later-wins over this descending list, so the lowest-priority provider wins a
/// conflicting project key. That inversion is OMP's behavior, not a simplification.
const PROJECT_SETTINGS_INPUTS: &[(&str, &str, Syntax)] = &[
    ("native", ".omp/settings.json", Syntax::Json),
    ("native", ".omp/config.yml", Syntax::Yaml),
    ("claude", ".claude/settings.json", Syntax::Json),
    ("codex", ".codex/config.toml", Syntax::Toml),
    ("gemini", ".gemini/settings.json", Syntax::Json),
    ("opencode", "opencode.json", Syntax::Json),
    ("cursor", ".cursor/settings.json", Syntax::Json),
];

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Syntax {
    Json,
    Yaml,
    Toml,
}

/// The Skill-affecting projection of the merged OMP settings.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) struct SkillSettings {
    /// Master gate. When false OMP discovers no Skill at all.
    pub(super) enabled: bool,
    /// Per-provider-and-level toggles, keyed exactly as OMP keys them.
    ///
    /// Every derived answer — the `native` project and user gates, and the third-party fold that
    /// governs a provider without its own toggle — is computed from this map rather than cached, so
    /// no two fields can disagree.
    pub(super) toggles: BTreeMap<String, bool>,
    /// Configured extra Skill roots, in order.
    pub(super) custom_directories: Vec<String>,
    /// Glob patterns that exclude a Skill by name.
    pub(super) ignored_skills: Vec<String>,
    /// Glob patterns that allow-list Skill names; empty means allow all.
    pub(super) include_skills: Vec<String>,
    /// Skill names disabled through the top-level `disabledExtensions` list.
    pub(super) disabled_skills: Vec<String>,
    /// Provider ids removed by the top-level `disabledProviders` list.
    ///
    /// OMP loads this into the capability registry before any Skill root is scanned
    /// (`capability/index.ts:285-289`), and `filterProviders` then drops every provider whose id
    /// it names (`capability/index.ts:239`). Because `<launch-cwd>/.omp/skills` is served only by
    /// `native`, listing that id makes the mount destination unreadable while every other check
    /// still passes.
    pub(super) disabled_providers: Vec<String>,
    /// Every settings input that existed and contributed, in load order.
    pub(super) inputs: Vec<PathBuf>,
}

impl Default for SkillSettings {
    fn default() -> Self {
        // OMP's schema defaults: every toggle true, every list empty.
        let mut toggles = BTreeMap::new();
        for key in TOGGLE_KEYS {
            toggles.insert((*key).to_owned(), true);
        }
        Self {
            enabled: true,
            toggles,
            custom_directories: Vec::new(),
            ignored_skills: Vec::new(),
            include_skills: Vec::new(),
            disabled_skills: Vec::new(),
            disabled_providers: Vec::new(),
            inputs: Vec::new(),
        }
    }
}

/// Every source-toggle key OMP reads, with the `(provider, level)` pair each one gates.
pub(super) const TOGGLE_KEYS: &[&str] = &[
    "enableAgentsProject",
    "enableAgentsUser",
    "enableClaudeProject",
    "enableClaudeUser",
    "enableCodexUser",
    "enablePiProject",
    "enablePiUser",
];

/// The five toggles OMP folds together for a provider that has none of its own.
const THIRD_PARTY_TOGGLE_KEYS: &[&str] = &[
    "enableClaudeProject",
    "enableClaudeUser",
    "enableCodexUser",
    "enablePiProject",
    "enablePiUser",
];

/// Loads the OMP settings layers and projects the Skill-affecting fields.
///
/// # Errors
///
/// Returns [`CatalogError::InvalidSelectedSkill`]-free data errors as [`AppError::Catalog`] when a
/// trusted layer is malformed, and [`AppError::MissingInput`] when an input exists but cannot be
/// read. A third-party provider file that OMP would only warn about is skipped the same way.
pub(super) fn load(agent_dir: &Path, launch_cwd: &Path) -> Result<SkillSettings, AppError> {
    let mut merged = Value::Object(BTreeMap::new());
    let mut inputs = Vec::new();

    if let Some((path, value)) = load_global(agent_dir)? {
        inputs.push(path);
        merge(&mut merged, value);
    }
    for (path, value) in load_project(launch_cwd)? {
        inputs.push(path);
        merge(&mut merged, value);
    }

    project(&merged, inputs)
}

/// Loads the first existing global configuration file, exactly as OMP picks it.
fn load_global(agent_dir: &Path) -> Result<Option<(PathBuf, Value)>, AppError> {
    for name in MAIN_CONFIG_FILENAMES {
        let path = agent_dir.join(name);
        let Some(text) = read_regular(&path)? else {
            continue;
        };
        // The global file is OMP's own trusted configuration. OMP quarantines and then throws on a
        // malformed one, so SkillMount refuses to plan against it rather than reading `{}` and
        // silently modelling the wrong namespace.
        let value =
            parse(&text, Syntax::Yaml).map_err(|reason| invalid_settings(&path, &reason))?;
        return Ok(Some((path, value)));
    }
    Ok(None)
}

/// Loads every project settings input in OMP's descending-priority order.
fn load_project(launch_cwd: &Path) -> Result<Vec<(PathBuf, Value)>, AppError> {
    let native_scope_populated = directory_has_entries(&launch_cwd.join(".omp"))?;
    let mut layers = Vec::new();

    for (provider, relative, syntax) in PROJECT_SETTINGS_INPUTS {
        // OMP only consults its own project scope when that directory is non-empty.
        if *provider == "native" && !native_scope_populated {
            continue;
        }
        let path = launch_cwd.join(relative);
        let Some(text) = read_regular(&path)? else {
            continue;
        };
        match parse(&text, *syntax) {
            Ok(value) => layers.push((path, value)),
            // A third-party provider file is only warned about and skipped by OMP. Failing here
            // would refuse a session OMP itself starts, so the layer is dropped the same way — and
            // the winner-visibility check below still fails closed if the result hides a selection.
            Err(_) if *provider != "native" => {}
            Err(reason) => return Err(invalid_settings(&path, &reason)),
        }
    }
    Ok(layers)
}

/// Reads one bounded settings or manifest input without blocking on a FIFO or device.
///
/// A file larger than the inspection bound is refused rather than streamed, so a hostile or corrupt
/// input cannot make planning unbounded.
///
/// A symbolic link to a regular file is followed, because OMP's own loader follows it and a
/// dotfile-managed `config.yml` is an ordinary setup; refusing it would fail sessions OMP serves.
/// What is refused is anything that is not a regular file once opened. `classify` cannot decide
/// that on its own - it folds a regular file, a FIFO, a socket, and a device into one
/// [`PathKind::NotDirectory`] state - so the read goes through the same helper the `SKILL.md` path
/// uses: `O_NONBLOCK` on Unix, and a regular-file check *after* opening, which also closes the
/// window a path swapped after `classify` would otherwise leave.
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

/// Returns whether a directory exists and holds at least one entry.
fn directory_has_entries(path: &Path) -> Result<bool, AppError> {
    let resolved = classify(path)?;
    if !matches!(resolved.kind, PathKind::Directory | PathKind::DirectoryLink) {
        return Ok(false);
    }
    let mut entries = fs::read_dir(path).map_err(|error| AppError::MissingInput {
        path: path.to_path_buf(),
        reason: format!("cannot enumerate the OMP project scope: {error}"),
    })?;
    Ok(entries.next().is_some())
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

/// Projects the merged tree onto the fields that decide Skill visibility.
///
/// # Errors
///
/// Returns [`AppError::MissingInput`] when an untrusted array names more entries than this release
/// will match against, so a 1 MiB document cannot decide how much work planning does.
fn project(merged: &Value, inputs: Vec<PathBuf>) -> Result<SkillSettings, AppError> {
    let mut settings = SkillSettings {
        inputs,
        ..SkillSettings::default()
    };
    if let Some(enabled) = merged.bool_at("skills.enabled") {
        settings.enabled = enabled;
    }
    for key in TOGGLE_KEYS {
        if let Some(value) = merged.bool_at(&format!("skills.{key}")) {
            settings.toggles.insert((*key).to_owned(), value);
        }
    }
    settings.custom_directories = merged.bounded_strings_at("skills.customDirectories")?;
    settings.ignored_skills = merged.bounded_strings_at("skills.ignoredSkills")?;
    settings.include_skills = merged.bounded_strings_at("skills.includeSkills")?;
    settings.disabled_skills = merged
        .bounded_strings_at("disabledExtensions")?
        .into_iter()
        .filter_map(|entry| entry.strip_prefix("skill:").map(str::to_owned))
        .collect();
    settings.disabled_providers = merged.bounded_strings_at("disabledProviders")?;
    Ok(settings)
}

impl SkillSettings {
    fn toggle(&self, key: &str) -> bool {
        self.toggles.get(key).copied().unwrap_or(true)
    }

    /// Returns whether a `(provider, level)` pair is enabled, exactly as OMP decides it.
    pub(super) fn source_enabled(&self, provider: &str, project_level: bool) -> bool {
        // `disabledProviders` removes the provider from the registry before any scan, so it
        // outranks every per-level toggle.
        if self.provider_disabled(provider) {
            return false;
        }
        match (provider, project_level) {
            // `omp-managed` has no toggle, and `skills.customDirectories` is scanned outside the
            // provider registry altogether, so OMP never applies a source toggle to either
            // (`extensibility/skills.ts:266-271` filters a custom directory only on disabled names
            // and the ignore/include patterns).
            ("omp-managed" | "custom", _) => true,
            ("codex", false) => self.toggle("enableCodexUser"),
            ("claude", false) => self.toggle("enableClaudeUser"),
            ("claude", true) => self.toggle("enableClaudeProject"),
            ("native", false) => self.toggle("enablePiUser"),
            ("native", true) => self.toggle("enablePiProject"),
            ("agents", false) => self.toggle("enableAgentsUser"),
            ("agents", true) => self.toggle("enableAgentsProject"),
            // A provider with no dedicated toggle is enabled when any third-party toggle is on.
            _ => THIRD_PARTY_TOGGLE_KEYS.iter().any(|key| self.toggle(key)),
        }
    }

    /// Returns whether `disabledProviders` removes this provider id from OMP's registry.
    ///
    /// `custom` is not a provider id, so the list can never reach a custom directory.
    pub(super) fn provider_disabled(&self, provider: &str) -> bool {
        provider != "custom"
            && self
                .disabled_providers
                .iter()
                .any(|entry| entry == provider)
    }

    /// Returns whether a Skill name survives every configured filter.
    ///
    /// The order is OMP's: `disabledExtensions`, then the source toggle the caller already applied,
    /// then `ignoredSkills`, then `includeSkills`.
    pub(super) fn name_visible(&self, name: &str) -> bool {
        if self.disabled_skills.iter().any(|entry| entry == name) {
            return false;
        }
        if self
            .ignored_skills
            .iter()
            .any(|pattern| glob_matches(pattern, name))
        {
            return false;
        }
        if self.include_skills.is_empty() {
            return true;
        }
        self.include_skills
            .iter()
            .any(|pattern| glob_matches(pattern, name))
    }
}

/// Matches one glob pattern against a Skill name, reproducing `Bun.Glob`.
///
/// OMP matches `skills.ignoredSkills` and `skills.includeSkills` against the bare Skill name with
/// `new Bun.Glob(pattern).match(name)` (`extensibility/skills.ts:181,187`). Supporting only `*`,
/// `?`, and `[...]` left brace alternation, leading `!` negation, `\` escapes, and `[^...]`
/// unhandled, so an operator pattern that hides a Skill in OMP reported it visible here — and a
/// mount planned against that reading is applied and then silently ignored.
///
/// Every rule below was measured against `Bun.Glob` 1.3.14, which is the runtime OMP 17.2.9 pins.
fn glob_matches(pattern: &str, name: &str) -> bool {
    let pattern: Vec<char> = pattern.chars().collect();
    let name: Vec<char> = name.chars().collect();
    // A leading `!` negates the whole match and stacks: `!!a` matches `a`. It is special only at
    // the start, so `a!b` is literal, and `\!a` escapes it.
    let mut negated = false;
    let mut start = 0;
    while pattern.get(start) == Some(&'!') {
        negated = !negated;
        start += 1;
    }
    matches_from(&pattern[start..], &name) != negated
}

fn matches_from(pattern: &[char], name: &[char]) -> bool {
    match pattern.first() {
        None => name.is_empty(),
        Some('{') => match_alternation(pattern, name),
        Some('*') => {
            // `**` crosses a separator, `*` does not. A Skill name rarely holds one, but a
            // frontmatter `name` is arbitrary text and OMP applies the same rule to it.
            let (crosses, rest) = if pattern.get(1) == Some(&'*') {
                (true, &pattern[2..])
            } else {
                (false, &pattern[1..])
            };
            for split in 0..=name.len() {
                if matches_from(rest, &name[split..]) {
                    return true;
                }
                if !crosses && name.get(split) == Some(&'/') {
                    break;
                }
            }
            false
        }
        Some('?') => {
            matches!(name.first(), Some(head) if *head != '/')
                && matches_from(&pattern[1..], &name[1..])
        }
        Some('[') => match (name.first(), match_class(pattern, 0, name)) {
            (Some(_), Some(next)) => matches_from(&pattern[next..], &name[1..]),
            _ => false,
        },
        // An escape binds the next character literally; a trailing `\` matches nothing.
        Some('\\') => match (pattern.get(1), name.first()) {
            (Some(literal), Some(head)) if literal == head => {
                matches_from(&pattern[2..], &name[1..])
            }
            _ => false,
        },
        Some(literal) => name.first() == Some(literal) && matches_from(&pattern[1..], &name[1..]),
    }
}

/// Matches a `{a,b}` group against `name`, trying each top-level alternative with the same tail.
///
/// Alternatives are substituted rather than pre-expanded, so a pattern with several groups costs
/// backtracking rather than a combinatorial rewrite. An unterminated `{` matches nothing at all,
/// which is what `Bun.Glob` does — `{a,b` does not even match itself.
fn match_alternation(pattern: &[char], name: &[char]) -> bool {
    let Some(close) = closing_brace(pattern) else {
        return false;
    };
    let tail = &pattern[close + 1..];
    for alternative in top_level_alternatives(&pattern[1..close]) {
        let mut candidate = alternative;
        candidate.extend_from_slice(tail);
        if matches_from(&candidate, name) {
            return true;
        }
    }
    false
}

/// Returns the index of the `}` closing the group opened at index 0, honouring nesting and escapes.
fn closing_brace(pattern: &[char]) -> Option<usize> {
    let mut depth = 0usize;
    let mut index = 0;
    while index < pattern.len() {
        match pattern[index] {
            '\\' => index += 1,
            '{' => depth += 1,
            '}' => {
                depth -= 1;
                if depth == 0 {
                    return Some(index);
                }
            }
            _ => {}
        }
        index += 1;
    }
    None
}

/// Splits a group body on its top-level commas, keeping nested groups and escapes intact.
fn top_level_alternatives(body: &[char]) -> Vec<Vec<char>> {
    let mut alternatives = Vec::new();
    let mut current = Vec::new();
    let mut depth = 0usize;
    let mut index = 0;
    while index < body.len() {
        let token = body[index];
        match token {
            '\\' => {
                current.push(token);
                if let Some(escaped) = body.get(index + 1) {
                    current.push(*escaped);
                    index += 1;
                }
            }
            '{' => {
                depth += 1;
                current.push(token);
            }
            '}' => {
                depth = depth.saturating_sub(1);
                current.push(token);
            }
            ',' if depth == 0 => alternatives.push(std::mem::take(&mut current)),
            _ => current.push(token),
        }
        index += 1;
    }
    alternatives.push(current);
    alternatives
}

/// Matches one `[...]` class and returns the pattern index just past it.
///
/// Both `[!...]` and `[^...]` negate, and an escape binds inside the class. Unlike `?`, a class may
/// match a separator.
fn match_class(pattern: &[char], open: usize, name: &[char]) -> Option<usize> {
    let candidate = *name.first()?;
    let mut index = open + 1;
    let negated = matches!(pattern.get(index), Some('!' | '^'));
    if negated {
        index += 1;
    }
    let mut matched = false;
    let mut members = 0;
    while let Some(token) = pattern.get(index) {
        if *token == ']' && members > 0 {
            index += 1;
            return (matched != negated).then_some(index);
        }
        members += 1;
        let (token, width) = if *token == '\\' {
            (*pattern.get(index + 1)?, 2)
        } else {
            (*token, 1)
        };
        if pattern.get(index + width) == Some(&'-')
            && pattern
                .get(index + width + 1)
                .is_some_and(|end| *end != ']')
        {
            let end = pattern[index + width + 1];
            if (token..=end).contains(&candidate) {
                matched = true;
            }
            index += width + 2;
        } else {
            if token == candidate {
                matched = true;
            }
            index += width;
        }
    }
    // An unterminated class is a literal `[`, which `Bun.Glob` never matches.
    None
}

fn invalid_settings(path: &Path, reason: &str) -> AppError {
    AppError::Catalog(CatalogError::InvalidSelectedSkill {
        path: path.to_path_buf(),
        reason: format!("OMP settings input cannot be interpreted: {reason}"),
    })
}

#[cfg(test)]
mod tests {
    use super::{
        MAX_SETTINGS_BYTES, SkillSettings, Syntax, Value, glob_matches, load, merge, parse,
    };
    use crate::error::ExitCategory;
    use crate::test_support::TestDir;
    use std::collections::BTreeMap;
    use std::fs;
    use std::path::Path;

    fn object(pairs: &[(&str, Value)]) -> Value {
        Value::Object(
            pairs
                .iter()
                .map(|(key, value)| ((*key).to_owned(), value.clone()))
                .collect::<BTreeMap<_, _>>(),
        )
    }

    fn write(path: &Path, contents: &str) {
        fs::create_dir_all(path.parent().expect("settings parent")).expect("settings directory");
        fs::write(path, contents).expect("settings file");
    }

    #[test]
    fn objects_merge_recursively_while_arrays_and_scalars_replace() {
        let mut base = object(&[(
            "skills",
            object(&[
                ("enabled", Value::Bool(true)),
                (
                    "customDirectories",
                    Value::Array(vec![Value::String("a".to_owned())]),
                ),
                ("enablePiUser", Value::Bool(true)),
            ]),
        )]);
        merge(
            &mut base,
            object(&[(
                "skills",
                object(&[
                    ("enabled", Value::Bool(false)),
                    (
                        "customDirectories",
                        Value::Array(vec![Value::String("b".to_owned())]),
                    ),
                ]),
            )]),
        );

        assert_eq!(base.bool_at("skills.enabled"), Some(false));
        assert_eq!(base.strings_at("skills.customDirectories"), ["b"]);
        assert_eq!(
            base.bool_at("skills.enablePiUser"),
            Some(true),
            "an absent overlay key must not clear a lower layer"
        );
    }

    #[test]
    fn an_empty_document_is_an_empty_mapping_and_a_non_mapping_root_is_rejected() {
        assert_eq!(
            parse("", Syntax::Yaml),
            Ok(Value::Object(BTreeMap::new())),
            "an empty YAML document is an empty mapping"
        );
        assert!(parse("[1, 2]", Syntax::Json).is_err());
        assert!(parse("- one\n- two\n", Syntax::Yaml).is_err());
    }

    #[test]
    fn layers_apply_in_omp_order_with_the_project_priority_inversion() {
        let fixture = TestDir::new("omp-settings-order");
        let agent_dir = fixture.0.join("agent");
        let project = fixture.0.join("project");
        write(
            &agent_dir.join("config.yml"),
            "skills:\n  enabled: true\n  ignoredSkills: [\"global-*\"]\n",
        );
        write(
            &project.join(".omp/settings.json"),
            "{\"skills\":{\"ignoredSkills\":[\"native-*\"]}}",
        );
        // Cursor is the lowest-priority settings provider, so it wins the conflicting key.
        write(
            &project.join(".cursor/settings.json"),
            "{\"skills\":{\"ignoredSkills\":[\"cursor-*\"]}}",
        );

        let settings = load(&agent_dir, &project).expect("layers load");
        assert_eq!(settings.ignored_skills, ["cursor-*"]);
        assert_eq!(settings.inputs.len(), 3);
    }

    #[test]
    fn the_first_existing_global_filename_wins() {
        let fixture = TestDir::new("omp-settings-global");
        let agent_dir = fixture.0.join("agent");
        write(&agent_dir.join("config.yml"), "skills:\n  enabled: false\n");
        write(&agent_dir.join("config.yaml"), "skills:\n  enabled: true\n");

        let settings = load(&agent_dir, &fixture.0.join("project")).expect("global loads");
        assert!(!settings.enabled);
        assert_eq!(settings.inputs.len(), 1);
        assert!(settings.inputs[0].ends_with("config.yml"));
    }

    #[test]
    fn an_empty_native_project_scope_contributes_nothing() {
        let fixture = TestDir::new("omp-settings-empty-scope");
        let project = fixture.0.join("project");
        fs::create_dir_all(project.join(".omp")).expect("empty project scope");

        let settings = load(&fixture.0.join("agent"), &project).expect("empty scope loads");
        assert!(settings.inputs.is_empty());
    }

    #[test]
    fn a_malformed_trusted_layer_is_a_data_error_and_a_third_party_layer_is_skipped() {
        let fixture = TestDir::new("omp-settings-malformed");
        let agent_dir = fixture.0.join("agent");
        let project = fixture.0.join("project");
        write(&agent_dir.join("config.yml"), "skills: {enabled: true}\n");
        write(&project.join(".cursor/settings.json"), "{not json");

        let settings = load(&agent_dir, &project).expect("a third-party layer only warns");
        assert!(settings.enabled);
        assert_eq!(settings.inputs.len(), 1);

        write(&agent_dir.join("config.yml"), "skills: {enabled\n");
        let error = load(&agent_dir, &project).expect_err("the global layer must fail closed");
        assert_eq!(error.category(), ExitCategory::Data);
    }

    #[test]
    fn a_codex_project_toml_can_disable_every_omp_skill() {
        let fixture = TestDir::new("omp-settings-toml");
        let project = fixture.0.join("project");
        write(
            &project.join(".codex/config.toml"),
            "[skills]\nenabled = false\n",
        );

        let settings = load(&fixture.0.join("agent"), &project).expect("TOML layer loads");
        assert_eq!(
            settings.inputs.len(),
            1,
            "the Codex project config must contribute a layer"
        );
        assert!(
            !settings.enabled,
            "a Codex project config really does gate OMP Skill discovery"
        );
    }

    #[test]
    fn an_oversized_or_non_regular_input_fails_closed() {
        let fixture = TestDir::new("omp-settings-bounds");
        let agent_dir = fixture.0.join("agent");
        fs::create_dir_all(&agent_dir).expect("agent dir");
        let oversized = "#".repeat(usize::try_from(MAX_SETTINGS_BYTES).expect("bound fits") + 1);
        fs::write(agent_dir.join("config.yml"), oversized).expect("oversized settings");

        let error = load(&agent_dir, &fixture.0.join("project"))
            .expect_err("an oversized input must be refused");
        assert_eq!(error.category(), ExitCategory::MissingInput);

        fs::remove_file(agent_dir.join("config.yml")).expect("remove oversized settings");
        fs::create_dir(agent_dir.join("config.yml")).expect("directory in place of settings");
        let error = load(&agent_dir, &fixture.0.join("project"))
            .expect_err("a directory is not a settings input");
        assert_eq!(error.category(), ExitCategory::MissingInput);
    }

    #[test]
    fn source_toggles_follow_omps_own_mapping_and_fallback() {
        let mut settings = SkillSettings::default();
        assert!(settings.source_enabled("native", true));
        assert!(settings.source_enabled("github", false));

        for key in [
            "enableClaudeProject",
            "enableClaudeUser",
            "enableCodexUser",
            "enablePiProject",
            "enablePiUser",
        ] {
            settings.toggles.insert(key.to_owned(), false);
        }
        assert!(
            !settings.source_enabled("github", false),
            "a provider without its own toggle follows the third-party fold"
        );
        assert!(
            settings.source_enabled("agents", true),
            "the agents toggles are deliberately outside that fold"
        );
        assert!(
            settings.source_enabled("omp-managed", false),
            "managed Skills have no toggle"
        );
    }

    #[test]
    fn disabled_providers_outranks_every_source_toggle() {
        // OMP drops a listed provider from the capability registry before any root is scanned
        // (`capability/index.ts:239,285-289`), so `native` being listed makes the mount
        // destination unreadable while `enablePiProject` still reads as true.
        let mut settings = SkillSettings {
            disabled_providers: vec!["native".to_owned()],
            ..SkillSettings::default()
        };
        assert!(settings.provider_disabled("native"));
        assert!(!settings.source_enabled("native", true));
        assert!(!settings.source_enabled("native", false));
        assert!(
            settings.source_enabled("claude", true),
            "an unlisted provider keeps its own toggle"
        );

        // `custom` is not a provider id, so the list can never reach a custom directory.
        settings.disabled_providers = vec!["custom".to_owned()];
        assert!(!settings.provider_disabled("custom"));
        assert!(settings.source_enabled("custom", false));
    }

    #[test]
    fn a_custom_directory_is_not_gated_by_a_source_toggle() {
        // `skills.customDirectories` is scanned outside the provider registry, so OMP never
        // applies `isSourceEnabled` to it (`extensibility/skills.ts:266-271`). Folding it into the
        // third-party fallback hid every custom Skill from `inspect` for the natural
        // "only my own curated directory" configuration.
        let mut settings = SkillSettings::default();
        for key in super::TOGGLE_KEYS {
            settings.toggles.insert((*key).to_owned(), false);
        }
        assert!(
            settings.source_enabled("custom", false),
            "a disabled provider set must not hide a custom directory"
        );
        assert!(!settings.source_enabled("github", false));
    }

    #[test]
    fn disabled_providers_is_read_from_the_merged_top_level_key() {
        let fixture = TestDir::new("omp-settings-disabled-providers");
        let agent_dir = fixture.0.join("agent");
        write(
            &agent_dir.join("config.yml"),
            "disabledProviders: [\"native\"]\nskills:\n  enabled: true\n",
        );

        let settings = load(&agent_dir, &fixture.0.join("project")).expect("global loads");
        assert_eq!(settings.disabled_providers, ["native"]);
        assert!(!settings.source_enabled("native", true));
    }

    #[test]
    fn filters_apply_in_order_and_match_names_rather_than_paths() {
        let mut settings = SkillSettings {
            disabled_skills: vec!["blocked".to_owned()],
            ignored_skills: vec!["git-*".to_owned()],
            include_skills: vec!["docker".to_owned(), "git-log".to_owned()],
            ..SkillSettings::default()
        };

        assert!(!settings.name_visible("blocked"));
        assert!(
            !settings.name_visible("git-log"),
            "an ignore pattern wins over an include pattern"
        );
        assert!(settings.name_visible("docker"));
        assert!(!settings.name_visible("other"));

        settings.include_skills.clear();
        assert!(
            settings.name_visible("other"),
            "an empty allow-list allows all"
        );
    }

    #[test]
    fn glob_matching_covers_the_shapes_omp_accepts() {
        for (pattern, name, expected) in [
            ("*", "anything", true),
            ("git-*", "git-log", true),
            ("git-*", "docker", false),
            ("*-log", "git-log", true),
            ("g?t", "git", true),
            ("g?t", "gaat", false),
            ("[gd]it", "git", true),
            ("[gd]it", "bit", false),
            ("[!g]it", "bit", true),
            ("[!g]it", "git", false),
            ("[a-c]ok", "bok", true),
            ("[a-c]ok", "zok", false),
            ("exact", "exact", true),
            ("exact", "exactly", false),
            ("a*b*c", "azzbzzc", true),
            ("a*b*c", "azzc", false),
        ] {
            assert_eq!(
                glob_matches(pattern, name),
                expected,
                "{pattern:?} against {name:?}"
            );
        }
    }

    /// Every row was measured against `Bun.Glob` 1.3.14, the runtime OMP 17.2.9 pins.
    ///
    /// The constructs below were previously unhandled. Direction matters: a pattern OMP matches but
    /// this release does not reports a hidden Skill as visible, which is the silent no-op mount
    /// `verify_selected_visibility` exists to prevent.
    #[test]
    fn glob_matching_reproduces_bun_glob_for_every_measured_construct() {
        for (pattern, name, expected) in [
            // Brace alternation, including nesting and empty alternatives.
            ("{git,docker}", "git", true),
            ("{git,docker}", "docker", true),
            ("{git,docker}", "npm", false),
            ("{git,docker}-*", "git-flow", true),
            ("a{b,c}d", "abd", true),
            ("a{b,c}d", "azd", false),
            ("a{b,c}", "a", false),
            ("{a,{b,c}}", "b", true),
            ("{a,{b,c}}", "a", true),
            ("{}", "", true),
            ("{,a}", "", true),
            ("{,a}", "a", true),
            ("{a,b}*", "ax", true),
            ("*-{x,y}", "n-x", true),
            ("*-{x,y}", "n-z", false),
            ("{a,b}}", "a}", true),
            // An unterminated or literal-looking group matches nothing, not even itself.
            ("{a,b", "{a,b", false),
            ("{a,b}", "{a,b}", false),
            ("{a}", "a", true),
            ("{a}", "{a}", false),
            // Leading `!` negates and stacks; it is literal anywhere else and escapable.
            ("!keep", "keep", false),
            ("!keep", "other", true),
            ("!keep-*", "keep-me", false),
            ("!keep-*", "drop-me", true),
            ("!", "x", true),
            ("!*", "anything", false),
            ("!!a", "a", true),
            ("!!a", "!a", false),
            ("a!b", "a!b", true),
            ("a!b", "axb", false),
            ("\\!a", "!a", true),
            ("\\!a", "a", false),
            // Escapes bind the next character, including inside a class; a trailing `\` matches
            // nothing.
            ("\\*", "*", true),
            ("\\*", "x", false),
            ("a\\*b", "a*b", true),
            ("a\\*b", "axb", false),
            ("\\{a\\}", "{a}", true),
            ("\\\\", "\\", true),
            ("a\\", "a", false),
            ("[a\\]b]", "]", true),
            // `^` negates a class exactly as `!` does.
            ("[^ab]", "c", true),
            ("[^ab]", "a", false),
            // A class may match a separator; `?` may not, and `*` does not cross one while `**`
            // does.
            ("[a/b]", "/", true),
            ("?", "/", false),
            ("a?b", "a/b", false),
            ("*", "a/b", false),
            ("**", "a/b", true),
            ("*", "", true),
            ("?", "", false),
            // A bare or empty class never matches.
            ("[", "[", false),
            ("[]", "[", false),
            ("}", "}", true),
            // Matching is case sensitive.
            ("{git,docker}", "GIT", false),
            ("[a-c]", "B", false),
        ] {
            assert_eq!(
                glob_matches(pattern, name),
                expected,
                "{pattern:?} against {name:?}"
            );
        }
    }
}
