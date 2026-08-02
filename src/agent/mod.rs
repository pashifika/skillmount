//! Agent adapter boundary and read-only discovery inspection.
//!
//! An adapter observes and describes; it never mutates. Every method here returns a snapshot or a
//! plan, and the shared application and transaction layers apply those values after the mutation
//! boundary.

pub mod claude;
pub mod codex;

#[cfg(test)]
mod tests;

use std::collections::BTreeMap;
use std::ffi::{OsStr, OsString};
use std::fs;
use std::path::{Path, PathBuf};

use crate::diagnostic::Diagnostic;
use crate::domain::{AgentId, RunContext, SkillCatalog, SkillNameKey};
use crate::error::AppError;
use crate::lock::LockResource;
use crate::mount::MountPlan;
use crate::mount::resolve::{PathKind, ResolvedEntry, classify};

/// Which namespace a discovery scope represents.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum ScopeKind {
    /// A Codex `.agents/skills` namespace, the authoritative discovery entry.
    CodexAuthoritative,
    /// A Codex `.codex/skills` namespace, a compatibility backing candidate only.
    CodexCompatibility,
    /// An ancestor `.agents/skills` between the launch CWD and the project root.
    CodexAncestor,
    /// The project's own `.claude/skills`, which `SkillMount` never modifies by default.
    ClaudeProject,
    /// The user-level `.claude/skills`.
    ClaudeUser,
    /// The isolated session root's `.claude/skills` that a launch would stage into.
    ClaudeStaging,
    /// A namespace made visible by a passthrough `--add-dir`.
    ClaudeAddDir,
}

impl ScopeKind {
    /// Returns the stable label used in read-only output.
    #[must_use]
    pub const fn label(self) -> &'static str {
        match self {
            Self::CodexAuthoritative => "codex authoritative",
            Self::CodexCompatibility => "codex compatibility",
            Self::CodexAncestor => "codex ancestor",
            Self::ClaudeProject => "claude project",
            Self::ClaudeUser => "claude user",
            Self::ClaudeStaging => "claude staging",
            Self::ClaudeAddDir => "claude add-dir",
        }
    }
}

/// One Skill-shaped entry an agent can already see in a discovery scope.
///
/// The name is kept as the raw platform value plus a comparison key rather than as a validated
/// `SkillName`. See `docs/adr/0010-discovery-entry-identity.md`: pre-existing entries are written
/// by users and other tools, so they routinely fail the portable-name grammar, and dropping them
/// would make conflict detection unsound.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ExistingSkill {
    /// Comparison key shared with source overlay resolution.
    pub comparison_key: SkillNameKey,
    /// Entry name exactly as stored on disk.
    pub raw_name: OsString,
    /// Visible entry path inside the scope.
    pub entry: PathBuf,
    /// Classification of the entry without implicitly following it.
    pub kind: PathKind,
    /// Canonical directory the entry ultimately refers to, when it resolves.
    pub source_canonical: Option<PathBuf>,
}

/// A namespace the agent will search, together with everything already visible in it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DiscoveryScope {
    /// Which namespace this is.
    pub kind: ScopeKind,
    /// Observed state of the namespace directory itself.
    pub state: ResolvedEntry,
    /// Other visible paths that reach the same terminal directory as this scope.
    ///
    /// Populated when duplicate scopes are folded away, so the same directory is not counted
    /// twice. The paths the operator can actually see are kept so output still explains how the
    /// store was reached.
    pub aliases: Vec<PathBuf>,
    /// Visible Skills keyed by comparison key, ordered deterministically.
    pub existing_skills: BTreeMap<SkillNameKey, ExistingSkill>,
    /// Non-fatal observations about this scope.
    pub warnings: Vec<Diagnostic>,
}

impl DiscoveryScope {
    /// Returns the entry occupying `key`, if the agent can already see one.
    #[must_use]
    pub fn occupant(&self, key: &SkillNameKey) -> Option<&ExistingSkill> {
        self.existing_skills.get(key)
    }
}

/// Everything one adapter observed, without mutating anything.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DiscoverySnapshot {
    /// Adapter that produced the snapshot.
    pub agent: AgentId,
    /// Every scope the child will search, in adapter-defined preflight order.
    pub scopes: Vec<DiscoveryScope>,
    /// Authoritative discovery entry the child reads.
    pub discovery_entry: PathBuf,
    /// Store that selected Skills would be mounted into.
    pub backing_store: PathBuf,
    /// Observed state of the backing store.
    pub backing_store_state: PathKind,
    /// Resources a later transaction would lock, in deterministic acquisition order.
    pub lock_resources: Vec<LockResource>,
    /// Non-fatal observations.
    pub warnings: Vec<Diagnostic>,
}

impl DiscoverySnapshot {
    /// Returns the scope that planned mounts would be written into.
    #[must_use]
    pub fn mount_scope(&self) -> Option<&DiscoveryScope> {
        self.scopes
            .iter()
            .find(|scope| scope.state.entry == self.backing_store)
    }
}

/// A read-only agent adapter.
///
/// Command preparation is intentionally absent from this trait: it would consume an applied plan
/// and mutate a `Command`, while this boundary is restricted to read-only observation and
/// description. Keeping it out also makes the currently reserved child-launch boundary explicit.
pub trait AgentAdapter {
    /// Returns the adapter's agent.
    fn id(&self) -> AgentId;

    /// Returns the executable name looked up through `PATH` when none was supplied.
    fn default_executable(&self) -> &'static OsStr;

