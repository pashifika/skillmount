//! Agent adapter boundary, read-only discovery inspection, and advisory version observation.
//!
//! An adapter observes and describes; it never mutates. Every adapter method returns a snapshot or
//! plan, and the shared application and transaction layers apply those values after the mutation
//! boundary. The sibling `version` module may launch one bounded, shell-free `--version` process;
//! it has no access to locks, journals, transactions, mount mutation, or cleanup identity.

pub mod claude;
pub mod codex;
pub mod omp;
pub(crate) mod version;

#[cfg(test)]
mod tests;

use std::collections::BTreeMap;
use std::ffi::OsString;
use std::fs;
use std::path::{Path, PathBuf};

use crate::diagnostic::{Diagnostic, DiagnosticKind};
use crate::domain::{AgentId, CatalogPolicy, RunContext, SkillCatalog, SkillNameKey};
use crate::error::AppError;
use crate::lock::LockResource;
use crate::mount::MountPlan;
use crate::mount::resolve::{PathKind, ResolvedEntry, classify};

/// Returns the single registered adapter for one supported Agent.
///
/// The reference is `'static` and allocation-free: every adapter is a stateless zero-sized value
/// and the supported set is closed at compile time, so no `Box` is needed. Dynamic dispatch is
/// confined to a handful of orchestration checkpoints whose cost is dominated by filesystem and
/// process work. This is the one registration point: adding a compile-time Agent must not require
/// an Agent-specific policy branch in any shared caller.
pub(crate) fn adapter(agent: AgentId) -> &'static dyn AgentAdapter {
    match agent {
        AgentId::Codex => &codex::CodexAdapter,
        AgentId::Claude => &claude::ClaudeAdapter,
        AgentId::Omp => &omp::OmpAdapter,
    }
}

/// Which namespace a discovery scope represents.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum ScopeKind {
    /// The project's preferred Codex `.agents/skills` namespace.
    CodexProjectAgents,
    /// The project's legacy Codex `.codex/skills` namespace.
    CodexProjectLegacy,
    /// An ancestor `.agents/skills` between the launch CWD and the project root.
    CodexAncestorAgents,
    /// An ancestor `.codex/skills` retained by Codex for compatibility.
    CodexAncestorLegacy,
    /// The user's cross-agent `$HOME/.agents/skills` namespace.
    CodexUserAgents,
    /// The deprecated user `$CODEX_HOME/skills` namespace.
    CodexUserLegacy,
    /// Bundled Skills under `$CODEX_HOME/skills/.system`.
    CodexSystem,
    /// Host-wide administrator Skills, such as `/etc/codex/skills` on Unix.
    CodexAdmin,
    /// The project's own `.claude/skills`, which `SkillMount` never modifies by default.
    ClaudeProject,
    /// An unqualified `.claude/skills` between the launch CWD and the project root.
    ClaudeAncestor,
    /// The host-wide enterprise Claude Code Skill namespace.
    ClaudeManaged,
    /// The user-level `.claude/skills`.
    ClaudeUser,
    /// The isolated session root's `.claude/skills` that a launch would stage into.
    ClaudeStaging,
    /// A namespace made visible by a passthrough `--add-dir`.
    ClaudeAddDir,
    /// The launch CWD's own `.omp/skills`, which an OMP session mounts into.
    OmpProject,
    /// An ancestor `.omp/skills` between the launch CWD and the OMP walk boundary.
    OmpAncestor,
    /// The active OMP agent directory's `skills/` root.
    OmpUser,
    /// A `skills/` root beside an enabled OMP extension package.
    OmpPlugin,
    /// A configured `skills.customDirectories` entry.
    OmpCustom,
    /// A compatibility provider root OMP reads for another Agent's layout.
    OmpCompatibility,
    /// Auto-learned Skills under the OMP agent directory's `managed-skills`.
    OmpManaged,
}

