//! Side-effect-free catalog core and shared executable boundary for `SkillMount`.
//!
//! This slice parses the complete wrapper contract, resolves platform-native paths, discovers
//! ordered Skill sources, and produces an immutable validated catalog. Mount planning,
//! filesystem mutation, transaction recovery, and agent launch intentionally remain outside
//! this change.

mod app;
pub mod catalog;
mod cli;
pub mod diagnostic;
pub mod domain;
pub mod error;
mod paths;

use std::ffi::OsString;
use std::process::ExitCode;

/// Runs the shared `SkillMount` command-line entry point.
///
/// Both installed executable names delegate directly to this function. Arguments and paths are
/// retained as platform-native values until diagnostics are rendered.
#[must_use]
pub fn run_from<I, T>(args: I) -> ExitCode
where
    I: IntoIterator<Item = T>,
    T: Into<OsString> + Clone,
{
    app::run_from(args)
}
