//! Structured diagnostics emitted by catalog resolution.

use std::path::PathBuf;

/// The severity assigned to a catalog diagnostic.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DiagnosticSeverity {
    /// A non-fatal condition that callers should surface to the user.
    Warning,
}

/// A structured, side-effect-free catalog diagnostic.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Diagnostic {
    /// Diagnostic severity.
    pub severity: DiagnosticSeverity,
    /// Human-readable summary.
    pub message: String,
    /// Related path, when applicable.
    pub path: Option<PathBuf>,
    /// Zero-based source occurrence, when applicable.
    pub source_ordinal: Option<usize>,
}

impl Diagnostic {
    /// Creates a warning associated with a selected Skill.
    #[must_use]
    pub fn warning(message: impl Into<String>, path: impl Into<PathBuf>) -> Self {
        Self {
            severity: DiagnosticSeverity::Warning,
            message: message.into(),
            path: Some(path.into()),
            source_ordinal: None,
        }
    }
}
