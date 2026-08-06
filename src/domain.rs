//! Application and catalog domain types.

use std::error::Error;
use std::ffi::{OsStr, OsString};
use std::fmt;
use std::path::{Path, PathBuf};

use crate::diagnostic::Diagnostic;
use crate::error::AppError;

/// A supported agent adapter.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AgentId {
    /// Codex CLI from `OpenAI`.
    Codex,
    /// Anthropic Claude Code CLI.
    Claude,
}

/// Stable declarative metadata for one supported Agent.
///
/// This is the single source of truth for the persistent journal label, the operator display
/// name, the default executable basename, mount-mode support, and the project-relative discovery
/// layout an operator command inspects. No other module may restate those literals, so adding a
/// compile-time Agent does not require an Agent-specific branch in a shared caller.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct AgentDescriptor {
    id: AgentId,
    label: &'static str,
    display_name: &'static str,
    executable: &'static str,
    default_mount_mode: MountMode,
    explicit_mount_modes: &'static [MountMode],
    project_layout_paths: &'static [&'static str],
}

static CODEX_DESCRIPTOR: AgentDescriptor = AgentDescriptor {
    id: AgentId::Codex,
    label: "codex",
    display_name: "Codex",
    executable: "codex",
    default_mount_mode: MountMode::Project,
    explicit_mount_modes: &[MountMode::Project],
    project_layout_paths: &[".agents/skills", ".codex/skills"],
};

static CLAUDE_DESCRIPTOR: AgentDescriptor = AgentDescriptor {
    id: AgentId::Claude,
    label: "claude",
    display_name: "Claude",
    executable: "claude",
    default_mount_mode: MountMode::Staging,
    explicit_mount_modes: &[MountMode::Project, MountMode::Staging],
    project_layout_paths: &[".claude/skills"],
};

impl AgentDescriptor {
    /// Returns the closed identity this descriptor describes.
    #[must_use]
    pub const fn id(&self) -> AgentId {
        self.id
    }

    /// Returns the stable label used in output and in the transaction journal.
    #[must_use]
    pub const fn label(&self) -> &'static str {
        self.label
    }

    /// Returns the operator-facing display name.
    #[must_use]
    pub const fn display_name(&self) -> &'static str {
        self.display_name
    }

    /// Returns the executable name searched through `PATH` when none was supplied.
    #[must_use]
    pub fn executable_name(&self) -> &'static OsStr {
        OsStr::new(self.executable)
    }

    /// Returns the mount location selected when the operator did not name one.
    #[must_use]
    pub const fn default_mount_mode(&self) -> MountMode {
        self.default_mount_mode
    }

    /// Returns whether the operator may name this mount location explicitly.
    #[must_use]
    pub fn supports_explicit_mount_mode(&self, mode: MountMode) -> bool {
        self.explicit_mount_modes.contains(&mode)
    }

    /// Returns the project-relative discovery layouts an operator command inspects.
    #[must_use]
    pub const fn project_layout_paths(&self) -> &'static [&'static str] {
        self.project_layout_paths
    }
}

impl AgentId {
    /// Every supported Agent, in the one deterministic order shared by inspection and diagnosis.
    pub const ALL: &'static [Self] = &[Self::Codex, Self::Claude];

    /// Returns the stable declarative metadata for this Agent.
    #[must_use]
    pub const fn descriptor(self) -> &'static AgentDescriptor {
        match self {
            Self::Codex => &CODEX_DESCRIPTOR,
            Self::Claude => &CLAUDE_DESCRIPTOR,
        }
    }

    /// Returns the executable name searched through `PATH` by the later process layer.
    #[must_use]
    pub fn executable_name(self) -> &'static OsStr {
        self.descriptor().executable_name()
    }

    /// Returns the stable label used in output and in the transaction journal.
    #[must_use]
    pub const fn label(self) -> &'static str {
        self.descriptor().label()
    }

    /// Parses a label a journal recorded earlier.
    #[must_use]
    pub fn parse(label: &str) -> Option<Self> {
        Self::ALL
            .iter()
            .copied()
            .find(|agent| agent.label() == label)
    }
}

