//! Process-supervisor fixture that keeps stream redirection outside production code.

use std::env;
use std::ffi::OsString;
use std::fs::{self, OpenOptions};
use std::io::{self, Write};
use std::path::PathBuf;
use std::process::ExitCode;

use skillmount::mount::LaunchPlan;
use skillmount::process::{
    CleanupFailure, CleanupOutcome, ProcessSupervisor, SupervisionRequest, map_exit,
};

const EXECUTABLE_ENV: &str = "SKILLMOUNT_HARNESS_EXECUTABLE";
const CWD_ENV: &str = "SKILLMOUNT_HARNESS_CWD";
const INJECTED_COUNT_ENV: &str = "SKILLMOUNT_HARNESS_INJECTED_COUNT";
const CLEANUP_COUNTER_ENV: &str = "SKILLMOUNT_HARNESS_CLEANUP_COUNTER";
const CLEANUP_FAIL_ENV: &str = "SKILLMOUNT_HARNESS_CLEANUP_FAIL";
const OUTCOME_ENV: &str = "SKILLMOUNT_HARNESS_OUTCOME";

fn main() -> ExitCode {
    match run() {
        Ok(code) => ExitCode::from(code),
        Err(error) => {
            eprintln!("skillmount supervisor harness: {error}");
            ExitCode::from(70)
        }
    }
}

fn run() -> io::Result<u8> {
    let executable = required_os(EXECUTABLE_ENV)?;
    let cwd = required_os(CWD_ENV)?;
    let arguments: Vec<_> = env::args_os().skip(1).collect();
    let injected_count = env::var(INJECTED_COUNT_ENV)
        .unwrap_or_else(|_| "0".to_owned())
        .parse::<usize>()
        .map_err(invalid_data)?;
    if injected_count > arguments.len() {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "injected argument count exceeds the supplied argument count",
        ));
    }
    let (injected_args, passthrough_args) = arguments.split_at(injected_count);

    let cleanup_counter = env::var_os(CLEANUP_COUNTER_ENV).map(PathBuf::from);
    let cleanup_fails = env::var_os(CLEANUP_FAIL_ENV).is_some_and(|value| value == "1");
    let cleanup_cwd = PathBuf::from(&cwd);
    let outcome = ProcessSupervisor::new().supervise(
        SupervisionRequest::new(LaunchPlan {
            executable: PathBuf::from(executable),
            cwd: cleanup_cwd.clone(),
            injected_args: injected_args.to_vec(),
            passthrough_args: passthrough_args.to_vec(),
        }),
        move || run_cleanup(cleanup_counter.as_deref(), cleanup_fails, &cleanup_cwd),
    );
    let decision = map_exit(&outcome);
    if let Some(path) = env::var_os(OUTCOME_ENV) {
        fs::write(
            path,
            format!("outcome={outcome:#?}\ndecision={decision:#?}\n"),
        )?;
    }
    Ok(decision.code)
}

fn run_cleanup(
    counter: Option<&std::path::Path>,
    configured_failure: bool,
    cwd: &std::path::Path,
) -> CleanupOutcome {
    if let Some(path) = counter {
        let result = OpenOptions::new()
            .create(true)
            .append(true)
            .open(path)
            .and_then(|mut file| file.write_all(b"cleanup\n"));
        if let Err(error) = result {
            return cleanup_failure(cwd, format!("could not record cleanup: {error}"));
        }
    }
    if configured_failure {
        cleanup_failure(cwd, "configured cleanup failure".to_owned())
    } else {
        CleanupOutcome::Succeeded
    }
}

fn cleanup_failure(cwd: &std::path::Path, reason: String) -> CleanupOutcome {
    CleanupOutcome::Failed(CleanupFailure {
        reason,
        failed_paths: vec![cwd.join("retained-mount")],
        retained_journal: Some(cwd.join("retained-journal")),
        recovery_command: vec![OsString::from("asm"), OsString::from("cleanup")],
    })
}

fn required_os(name: &str) -> io::Result<OsString> {
    env::var_os(name).ok_or_else(|| {
        io::Error::new(
            io::ErrorKind::InvalidInput,
            format!("required environment variable {name} is absent"),
        )
    })
}

fn invalid_data(error: impl std::error::Error + Send + Sync + 'static) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidData, error)
}