impl ScopeKind {
    /// Returns the stable label used in read-only output.
    #[must_use]
    pub const fn label(self) -> &'static str {
        match self {
            Self::CodexProjectAgents => "codex project .agents",
            Self::CodexProjectLegacy => "codex project .codex",
            Self::CodexAncestorAgents => "codex ancestor .agents",
            Self::CodexAncestorLegacy => "codex ancestor .codex",
            Self::CodexUserAgents => "codex user agents",
            Self::CodexUserLegacy => "codex user legacy",
            Self::CodexSystem => "codex system",
            Self::CodexAdmin => "codex admin",
            Self::ClaudeProject => "claude project",
            Self::ClaudeAncestor => "claude ancestor",
            Self::ClaudeManaged => "claude managed",
            Self::ClaudeUser => "claude user",
            Self::ClaudeStaging => "claude staging",
            Self::ClaudeAddDir => "claude add-dir",
            Self::OmpProject => "omp project",
            Self::OmpAncestor => "omp ancestor",
            Self::OmpUser => "omp user",
            Self::OmpPlugin => "omp plugin",
            Self::OmpCustom => "omp custom",
            Self::OmpCompatibility => "omp compatibility",
            Self::OmpManaged => "omp managed",
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
    /// Classification visible to the child, or anticipated at launch for a pinned embedded Skill.
    pub kind: PathKind,
    /// Canonical directory the entry ultimately refers to, when it resolves.
    pub source_canonical: Option<PathBuf>,
}

/// One visible Skill together with the scope that contributes it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct VisibleSkill {
    /// Scope in which Codex or Claude discovers the Skill.
    pub scope: ScopeKind,
    /// Existing Skill evidence retained from that scope.
    pub skill: ExistingSkill,
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
    /// Canonical directories reached while inspecting this scope.
    ///
    /// Recursive adapters retain every terminal, not only the root, so an alias into a shared
    /// collection contributes the physical lock that serializes another `SkillMount` session.
    pub observed_directories: Vec<PathBuf>,
    /// Visible Skills keyed by comparison key, ordered deterministically.
    pub existing_skills: BTreeMap<SkillNameKey, Vec<ExistingSkill>>,
    /// Immediate namespace entries keyed by their filesystem name.
    ///
    /// For direct-entry discovery models this mirrors `existing_skills`. Codex keeps it separate:
    /// recursive frontmatter names describe what the child can select, while these entries decide
    /// whether a mount destination path is physically free.
    pub direct_entries: BTreeMap<SkillNameKey, Vec<ExistingSkill>>,
    /// Non-fatal observations about this scope.
    pub warnings: Vec<Diagnostic>,
}

impl DiscoveryScope {
    /// Returns the entry occupying `key`, if the agent can already see one.
    #[must_use]
    pub fn occupant(&self, key: &SkillNameKey) -> Option<&ExistingSkill> {
        self.existing_skills
            .get(key)
            .and_then(|occupants| occupants.first())
    }

    /// Returns every visible Skill declaring `key`, in deterministic path order.
    #[must_use]
    pub fn occupants(&self, key: &SkillNameKey) -> &[ExistingSkill] {
        self.existing_skills.get(key).map_or(&[], Vec::as_slice)
    }

    /// Returns the immediate entry occupying a destination comparison key, if any.
    #[must_use]
    pub fn direct_occupant(&self, key: &SkillNameKey) -> Option<&ExistingSkill> {
        self.direct_entries
            .get(key)
            .and_then(|occupants| occupants.first())
    }

    /// Returns every immediate entry occupying `key`, including case variants.
    #[must_use]
    pub fn direct_occupants(&self, key: &SkillNameKey) -> &[ExistingSkill] {
        self.direct_entries.get(key).map_or(&[], Vec::as_slice)
    }
}

/// Everything one adapter observed, without mutating anything.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DiscoverySnapshot {
    /// Adapter that produced the snapshot.
    pub agent: AgentId,
    /// Every scope in the adapter's current discovery model, in preflight order.
    pub scopes: Vec<DiscoveryScope>,
    /// All visible Skills merged by logical name while retaining every duplicate.
    pub visible_skills: BTreeMap<SkillNameKey, Vec<VisibleSkill>>,
    /// Immediate entries in the one namespace selected mounts are written through.
    pub mount_entries: BTreeMap<SkillNameKey, Vec<ExistingSkill>>,
    /// Logical discovery entry the child reads.
    pub discovery_entry: PathBuf,
    /// Store that selected Skills would be mounted into.
    pub backing_store: PathBuf,
    /// Observed state of the backing store.
    pub backing_store_state: PathKind,
    /// Canonical directory the backing store resolves to, when it resolves through a link.
    ///
    /// `None` when the store is the canonical directory itself or does not exist yet. Diagnostics
    /// must show this whenever it is present: a mount planned through a directory link is applied to
    /// a directory the logical path does not name, and an operator who cannot see that path cannot
    /// tell which project the mount became visible to.
    pub backing_store_canonical: Option<PathBuf>,
    /// Resources the transaction locks; acquisition derives a deduplicated, sorted key set.
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