/// The requested link implementation.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LinkMode {
    /// Select the platform default.
    Auto,
    /// Use a directory symbolic link.
    Symlink,
    /// Use a Windows junction.
    Junction,
}

/// The selected mount location strategy.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MountMode {
    /// Mount into the project discovery namespace.
    Project,
    /// Mount into an isolated staging namespace.
    Staging,
}

/// Existing-destination handling policy.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ConflictPolicy {
    /// Reject an existing destination.
    Error,
    /// Preserve and skip an existing destination.
    Skip,
}

/// Metadata validation policy.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ValidationLevel {
    /// Apply adapter-required metadata checks.
    Basic,
    /// Apply portable cross-agent metadata checks.
    Strict,
    /// Skip metadata checks while retaining structural safety checks.
    None,
}

/// Validated wrapper options for one agent session.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RunOptions {
    /// Link implementation.
    pub link_mode: LinkMode,
    /// Mount location strategy.
    pub mount_mode: MountMode,
    /// Existing-destination handling policy.
    pub conflict: ConflictPolicy,
    /// Metadata validation policy.
    pub validation: ValidationLevel,
    /// Whether planning should remain read-only.
    pub dry_run: bool,
    /// Whether cleanup should retain transaction-owned mounts.
    pub keep_mounts: bool,
    /// Whether transaction recovery is disabled.
    pub no_recover: bool,
    /// Requested diagnostic verbosity.
    pub verbosity: u8,
}

/// One ordered `--skills-dir` occurrence before structural discovery.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SourceOccurrence {
    /// Zero-based command-line ordinal.
    pub ordinal: usize,
    /// Original platform-native path value.
    pub input_path: PathBuf,
    /// Invocation-relative absolute path.
    pub resolved_path: PathBuf,
}

/// One accessible and canonicalized source occurrence.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SkillSource {
    /// Zero-based command-line ordinal.
    pub ordinal: usize,
    /// Original platform-native path value.
    pub input_path: PathBuf,
    /// Invocation-relative absolute path.
    pub resolved_path: PathBuf,
    /// Canonical input directory.
    pub canonical_path: PathBuf,
}

/// Resolved configuration roots for the Codex CLI.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CodexAgent {
    /// Explicit resolved executable path, or the bare executable name for `PATH` lookup.
    pub(crate) executable: PathBuf,
    /// User home used by Codex's user-wide `.agents/skills` root.
    pub(crate) user_home: PathBuf,
    /// Codex configuration home, including deprecated and bundled Skill roots.
    pub(crate) home: PathBuf,
    /// Canonical `CODEX_HOME` value that must be propagated to a Codex child.
    ///
    /// `None` means Codex ignored an absent, empty, or non-Unicode override and derived its
    /// default from the user home instead.
    pub(crate) home_override: Option<PathBuf>,
    /// Host-wide Codex Skill root when the platform exposes one.
    pub(crate) admin_skills: Option<PathBuf>,
}

/// Resolved configuration roots for the Claude Code CLI.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ClaudeAgent {
    /// Explicit resolved executable path, or the bare executable name for `PATH` lookup.
    pub(crate) executable: PathBuf,
    /// Claude Code user configuration directory, after applying `CLAUDE_CONFIG_DIR`.
    pub(crate) config_dir: PathBuf,
    /// Host-wide enterprise Claude Code Skill root.
    pub(crate) managed_skills: PathBuf,
}

/// Resolved configuration for the one selected Agent.
///
/// A session resolves only the Agent it will launch. Two Agents' roots therefore cannot coexist in
/// one context, and configuration belonging solely to an Agent that was not selected can neither
/// change nor fail the selected command.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ResolvedAgent {
    /// Codex CLI was selected.
    Codex(CodexAgent),
    /// Claude Code CLI was selected.
    Claude(ClaudeAgent),
}

