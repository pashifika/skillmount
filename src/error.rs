//! Typed application errors and stable wrapper exit categories.

use std::error::Error;
use std::ffi::OsString;
use std::fmt;
use std::path::PathBuf;

/// Stable wrapper exit categories.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(u8)]
pub enum ExitCategory {
    /// Invalid syntax or an incompatible option combination.
    Usage = 64,
    /// Invalid catalog data or selected Skill.
    Data = 65,
    /// Missing, inaccessible, or wrongly typed input.
    MissingInput = 66,
    /// Internal invariant or unsupported application boundary.
    Internal = 70,
    /// Link creation or removal, cleanup, permission, or destination-conflict failure.
    Filesystem = 73,
    /// Temporary lock or stale-transaction failure.
    Temporary = 75,
    /// User interrupt.
    Interrupted = 130,
}

impl ExitCategory {
    /// Returns the process exit code.
    #[must_use]
    pub const fn code(self) -> u8 {
        self as u8
    }
}

/// Catalog/data errors that must fail before planning.
#[derive(Debug)]
pub enum CatalogError {
    /// An accessible catalog contains no structural candidates.
    EmptyCatalog {
        /// Zero-based source occurrence.
        source_ordinal: usize,
        /// Catalog input path.
        path: PathBuf,
    },
    /// Two entries within one occurrence have the same logical key.
    DuplicateLogicalName {
        /// Zero-based source occurrence.
        source_ordinal: usize,
        /// First candidate in deterministic order.
        first: PathBuf,
        /// Conflicting candidate.
        second: PathBuf,
    },
    /// A selected directory name is unsafe.
    InvalidSkillName {
        /// Selected Skill directory.
        path: PathBuf,
        /// Validation failure.
        reason: String,
    },
    /// Two selected names resolve to the same canonical directory.
    CanonicalAlias {
        /// Shared canonical directory.
        canonical: PathBuf,
        /// First selected mount name.
        first_name: OsString,
        /// Conflicting selected mount name.
        second_name: OsString,
    },
    /// A selected candidate violates structural or metadata validation.
    InvalidSelectedSkill {
        /// Selected Skill or `SKILL.md` path.
        path: PathBuf,
        /// Validation failure.
        reason: String,
    },
    /// A selected source overlaps a future destination store.
    SourceDestinationCycle {
        /// Source directory involved in the overlap.
        source: PathBuf,
        /// Future destination store involved in the overlap.
        destination: PathBuf,
    },
}

impl fmt::Display for CatalogError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::EmptyCatalog {
                source_ordinal,
                path,
            } => write!(
                formatter,
                "--skills-dir #{} is an empty Skill catalog: {}",
                source_ordinal + 1,
                path.display()
            ),
            Self::DuplicateLogicalName {
                source_ordinal,
                first,
                second,
            } => write!(
                formatter,
                "--skills-dir #{} contains duplicate logical Skill names: {} and {}",
                source_ordinal + 1,
                first.display(),
                second.display()
            ),
            Self::InvalidSkillName { path, reason } => {
                write!(
                    formatter,
                    "invalid Skill mount name at {}: {reason}",
                    path.display()
                )
            }
            Self::CanonicalAlias {
                canonical,
                first_name,
                second_name,
            } => write!(
                formatter,
                "different Skill names {:?} and {:?} resolve to the same directory {}",
                first_name,
                second_name,
                canonical.display()
            ),
            Self::InvalidSelectedSkill { path, reason } => {
                write!(
                    formatter,
                    "invalid selected Skill {}: {reason}",
                    path.display()
                )
            }
            Self::SourceDestinationCycle {
                source,
                destination,
            } => write!(
                formatter,
                "Skill source {} overlaps destination store {}",
                source.display(),
                destination.display()
            ),
        }
    }
}

impl Error for CatalogError {}

/// Planning failures that must stop a run while it is still read-only.
///
/// These report [`ExitCategory::Filesystem`], not the catalog category: the selected catalog is
/// valid and the obstruction is destination state the operator owns. Exit code 73 is the
/// destination-conflict code, so a caller can distinguish "your Skills are wrong" from "your
/// project already has something there" without parsing text.
#[derive(Debug)]
pub enum PlanError {
    /// A discovery entry is broken, cyclic, over-deep, or not a directory.
    AmbiguousDiscoveryEntry {
        /// Discovery entry that could not be resolved.
        path: PathBuf,
        /// Observed classification.
        state: &'static str,
    },
    /// A selected Skill collides with an entry the agent can already see.
    DestinationConflict {
        /// Logical Skill name.
        name: OsString,
        /// Scope in which the collision was observed.
        scope: &'static str,
        /// Existing entry that occupies the name.
        existing: PathBuf,
        /// Observed classification of the existing entry.
        existing_state: &'static str,
        /// Canonical source of the selected Skill.
        selected: PathBuf,
    },
    /// The observed layout is one that planning refuses to interpret.
    UnsupportedLayout {
        /// Path the layout was observed at.
        path: PathBuf,
        /// Why the layout cannot be planned.
        reason: String,
    },
}