/// Returns the canonical directory `store` resolves to, but only when that differs from `store`.
///
/// A store that is already the canonical directory needs no second path in a diagnostic, and a
/// missing store has no identity to report yet. Every adapter uses this so the reported shape is the
/// same for all three Agents.
#[must_use]
pub(crate) fn canonical_backing(store: &Path, state: &ResolvedEntry) -> Option<PathBuf> {
    state
        .terminal
        .as_ref()
        .filter(|terminal| terminal.as_path() != store)
        .cloned()
}

/// A read-only agent adapter.
///
/// Command mutation is intentionally absent from this trait. An adapter describes a
/// [`crate::mount::LaunchPlan`], while the shared application and process layers create and own the
/// child after the transaction is active. An adapter never opens a journal, acquires a lock,
/// applies or removes a link, spawns a child, or selects error precedence; the application decides
/// when each method below runs.
pub(crate) trait AgentAdapter {
    /// Returns the dated compatibility evidence this adapter was last tested against.
    ///
    /// Stable identity is not restated here: [`AgentId::descriptor`] is the single metadata source,
    /// and version evidence is deliberately separate from it because it is evidence, not identity.
    fn version_spec(&self) -> version::VersionSpec;

    /// Returns the Agent-required catalog facts for one selected Skill.
    fn catalog_policy(&self) -> CatalogPolicy;

    /// Returns every future destination store, used only for source/destination cycle rejection.
    fn destination_stores(&self, context: &RunContext) -> Vec<PathBuf>;

    /// Checks passthrough arguments for combinations that would defeat Skill loading.
    ///
    /// # Errors
    ///
    /// Returns [`AppError::Usage`] when an argument is incompatible with mounting Skills.
    fn validate_passthrough_args(&self, args: &[OsString]) -> Result<Vec<Diagnostic>, AppError>;

    /// Re-checks release-independent hazards that can invalidate the inspected launch contract.
    ///
    /// This is read-only and repeatable. The application calls it before `SkillMount` state access
    /// and again after the lock set stabilizes.
    ///
    /// # Errors
    ///
    /// Returns an error when configuration outside this release's supported contract is present.
    fn validate_launch_invariants(&self, context: &RunContext) -> Result<(), AppError>;

    /// Returns agent-specific observations that depend on the selected catalog.
    fn catalog_diagnostics(
        &self,
        _context: &RunContext,
        _catalog: &SkillCatalog,
        _plan: &MountPlan,
    ) -> Vec<Diagnostic> {
        Vec::new()
    }

    /// Inspects every scope in the adapter's current discovery model without modifying any of them.
    ///
    /// # Errors
    ///
    /// Returns an error when a scope cannot be read, or when the agent's own discovery layout is
    /// ambiguous enough that no safe destination exists.
    fn inspect_discovery(&self, context: &RunContext) -> Result<DiscoverySnapshot, AppError>;

    /// Builds the complete deterministic plan for a validated catalog.
    ///
    /// The snapshot is a parameter rather than an internal step so the application can re-inspect
    /// under lock and rebuild through this same pure function.
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

    /// Revalidates the launch contract after apply and immediately before the child is spawned.
    ///
    /// The snapshot and plan are the locked pre-apply values, so an adapter can ignore exactly the
    /// transaction-owned actions it just asked for. Version evidence is deliberately not observed
    /// again here: an Agent update during apply is not launch authorization.
    ///
    /// # Errors
    ///
    /// Returns an error when a hard invariant changed after the plan was applied.
    fn validate_spawn_boundary(
        &self,
        context: &RunContext,
        _catalog: &SkillCatalog,
        _discovery: &DiscoverySnapshot,
        _plan: &MountPlan,
    ) -> Result<(), AppError> {
        self.validate_launch_invariants(context)
    }
}