impl ResolvedAgent {
    /// Returns the selected Agent's closed identity.
    #[must_use]
    pub const fn id(&self) -> AgentId {
        match self {
            Self::Codex(_) => AgentId::Codex,
            Self::Claude(_) => AgentId::Claude,
        }
    }

    /// Returns the selected Agent's stable declarative metadata.
    #[must_use]
    pub const fn descriptor(&self) -> &'static AgentDescriptor {
        self.id().descriptor()
    }

    /// Returns the executable the later process layer launches.
    #[must_use]
    pub fn executable(&self) -> &Path {
        match self {
            Self::Codex(codex) => &codex.executable,
            Self::Claude(claude) => &claude.executable,
        }
    }

    /// Returns the Codex roots.
    ///
    /// # Errors
    ///
    /// Returns [`AppError::Internal`] when a concrete adapter was called with another Agent's
    /// resolved context. Normal parsing and registry lookup make that mismatch unconstructable.
    pub(crate) fn codex(&self) -> Result<&CodexAgent, AppError> {
        if let Self::Codex(codex) = self {
            return Ok(codex);
        }
        Err(mismatched_agent(AgentId::Codex, self.id()))
    }

    /// Returns the Claude roots.
    ///
    /// # Errors
    ///
    /// Returns [`AppError::Internal`] when a concrete adapter was called with another Agent's
    /// resolved context. Normal parsing and registry lookup make that mismatch unconstructable.
    pub(crate) fn claude(&self) -> Result<&ClaudeAgent, AppError> {
        if let Self::Claude(claude) = self {
            return Ok(claude);
        }
        Err(mismatched_agent(AgentId::Claude, self.id()))
    }

    /// Returns the Codex roots for in-place fixture adjustment.
    ///
    /// Test-only: production code never mutates resolved roots after `paths` builds them.
    ///
    /// # Panics
    ///
    /// Panics when the fixture selected another Agent.
    #[cfg(test)]
    pub(crate) fn codex_mut(&mut self) -> &mut CodexAgent {
        let expected = mismatched_agent(AgentId::Codex, self.id());
        if let Self::Codex(codex) = self {
            return codex;
        }
        panic!("{expected}")
    }

    /// Returns the Claude roots for in-place fixture adjustment.
    ///
    /// Test-only: production code never mutates resolved roots after `paths` builds them.
    ///
    /// # Panics
    ///
    /// Panics when the fixture selected another Agent.
    #[cfg(test)]
    pub(crate) fn claude_mut(&mut self) -> &mut ClaudeAgent {
        let expected = mismatched_agent(AgentId::Claude, self.id());
        if let Self::Claude(claude) = self {
            return claude;
        }
        panic!("{expected}")
    }
}

fn mismatched_agent(expected: AgentId, actual: AgentId) -> AppError {
    AppError::Internal(format!(
        "the {} adapter was called with a resolved {} context",
        expected.label(),
        actual.label()
    ))
}

/// Declarative catalog requirements contributed by the selected Agent.
///
/// A policy may only strengthen an Agent's compatibility requirements. It cannot disable the
/// structural, canonicalization, destination-cycle, portable-name, selection-order, or no-fallback
/// checks the catalog owns unconditionally.
// Each flag is an independent Agent-required fact drawn from one pinned Agent's loader, so this is
// a declarative table rather than hidden state. A state machine would invent combinations no Agent
// has, and five two-variant enums would add five type declarations without changing one decision.
#[allow(clippy::struct_excessive_bools)]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct CatalogPolicy {
    /// The selected source must expose an exact regular `SKILL.md` directory entry.
    pub requires_exact_skill_md_entry: bool,
    /// Frontmatter is parsed even when metadata validation is [`ValidationLevel::None`].
    pub always_parses_metadata: bool,
    /// A non-empty frontmatter `name` is required at [`ValidationLevel::Basic`].
    pub requires_name: bool,
    /// A non-empty frontmatter `description` is required.
    pub requires_description: bool,
    /// A present frontmatter `name` must equal the portable mount name.
    pub requires_matching_name: bool,
}

