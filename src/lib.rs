//! Catalog, planning, durable mutation, and the shared executable boundary for `SkillMount`.
//!
//! The read-only half parses the wrapper contract, preserves platform-native paths, resolves an
//! ordered Skill catalog, inspects every namespace the child agent would search, and builds one
//! deterministic mount plan. `inspect` and `--dry-run` create no directory, link, lock, journal,
//! recovery mutation, or child process, and `tests/read_only.rs` fails if any of them ever does.
//!
//! A mutating session acquires the discovery snapshot's resource locks, recovers incomplete
//! transactions, builds and stabilizes the full plan under those locks, persists a write-ahead
//! journal, applies, and cleans up. Agent child launch is the remaining reserved boundary; see
//! `docs/architecture.md` for current status and the cross-module ordering rules.

pub mod agent;
mod app;
pub mod catalog;
pub mod checkpoint;
mod cli;
pub mod diagnostic;
pub mod domain;
pub mod error;
pub mod journal;
pub mod link;
pub mod lock;
pub mod mount;
mod native;
mod paths;
mod render;
pub mod state;
#[cfg(test)]
mod test_support;
pub mod transaction;

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
