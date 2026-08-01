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
    /// Filesystem mutation or cleanup failure.
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

/// An error returned by the shared application boundary.
#[derive(Debug)]
pub enum AppError {
    /// CLI syntax or compatibility failure.
    Usage(String),
    /// Invalid catalog data.
    Catalog(CatalogError),
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
            Self::MissingInput { .. } => ExitCategory::MissingInput,
            Self::Internal(_) => ExitCategory::Internal,
            Self::Filesystem(_) => ExitCategory::Filesystem,
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
            _ => None,
        }
    }
}

impl From<CatalogError> for AppError {
    fn from(error: CatalogError) -> Self {
        Self::Catalog(error)
    }
}
