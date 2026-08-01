//! Shared application boundary for the `SkillMount` executables.
//!
//! The foundation intentionally supports only help and version output. Skill discovery,
//! mounting, transactions, and agent launching will be added by later changes.

use std::ffi::{OsStr, OsString};
use std::io::{self, Write};
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

fn write_stdout(message: &str) -> ExitCode {
    match io::stdout().lock().write_all(message.as_bytes()) {
        Ok(()) => ExitCode::SUCCESS,
        Err(error) => {
            let exit_code = stdout_error_exit_code(&error);
            if exit_code == ExitCode::FAILURE {
                let _ = writeln!(
                    io::stderr().lock(),
                    "error: failed to write output: {error}"
                );
            }
            exit_code
        }
    }
}

fn stdout_error_exit_code(error: &io::Error) -> ExitCode {
    if error.kind() == io::ErrorKind::BrokenPipe {
        ExitCode::SUCCESS
    } else {
        ExitCode::FAILURE
    }
}

fn reject_not_implemented() -> ExitCode {
    let _ = writeln!(io::stderr().lock(), "{NOT_IMPLEMENTED}");
    ExitCode::from(2)
}

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
        return reject_not_implemented();
    }

    match option.as_deref() {
        None => write_stdout(HELP),
        Some(option) if option == OsStr::new("-h") || option == OsStr::new("--help") => {
            write_stdout(HELP)
        }
        Some(option) if option == OsStr::new("-V") || option == OsStr::new("--version") => {
            write_stdout(&format!("SkillMount {}\n", env!("CARGO_PKG_VERSION")))
        }
        Some(_) => reject_not_implemented(),
    }
}

#[cfg(test)]
mod tests {
    use super::stdout_error_exit_code;
    use std::io;
    use std::process::ExitCode;

    #[test]
    fn broken_pipe_is_a_successful_cli_termination() {
        let error = io::Error::from(io::ErrorKind::BrokenPipe);

        assert_eq!(stdout_error_exit_code(&error), ExitCode::SUCCESS);
    }

    #[test]
    fn other_stdout_errors_fail_without_panicking() {
        let error = io::Error::from(io::ErrorKind::Other);

        assert_eq!(stdout_error_exit_code(&error), ExitCode::FAILURE);
    }
}
