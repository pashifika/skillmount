//! Structured non-fatal diagnostics emitted by catalog and agent observation.

use std::path::PathBuf;

/// Stable category for a non-fatal observation.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DiagnosticKind {
    /// A catalog or shared-adapter observation with no narrower contract.
    General,
    /// A Codex discovery-layout or metadata observation.
    CodexDiscovery,
    /// A reminder that Codex Skill discovery and sandbox access are separate policies.
    CodexPermissionSeparation,
}

/// The severity assigned to a catalog diagnostic.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DiagnosticSeverity {
    /// A non-fatal condition that callers should surface to the user.
    Warning,
}

/// A structured, side-effect-free catalog diagnostic.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Diagnostic {
    /// Machine-readable category.
    pub kind: DiagnosticKind,
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
    /// Creates a general warning associated with a path.
    #[must_use]
    pub fn warning(message: impl Into<String>, path: impl Into<PathBuf>) -> Self {
        Self::warning_with_kind(DiagnosticKind::General, message, path)
    }

    /// Creates a warning with a stable machine-readable category.
    #[must_use]
    pub fn warning_with_kind(
        kind: DiagnosticKind,
        message: impl Into<String>,
        path: impl Into<PathBuf>,
    ) -> Self {
        Self {
            kind,
            severity: DiagnosticSeverity::Warning,
            message: message.into(),
            path: Some(path.into()),
            source_ordinal: None,
        }
    }
}
