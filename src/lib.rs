//! Side-effect-free catalog, planning, and shared executable boundary for `SkillMount`.
//!
//! This slice parses the complete wrapper contract, resolves platform-native paths, discovers
//! ordered Skill sources, produces an immutable validated catalog, inspects every namespace the
//! child agent will search, and builds one complete mount plan. Everything up to and including the
//! plan is read-only: `inspect` and `--dry-run` create no directory, link, lock, journal, or child
//! process, and `tests/read_only.rs` fails if any of them ever does.
//!
//! Applying a plan, acquiring locks, rebuilding under lock, writing a transaction journal,
//! recovering, and launching the agent intentionally remain outside this change.

pub mod agent;
mod app;
pub mod catalog;
mod cli;
pub mod diagnostic;
pub mod domain;
pub mod error;
pub mod lock;
pub mod mount;
mod paths;
mod render;
pub mod state;
#[cfg(test)]
mod test_support;

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