/// Fully resolved, mutation-free input for session planning.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RunContext {
    /// Resolved configuration for the selected Agent.
    pub agent: ResolvedAgent,
    /// Directory from which the wrapper was invoked.
    pub invocation_cwd: PathBuf,
    /// Directory in which the agent will later run.
    pub launch_cwd: PathBuf,
    /// Project root used for discovery.
    pub project_root: PathBuf,
    /// Ordered source occurrences.
    pub skill_sources: Vec<SourceOccurrence>,
    /// Identifier for a mutating session, or `None` while planning is still preliminary.
    ///
    /// A staging layout is addressed by this value, so it cannot be invented during planning: two
    /// identical `--dry-run` invocations would then print different output. A preliminary plan uses
    /// [`crate::state::PENDING_SESSION`] instead, and a mutating run mints the identifier once and
    /// replans with it before any transaction-owned staging entry is created — which is also what
    /// keeps two concurrent Claude sessions in separate staging roots.
    pub session_id: Option<crate::journal::TransactionId>,
    /// Opaque arguments following the standalone `--`.
    pub passthrough_args: Vec<OsString>,
    /// Validated wrapper options.
    pub options: RunOptions,
}

impl RunContext {
    /// Returns the selected Agent's closed identity.
    #[must_use]
    pub const fn agent_id(&self) -> AgentId {
        self.agent.id()
    }

    /// Returns the selected Agent's stable declarative metadata.
    #[must_use]
    pub const fn descriptor(&self) -> &'static AgentDescriptor {
        self.agent.descriptor()
    }

    /// Returns the executable the later process layer launches.
    #[must_use]
    pub fn executable(&self) -> &Path {
        self.agent.executable()
    }
}

/// A portable, safe Skill mount name.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct SkillName(String);

impl SkillName {
    /// Validates and constructs a portable Skill name.
    ///
    /// Names contain 1–64 lowercase ASCII letters, digits, or single hyphens. They must
    /// begin and end with an alphanumeric character.
    ///
    /// # Errors
    ///
    /// Returns a [`SkillNameError`] when the platform-native value is not valid UTF-8 or does
    /// not follow the portable lowercase-ASCII grammar.
    pub fn parse(value: &OsStr) -> Result<Self, SkillNameError> {
        let value = value.to_str().ok_or(SkillNameError::NonUtf8)?;
        if value.is_empty() {
            return Err(SkillNameError::Empty);
        }
        if value.len() > 64 {
            return Err(SkillNameError::TooLong);
        }
        if !value
            .bytes()
            .all(|byte| byte.is_ascii_lowercase() || byte.is_ascii_digit() || byte == b'-')
        {
            return Err(SkillNameError::InvalidCharacter);
        }
        if value.starts_with('-') || value.ends_with('-') {
            return Err(SkillNameError::BoundaryHyphen);
        }
        if value.contains("--") {
            return Err(SkillNameError::ConsecutiveHyphens);
        }
        Ok(Self(value.to_owned()))
    }

    /// Returns the validated name.
    #[must_use]
    pub fn as_str(&self) -> &str {
        &self.0
    }

    /// Returns the deterministic comparison key.
    #[must_use]
    pub fn comparison_key(&self) -> SkillNameKey {
        SkillNameKey::new(OsStr::new(&self.0))
    }
}

impl fmt::Display for SkillName {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        self.0.fmt(formatter)
    }
}