/// Inspects one discovery namespace without modifying it.
///
/// A namespace that is missing or unresolvable yields no visible Skills. Whether that state is
/// fatal is the adapter's decision: an unusable ancestor scope and an unusable preferred entry have
/// different consequences in the discovery model.
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
        insert_deterministically(&mut scope, existing, DiagnosticKind::General);
    }

    scope.direct_entries.clone_from(&scope.existing_skills);

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
        let Some(existing) = kept.iter_mut().find(|candidate| {
            candidate.state.terminal.as_deref() == Some(terminal.as_path())
                && compatible_traversal(candidate.kind, scope.kind)
        }) else {
            kept.push(scope);
            continue;
        };

        if scope.state.entry == preferred && existing.state.entry != preferred {
            let displaced = std::mem::replace(existing, scope);
            existing.aliases.push(displaced.state.entry);
            existing.aliases.extend(displaced.aliases);
            existing
                .observed_directories
                .extend(displaced.observed_directories);
            existing.warnings.extend(displaced.warnings);
        } else {
            existing.aliases.push(scope.state.entry);
            existing.aliases.extend(scope.aliases);
            existing
                .observed_directories
                .extend(scope.observed_directories);
            existing.warnings.extend(scope.warnings);
        }
        existing.observed_directories.sort();
        existing.observed_directories.dedup();
    }
    kept
}

/// Returns whether two roots are guaranteed to produce the same recursive inventory.
///
/// Bundled-system discovery deliberately refuses directory links, while every other supported
/// Codex root follows them. Claude's managed scope follows links like the other Claude scopes but
/// carries higher-precedence conflict semantics that cannot be discarded during a fold. Those two
/// scope kinds therefore only fold with another scope of the same semantic class.
const fn compatible_traversal(left: ScopeKind, right: ScopeKind) -> bool {
    matches!(left, ScopeKind::CodexSystem) == matches!(right, ScopeKind::CodexSystem)
        && matches!(left, ScopeKind::ClaudeManaged) == matches!(right, ScopeKind::ClaudeManaged)
}

/// Builds the merged visible-name index and the separate destination-occupancy map.
pub(crate) fn discovery_indexes(
    scopes: &[DiscoveryScope],
    backing_store: &Path,
) -> (
    BTreeMap<SkillNameKey, Vec<VisibleSkill>>,
    BTreeMap<SkillNameKey, Vec<ExistingSkill>>,
) {
    let mut visible: BTreeMap<SkillNameKey, Vec<VisibleSkill>> = BTreeMap::new();
    for scope in scopes {
        for (key, skills) in &scope.existing_skills {
            let entries = visible.entry(key.clone()).or_default();
            entries.extend(skills.iter().cloned().map(|skill| VisibleSkill {
                scope: scope.kind,
                skill,
            }));
        }
    }
    for entries in visible.values_mut() {
        entries.sort_by(|left, right| {
            left.scope
                .cmp(&right.scope)
                .then_with(|| left.skill.entry.cmp(&right.skill.entry))
        });
    }

    let mount_entries = scopes
        .iter()
        .find(|scope| scope.state.entry == backing_store)
        .map_or_else(BTreeMap::new, |scope| scope.direct_entries.clone());
    (visible, mount_entries)
}

/// Retains every representative per comparison key in deterministic order.
///
/// Multiple entries can fold to one key on a case-sensitive filesystem or declare one frontmatter
/// name under Codex. None may be dropped because a foreign source must outrank an otherwise
/// reusable link during conflict evaluation.
fn insert_deterministically(
    scope: &mut DiscoveryScope,
    existing: ExistingSkill,
    diagnostic_kind: DiagnosticKind,
) {
    let occupants = scope
        .existing_skills
        .entry(existing.comparison_key.clone())
        .or_default();
    if let Some(previous) = occupants.first() {
        scope.warnings.push(Diagnostic::warning_with_kind(
            diagnostic_kind,
            format!(
                "{} scope contains multiple entries with the logical name {}; both {} and {} remain visible",
                scope.kind.label(),
                existing.comparison_key,
                previous.entry.display(),
                existing.entry.display()
            ),
            existing.entry.clone(),
        ));
    }
    occupants.push(existing);
    occupants.sort_by(|left, right| {
        left.raw_name
            .cmp(&right.raw_name)
            .then_with(|| left.entry.cmp(&right.entry))
    });
}

/// Retains one immediate destination occupant without treating it as a logical Skill.
pub(crate) fn insert_direct_deterministically(scope: &mut DiscoveryScope, existing: ExistingSkill) {
    let occupants = scope
        .direct_entries
        .entry(existing.comparison_key.clone())
        .or_default();
    occupants.push(existing);
    occupants.sort_by(|left, right| {
        left.raw_name
            .cmp(&right.raw_name)
            .then_with(|| left.entry.cmp(&right.entry))
    });
}
