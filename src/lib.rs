//! Catalog, planning, durable mutation, and the shared executable boundary for `SkillMount`.
//!
//! The read-only half parses the wrapper contract, preserves platform-native paths, resolves an
//! ordered Skill catalog, inspects the namespaces in the selected adapter's current discovery
//! model, and builds one deterministic mount plan. `inspect` and `--dry-run` render dated
//! last-tested evidence but create no directory, link, lock, journal, recovery mutation, version
//! process, or Agent child; `tests/read_only.rs` fails if any of them ever does.
//!
//! A mutating session checks release-independent launch invariants, acquires the discovery
//! snapshot's resource locks, recovers incomplete transactions, builds and stabilizes the full plan
//! under those locks, persists a write-ahead journal, and applies it. It starts no Agent process
//! before the supervised child: mount visibility and removal come from the journal, the locks,
//! proven process-domain death, and ownership-verified removal, none of which depend on the
//! installed release. The implemented Codex, Claude, and OMP sessions repeat only the hard
//! invariants before launch, then use the generic process supervisor and clean up after the managed
//! process domain is dead; see `docs/architecture.md`, ADR 0033 and ADR 0036 for the version
//! evidence boundary, and ADR 0034 for the OMP discovery and launch contract.

pub mod agent;
mod app;
pub mod catalog;
pub mod checkpoint;
mod cli;
mod completion;
pub mod diagnostic;
pub mod domain;
pub mod error;
pub mod journal;
pub mod link;
pub mod lock;
pub mod mount;
mod native;
mod operator;
mod paths;
pub mod process;
mod render;
pub mod state;
#[cfg(test)]
mod test_support;
pub mod transaction;

use std::ffi::OsString;
use std::process::ExitCode;

/// Runs the shared `SkillMount` command-line entry point.
///
/// Both installed executable names delegate directly to this function. Arguments and paths remain
/// platform-native authority values; only final diagnostics may render a proved shell convenience.
#[must_use]
pub fn run_from<I, T>(args: I) -> ExitCode
where
    I: IntoIterator<Item = T>,
    T: Into<OsString> + Clone,
{
    app::run_from(args)
}