/// Why a directory name is not a portable Skill mount name.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SkillNameError {
    /// The platform-native name is not valid UTF-8.
    NonUtf8,
    /// The name is empty.
    Empty,
    /// The name exceeds 64 bytes.
    TooLong,
    /// The name contains a character outside lowercase ASCII, digits, and hyphens.
    InvalidCharacter,
    /// The name starts or ends with a hyphen.
    BoundaryHyphen,
    /// The name contains consecutive hyphens.
    ConsecutiveHyphens,
}

impl fmt::Display for SkillNameError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        let message = match self {
            Self::NonUtf8 => "name is not valid UTF-8",
            Self::Empty => "name is empty",
            Self::TooLong => "name exceeds 64 bytes",
            Self::InvalidCharacter => {
                "name must contain only lowercase ASCII letters, digits, and hyphens"
            }
            Self::BoundaryHyphen => "name must start and end with a letter or digit",
            Self::ConsecutiveHyphens => "name must not contain consecutive hyphens",
        };
        formatter.write_str(message)
    }
}

impl Error for SkillNameError {}

/// ASCII-lowercase logical identity shared by source overlay and discovery inspection.
///
/// Discovery entries are arbitrary platform-native names that may not be valid [`SkillName`]
/// values, so the key is retained as an [`OsString`]. Folding is restricted to ASCII on purpose:
/// full Unicode case folding is locale-sensitive and would make the comparison key
/// host-dependent.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct SkillNameKey(OsString);

impl SkillNameKey {
    /// Builds the comparison key for a platform-native entry name.
    #[must_use]
    pub fn new(name: &OsStr) -> Self {
        Self(ascii_lowercase(name))
    }

    /// Returns the comparison key as a platform-native value.
    #[must_use]
    pub fn as_os_str(&self) -> &OsStr {
        &self.0
    }
}

impl fmt::Display for SkillNameKey {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        // `OsStr::display` is newer than the crate MSRV, so the name is rendered through `Path`.
        Path::new(&self.0).display().fmt(formatter)
    }
}

#[cfg(unix)]
fn ascii_lowercase(value: &OsStr) -> OsString {
    use std::os::unix::ffi::{OsStrExt, OsStringExt};

    OsString::from_vec(
        value
            .as_bytes()
            .iter()
            .map(u8::to_ascii_lowercase)
            .collect(),
    )
}

#[cfg(windows)]
fn ascii_lowercase(value: &OsStr) -> OsString {
    use std::os::windows::ffi::{OsStrExt, OsStringExt};

    let lowered = value
        .encode_wide()
        .map(|unit| {
            if (u16::from(b'A')..=u16::from(b'Z')).contains(&unit) {
                unit + u16::from(b'a' - b'A')
            } else {
                unit
            }
        })
        .collect::<Vec<_>>();
    OsString::from_wide(&lowered)
}

/// The source identity retained for a selected or displaced candidate.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SkillOrigin {
    /// Zero-based source occurrence.
    pub source_ordinal: usize,
    /// Candidate directory as discovered through that occurrence.
    pub source_entry: PathBuf,
    /// Canonical candidate directory.
    pub source_canonical: PathBuf,
}

/// Known metadata extracted from a `SKILL.md` frontmatter envelope.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct SkillMetadata {
    /// Optional metadata name.
    pub name: Option<String>,
    /// Optional metadata description.
    pub description: Option<String>,
}

/// A fully validated selected Skill.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Skill {
    /// Safe destination component.
    pub mount_name: SkillName,
    /// Winning source identity.
    pub origin: SkillOrigin,
    /// Original `SKILL.md` entry path.
    pub skill_md: PathBuf,
    /// Metadata extracted under the active validation policy.
    pub metadata: SkillMetadata,
}

/// Why a candidate was displaced by a later occurrence.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ShadowReason {
    /// A later occurrence selected a different canonical source.
    DifferentSourceOverride,
    /// A later occurrence repeated the same canonical source.
    RepeatedCanonicalSource,
}

