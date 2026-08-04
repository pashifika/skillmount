//! Operator-facing environment diagnosis and explicit transaction recovery.

pub(crate) mod cleanup;
pub(crate) mod doctor;

/// Rendered output and stable status from one operator command.
pub(crate) struct CommandOutcome {
    pub(crate) output: String,
    pub(crate) code: u8,
}