impl fmt::Display for PlanError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::AmbiguousDiscoveryEntry { path, state } => write!(
                formatter,
                "discovery entry {} is a {state} and cannot be planned",
                path.display()
            ),
            Self::DestinationConflict {
                name,
                scope,
                existing,
                existing_state,
                selected,
            } => write!(
                formatter,
                "Skill {name:?} from {} conflicts with the {existing_state} {} already visible in the {scope} scope",
                selected.display(),
                existing.display()
            ),
            Self::UnsupportedLayout { path, reason } => {
                write!(formatter, "cannot plan {}: {reason}", path.display())
            }
        }
    }
}

impl Error for PlanError {}

/// Platform link-backend failures.
///
/// These are separate from [`PlanError`] because they occur *after* planning, against real
/// filesystem state. An unresolvable chain and an ownership mismatch are deliberately not here:
/// both are typed outcomes the caller decides about, not failures the backend imposes.
#[derive(Debug)]
pub enum LinkError {
    /// An entry could not be classified without following it.
    Inspect {
        /// Entry that could not be inspected.
        path: PathBuf,
        /// Operating-system failure.
        reason: String,
    },
    /// A directory link could not be created.
    Create {
        /// Path the link was being created at.
        destination: PathBuf,
        /// Directory the link was to refer to.
        source: PathBuf,
        /// Operating-system or eligibility failure.
        reason: String,
    },
    /// A staged entry could not be placed at its destination.
    Place {
        /// Transaction-unique staged entry.
        staged: PathBuf,
        /// Final destination the entry was to occupy.
        destination: PathBuf,
        /// Operating-system failure.
        reason: String,
    },
    /// A verified link entry could not be removed.
    Remove {
        /// Entry that could not be removed.
        path: PathBuf,
        /// Operating-system failure.
        reason: String,
    },
    /// The host cannot provide a guarantee the backend refuses to emulate.
    ///
    /// Reported instead of a check-then-act sequence. The V2 contract requires atomic no-replace
    /// placement; emulating it with a separate existence check would reintroduce exactly the race
    /// the guarantee exists to remove.
    Unsupported {
        /// Path the unavailable operation targeted.
        path: PathBuf,
        /// Capability the host does not provide.
        reason: String,
    },
    /// A directory link chain could not be resolved to a directory.
    UnresolvableChain {
        /// Entry the chain started at.
        entry: PathBuf,
        /// Observed chain state.
        state: &'static str,
    },
}

impl fmt::Display for LinkError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Inspect { path, reason } => {
                write!(formatter, "cannot inspect {}: {reason}", path.display())
            }
            Self::Create {
                destination,
                source,
                reason,
            } => write!(
                formatter,
                "cannot create a directory link at {} for {}: {reason}",
                destination.display(),
                source.display()
            ),
            Self::Place {
                staged,
                destination,
                reason,
            } => write!(
                formatter,
                "cannot place {} at {}: {reason}",
                staged.display(),
                destination.display()
            ),
            Self::Remove { path, reason } => {
                write!(formatter, "cannot remove {}: {reason}", path.display())
            }
            Self::Unsupported { path, reason } => write!(
                formatter,
                "the host cannot perform this operation on {} safely: {reason}",
                path.display()
            ),
            Self::UnresolvableChain { entry, state } => write!(
                formatter,
                "directory link {} is a {state} and cannot be resolved",
                entry.display()
            ),
        }
    }
}

impl Error for LinkError {}

/// Transaction-journal failures.
///
/// Every variant retains the journal path. A journal names the entries a transaction owns, so a
/// journal that cannot be read is the one situation where `SkillMount` knows something of its own
/// may be on disk and cannot prove which entry it is. Deleting the file to make the error go away
/// would discard that record permanently, so the file is always kept and the path is always
/// reported.
#[derive(Debug)]
pub enum JournalError {
    /// The journal exists but the host refused to read it.
    Unreadable {
        /// Journal path.
        path: PathBuf,
        /// Operating-system failure.
        reason: String,
    },
    /// The journal is truncated, is not a journal, or failed validation.
    Corrupt {
        /// Journal path.
        path: PathBuf,
        /// Why it cannot be acted on.
        reason: String,
    },
    /// The journal was written against a schema this build does not implement.
    UnsupportedVersion {
        /// Journal path.
        path: PathBuf,
        /// Version found in the header.
        found: String,
        /// Version this build writes.
        supported: u32,
    },
    /// The journal could not be made durable.
    Write {
        /// Journal path.
        path: PathBuf,
        /// Operating-system failure.
        reason: String,
    },
}

impl JournalError {
    /// Returns the journal path, which is retained in every case.
    #[must_use]
    pub fn path(&self) -> &PathBuf {
        match self {
            Self::Unreadable { path, .. }
            | Self::Corrupt { path, .. }
            | Self::UnsupportedVersion { path, .. }
            | Self::Write { path, .. } => path,
        }
    }

