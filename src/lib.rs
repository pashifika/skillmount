//! Shared application boundary for the `SkillMount` executables.
//!
//! The foundation intentionally supports only help and version output. Skill discovery,
//! mounting, transactions, and agent launching will be added by later changes.

use std::ffi::{OsStr, OsString};
use std::process::ExitCode;

const HELP: &str = concat!(
    "SkillMount ",
    env!("CARGO_PKG_VERSION"),
    "\n\n",
    "Portable skill mounting for coding agents.\n\n",
    "Usage: <asm|skillmount> [OPTIONS]\n\n",
    "Options:\n",
    "  -h, --help       Print help\n",
    "  -V, --version    Print version\n",
);

const NOT_IMPLEMENTED: &str =
    "error: SkillMount commands are not implemented yet; use --help for available options";

/// Runs the shared `SkillMount` command-line entry point.
///
/// Both installed executable names delegate directly to this function. The first argument is
/// treated as the executable name and is intentionally omitted from user-facing output so the
/// two shims remain behaviorally identical.
#[must_use]
pub fn run_from<I, T>(args: I) -> ExitCode
where
    I: IntoIterator<Item = T>,
    T: Into<OsString>,
{
    let mut args = args.into_iter().map(Into::into);
    let _executable = args.next();
    let option = args.next();

    if args.next().is_some() {
        eprintln!("{NOT_IMPLEMENTED}");
        return ExitCode::from(2);
    }

    match option.as_deref() {
        None => {
            print!("{HELP}");
            ExitCode::SUCCESS
        }
        Some(option) if option == OsStr::new("-h") || option == OsStr::new("--help") => {
            print!("{HELP}");
            ExitCode::SUCCESS
        }
        Some(option) if option == OsStr::new("-V") || option == OsStr::new("--version") => {
            println!("SkillMount {}", env!("CARGO_PKG_VERSION"));
            ExitCode::SUCCESS
        }
        Some(_) => {
            eprintln!("{NOT_IMPLEMENTED}");
            ExitCode::from(2)
        }
    }
}