/// Provenance for one displaced candidate.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ShadowedSkill {
    /// Displaced source identity.
    pub origin: SkillOrigin,
    /// Displacement classification.
    pub reason: ShadowReason,
}

/// One selected Skill and all of its displaced origins.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ResolvedSkill {
    /// Final rightmost winner.
    pub selected: Skill,
    /// Every earlier displaced origin.
    pub shadowed: Vec<ShadowedSkill>,
}

/// An immutable, deterministic, validated catalog.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SkillCatalog {
    /// Selected Skills sorted by comparison key.
    pub resolutions: Vec<ResolvedSkill>,
    /// Non-fatal validation diagnostics.
    pub warnings: Vec<Diagnostic>,
}

impl SkillCatalog {
    /// Counts logical names with at least one different-source override.
    #[must_use]
    pub fn override_count(&self) -> usize {
        self.resolutions
            .iter()
            .filter(|resolution| {
                resolution
                    .shadowed
                    .iter()
                    .any(|shadowed| shadowed.reason == ShadowReason::DifferentSourceOverride)
            })
            .count()
    }
}

#[cfg(test)]
mod tests {
    use super::{AgentId, MountMode};
    use std::collections::BTreeSet;

    #[test]
    fn every_supported_agent_has_one_unique_descriptor() {
        let mut labels = BTreeSet::new();
        let mut display_names = BTreeSet::new();
        let mut executables = BTreeSet::new();
        for agent in AgentId::ALL {
            let descriptor = agent.descriptor();
            assert_eq!(
                descriptor.id(),
                *agent,
                "the registry entry and its descriptor must agree"
            );
            assert!(labels.insert(descriptor.label()), "duplicate journal label");
            assert!(
                display_names.insert(descriptor.display_name()),
                "duplicate display name"
            );
            assert!(
                executables.insert(descriptor.executable_name()),
                "duplicate default executable"
            );
        }
        assert_eq!(labels.len(), AgentId::ALL.len());
    }

    #[test]
    fn a_journal_label_round_trips_and_an_unknown_label_fails_closed() {
        for agent in AgentId::ALL {
            assert_eq!(AgentId::parse(agent.label()), Some(*agent));
        }
        // A journal written by a later release must be retained, not reinterpreted.
        for unknown in ["", "omp", "CODEX", "codex ", "claude-code"] {
            assert_eq!(AgentId::parse(unknown), None, "{unknown:?}");
        }
    }

    #[test]
    fn the_default_executable_is_the_bare_basename_looked_up_through_path() {
        assert_eq!(AgentId::Codex.executable_name(), "codex");
        assert_eq!(AgentId::Claude.executable_name(), "claude");
    }

    #[test]
    fn declarative_mount_policy_reproduces_each_agents_documented_defaults() {
        let codex = AgentId::Codex.descriptor();
        assert_eq!(codex.default_mount_mode(), MountMode::Project);
        assert!(codex.supports_explicit_mount_mode(MountMode::Project));
        assert!(
            !codex.supports_explicit_mount_mode(MountMode::Staging),
            "Codex has no isolated staging namespace"
        );

        let claude = AgentId::Claude.descriptor();
        assert_eq!(claude.default_mount_mode(), MountMode::Staging);
        assert!(claude.supports_explicit_mount_mode(MountMode::Project));
        assert!(claude.supports_explicit_mount_mode(MountMode::Staging));
    }

    #[test]
    fn project_layout_paths_stay_distinct_across_agents() {
        let mut seen = BTreeSet::new();
        for agent in AgentId::ALL {
            for relative in agent.descriptor().project_layout_paths() {
                assert!(
                    seen.insert(*relative),
                    "two Agents claim the same project layout {relative}"
                );
            }
        }
        assert!(seen.contains(".agents/skills"));
        assert!(seen.contains(".codex/skills"));
        assert!(seen.contains(".claude/skills"));
    }
}