    /// Returns the operator-facing explanation without the path prefix.
    #[must_use]
    pub fn reason(&self) -> String {
        match self {
            Self::Unreadable { reason, .. }
            | Self::Corrupt { reason, .. }
            | Self::Write { reason, .. } => reason.clone(),
            Self::UnsupportedVersion {
                found, supported, ..
            } => format!(
                "schema version {found} is not supported by this build (writes {supported})"
            ),
        }
    }

    /// Returns whether the failure blocks recovery rather than the current write.
    ///
    /// A journal this build cannot interpret is a *temporary* condition: a build that understands
    /// it, or an operator who removes it deliberately, resolves the situation. Reporting it as a
    /// filesystem failure would suggest the destination is at fault and invite a retry that behaves
    /// identically.
    #[must_use]
    pub const fn blocks_recovery(&self) -> bool {
        matches!(
            self,
            Self::Unreadable { .. } | Self::Corrupt { .. } | Self::UnsupportedVersion { .. }
        )
    }
}

impl fmt::Display for JournalError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Unreadable { path, reason } => write!(
                formatter,
                "cannot read transaction journal {}: {reason}; it is retained",
                path.display()
            ),
            Self::Corrupt { path, reason } => write!(
                formatter,
                "transaction journal {} cannot be acted on: {reason}; it is retained for manual review",
                path.display()
            ),
            Self::UnsupportedVersion {
                path,
                found,
                supported,
            } => write!(
                formatter,
                "transaction journal {} uses schema version {found} and this build writes {supported}; it is retained and nothing was removed",
                path.display()
            ),
            Self::Write { path, reason } => write!(
                formatter,
                "cannot make transaction journal {} durable: {reason}",
                path.display()
            ),
        }
    }
}

impl Error for JournalError {}

/// An error returned by the shared application boundary.
#[derive(Debug)]
pub enum AppError {
    /// CLI syntax or compatibility failure.
    Usage(String),
    /// Invalid catalog data.
    Catalog(CatalogError),
    /// A resolved catalog cannot be realized against the observed project state.
    Plan(PlanError),
    /// A platform link backend could not complete an operation.
    Link(LinkError),
    /// A transaction journal could not be written, read, or interpreted.
    Journal(JournalError),
    /// Missing, inaccessible, or wrongly typed input.
    MissingInput {
        /// Input path.
        path: PathBuf,
        /// Operating-system or type-check failure.
        reason: String,
    },
    /// Internal or not-yet-implemented application boundary.
    Internal(String),
    /// Filesystem mutation or cleanup failure.
    Filesystem(String),
    /// Temporary lock or recovery failure.
    Temporary(String),
    /// User interrupt.
    Interrupted,
}

impl AppError {
    /// Returns the stable wrapper category for this error.
    #[must_use]
    pub const fn category(&self) -> ExitCategory {
        match self {
            Self::Usage(_) => ExitCategory::Usage,
            Self::Catalog(_) => ExitCategory::Data,
            // A destination conflict is a filesystem-state failure, not a catalog failure.
            Self::Plan(_) | Self::Link(_) | Self::Filesystem(_) => ExitCategory::Filesystem,
            Self::Journal(error) => {
                if error.blocks_recovery() {
                    ExitCategory::Temporary
                } else {
                    ExitCategory::Filesystem
                }
            }
            Self::MissingInput { .. } => ExitCategory::MissingInput,
            Self::Internal(_) => ExitCategory::Internal,
            Self::Temporary(_) => ExitCategory::Temporary,
            Self::Interrupted => ExitCategory::Interrupted,
        }
    }
}

impl fmt::Display for AppError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Usage(message)
            | Self::Internal(message)
            | Self::Filesystem(message)
            | Self::Temporary(message) => formatter.write_str(message),
            Self::Catalog(error) => error.fmt(formatter),
            Self::Plan(error) => error.fmt(formatter),
            Self::Link(error) => error.fmt(formatter),
            Self::Journal(error) => error.fmt(formatter),
            Self::MissingInput { path, reason } => {
                write!(
                    formatter,
                    "input {} is unavailable: {reason}",
                    path.display()
                )
            }
            Self::Interrupted => formatter.write_str("interrupted"),
        }
    }
}

impl Error for AppError {
    fn source(&self) -> Option<&(dyn Error + 'static)> {
        match self {
            Self::Catalog(error) => Some(error),
            Self::Plan(error) => Some(error),
            Self::Link(error) => Some(error),
            Self::Journal(error) => Some(error),
            _ => None,
        }
    }
}

impl From<CatalogError> for AppError {
    fn from(error: CatalogError) -> Self {
        Self::Catalog(error)
    }
}

impl From<PlanError> for AppError {
    fn from(error: PlanError) -> Self {
        Self::Plan(error)
    }
}

impl From<LinkError> for AppError {
    fn from(error: LinkError) -> Self {
        Self::Link(error)
    }
}

impl From<JournalError> for AppError {
    fn from(error: JournalError) -> Self {
        Self::Journal(error)
    }
}