    /// Checks passthrough arguments for combinations that would defeat Skill loading.
    ///
    /// # Errors
    ///
    /// Returns [`AppError::Usage`] when an argument is incompatible with mounting Skills.
    fn validate_passthrough_args(&self, args: &[OsString]) -> Result<Vec<Diagnostic>, AppError>;

    /// Inspects every scope the child will search, without modifying any of them.
    ///
    /// # Errors
    ///
    /// Returns an error when a scope cannot be read, or when the agent's own discovery layout is
    /// ambiguous enough that no safe destination exists.
    fn inspect_discovery(&self, context: &RunContext) -> Result<DiscoverySnapshot, AppError>;

    /// Builds the complete deterministic plan for a validated catalog.
    ///
    /// The snapshot is a parameter rather than an internal step so the transaction change can
    /// re-inspect under lock and rebuild through this same pure function.
    ///
    /// # Errors
    ///
    /// Returns [`AppError::Plan`] when any selected Skill conflicts with observed state.
    fn build_mount_plan(
        &self,
        context: &RunContext,
        catalog: &SkillCatalog,
        discovery: &DiscoverySnapshot,
    ) -> Result<MountPlan, AppError>;
}

/// Inspects one discovery namespace without modifying it.
///
/// A namespace that is missing or unresolvable yields no visible Skills. Whether that state is
/// fatal is the adapter's decision: an unusable ancestor scope and an unusable authoritative entry
/// have different consequences in the V2 state table.
///
/// # Errors
///
/// Returns [`AppError::MissingInput`] when the namespace or one of its entries cannot be read for
/// a reason other than being absent.
pub fn inspect_scope(kind: ScopeKind, entry: &Path) -> Result<DiscoveryScope, AppError> {
    let state = classify(entry)?;
    let mut scope = DiscoveryScope {
        kind,
        state,
        aliases: Vec::new(),
        existing_skills: BTreeMap::new(),
        warnings: Vec::new(),
    };
    if !matches!(
        scope.state.kind,
        PathKind::Directory | PathKind::DirectoryLink
    ) {
        return Ok(scope);
    }

    let entries = fs::read_dir(entry).map_err(|error| AppError::MissingInput {
        path: entry.to_path_buf(),
        reason: error.to_string(),
    })?;
    for child in entries {
        let child = child.map_err(|error| AppError::MissingInput {
            path: entry.to_path_buf(),
            reason: error.to_string(),
        })?;
        let raw_name = child.file_name();
        let child_state = classify(&child.path())?;
        let existing = ExistingSkill {
            comparison_key: SkillNameKey::new(&raw_name),
            raw_name,
            entry: child.path(),
            kind: child_state.kind,
            source_canonical: child_state.terminal,
        };
        insert_deterministically(&mut scope, existing);
    }

    Ok(scope)
}

/// Folds scopes that reach the same terminal directory into one, keeping every visible path.
///
/// Two scopes can name one directory: the expected Codex layout has `.agents/skills` linking to
/// `.codex/skills`, and both are inspected. Leaving both in the list would make the store's own
/// contents look like a foreign Skill visible in another scope, which turns every ordinary mount
/// into a spurious reuse and suppresses real conflicts. `preferred` wins when it takes part in a
/// collision, so the scope the plan writes through is the one that survives.
///
/// Scopes without a terminal directory are never folded: a missing or unresolvable namespace has
/// no identity to compare, and merging on its absence would silently hide it.
pub(crate) fn dedupe_scopes_by_terminal(
    scopes: Vec<DiscoveryScope>,
    preferred: &Path,
) -> Vec<DiscoveryScope> {
    let mut kept: Vec<DiscoveryScope> = Vec::new();
    for scope in scopes {
        let Some(terminal) = scope.state.terminal.clone() else {
            kept.push(scope);
            continue;
        };
        let Some(existing) = kept
            .iter_mut()
            .find(|candidate| candidate.state.terminal.as_deref() == Some(terminal.as_path()))
        else {
            kept.push(scope);
            continue;
        };

        if scope.state.entry == preferred && existing.state.entry != preferred {
            let displaced = std::mem::replace(existing, scope);
            existing.aliases.push(displaced.state.entry);
            existing.aliases.extend(displaced.aliases);
            existing.warnings.extend(displaced.warnings);
        } else {
            existing.aliases.push(scope.state.entry);
            existing.aliases.extend(scope.aliases);
            existing.warnings.extend(scope.warnings);
        }
    }
    kept
}

/// Keeps one representative per comparison key so host enumeration order cannot change results.
///
/// Two entries fold to one key on a case-sensitive filesystem. The smaller raw name always wins,
/// and the collision is reported rather than silently dropped, because the agent's own duplicate
/// precedence is undocumented and must not be relied on.
fn insert_deterministically(scope: &mut DiscoveryScope, existing: ExistingSkill) {
    let Some(previous) = scope.existing_skills.get(&existing.comparison_key) else {
        scope
            .existing_skills
            .insert(existing.comparison_key.clone(), existing);
        return;
    };

    let (kept, displaced) = if existing.raw_name < previous.raw_name {
        (existing, previous.clone())
    } else {
        (previous.clone(), existing)
    };
    scope.warnings.push(Diagnostic::warning(
        format!(
            "{} scope contains two entries with the logical name {}; {} is reported and {} is ignored",
            scope.kind.label(),
            kept.comparison_key,
            kept.entry.display(),
            displaced.entry.display()
        ),
        displaced.entry.clone(),
    ));
    scope
        .existing_skills
        .insert(kept.comparison_key.clone(), kept);
}
