//! Application and catalog domain types.

use std::error::Error;
use std::ffi::{OsStr, OsString};
use std::fmt;
use std::path::{Path, PathBuf};

use crate::diagnostic::Diagnostic;

/// A supported agent adapter.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AgentId {
    /// Codex CLI from `OpenAI`.
    Codex,
    /// Anthropic Claude Code CLI.
    Claude,
}

impl AgentId {
    /// Returns the executable name searched through `PATH` by the later process layer.
    #[must_use]
    pub fn executable_name(self) -> &'static OsStr {
        match self {
            Self::Codex => OsStr::new("codex"),
            Self::Claude => OsStr::new("claude"),
        }
    }

    /// Returns the stable label used in output and in the transaction journal.
    #[must_use]
    pub const fn label(self) -> &'static str {
        match self {
            Self::Codex => "codex",
            Self::Claude => "claude",
        }
    }

    /// Parses a label a journal recorded earlier.
    #[must_use]
    pub fn parse(label: &str) -> Option<Self> {
        match label {
            "codex" => Some(Self::Codex),
            "claude" => Some(Self::Claude),
            _ => None,
        }
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

/// Fully resolved, mutation-free input for session planning.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RunContext {
    /// Selected adapter.
    pub agent: AgentId,
    /// Directory from which the wrapper was invoked.
    pub invocation_cwd: PathBuf,
    /// Directory in which the agent will later run.
    pub launch_cwd: PathBuf,
    /// Project root used for discovery.
    pub project_root: PathBuf,
    /// User home used by agent-level discovery roots.
    pub user_home: PathBuf,
    /// Codex configuration home, including deprecated and bundled Skill roots.
    pub codex_home: PathBuf,
    /// Canonical `CODEX_HOME` value that must be propagated to a Codex child.
    ///
    /// `None` means Codex ignored an absent, empty, or non-Unicode override and derived its
    /// default from the user home instead.
    pub codex_home_override: Option<PathBuf>,
    /// Host-wide Codex Skill root when the platform exposes one.
    pub codex_admin_skills: Option<PathBuf>,
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
    /// Explicit resolved agent path, or the bare executable name for `PATH` lookup.
    pub agent_bin: PathBuf,
    /// Opaque arguments following the standalone `--`.
    pub passthrough_args: Vec<OsString>,
    /// Validated wrapper options.
    pub options: RunOptions,
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
