//! Process-supervisor fixture that keeps stream redirection outside production code.

use std::env;
use std::ffi::OsString;
use std::fs::{self, OpenOptions};
use std::io::{self, Write};
use std::path::PathBuf;
use std::process::ExitCode;
#[cfg(unix)]
use std::thread;
#[cfg(unix)]
use std::time::{Duration, Instant};

use skillmount::mount::LaunchPlan;
use skillmount::process::{CleanupFailure, ProcessSupervisor, SupervisionRequest, map_exit};

const EXECUTABLE_ENV: &str = "SKILLMOUNT_HARNESS_EXECUTABLE";
const CWD_ENV: &str = "SKILLMOUNT_HARNESS_CWD";
const INJECTED_COUNT_ENV: &str = "SKILLMOUNT_HARNESS_INJECTED_COUNT";
const CLEANUP_COUNTER_ENV: &str = "SKILLMOUNT_HARNESS_CLEANUP_COUNTER";
const CLEANUP_FAIL_ENV: &str = "SKILLMOUNT_HARNESS_CLEANUP_FAIL";
const CLEANUP_DELAY_ENV: &str = "SKILLMOUNT_HARNESS_CLEANUP_DELAY_MS";
const OUTCOME_ENV: &str = "SKILLMOUNT_HARNESS_OUTCOME";
const RUNS_ENV: &str = "SKILLMOUNT_HARNESS_RUNS";
#[cfg(unix)]
const SELF_INTERRUPTS_ENV: &str = "SKILLMOUNT_HARNESS_SELF_INTERRUPTS";

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
    let runs = env::var(RUNS_ENV)
        .unwrap_or_else(|_| "1".to_owned())
        .parse::<usize>()
        .map_err(invalid_data)?;
    if runs == 0 {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "supervisor harness run count must be positive",
        ));
    }
    #[cfg(unix)]
    spawn_self_interrupts()?;
    let launch = LaunchPlan {
        executable: PathBuf::from(executable),
        cwd: cleanup_cwd.clone(),
        injected_args: injected_args.to_vec(),
        passthrough_args: passthrough_args.to_vec(),
    };
    let mut outcomes = Vec::with_capacity(runs);
    let mut decisions = Vec::with_capacity(runs);
    for _ in 0..runs {
        let cleanup_counter = cleanup_counter.clone();
        let cleanup_cwd = cleanup_cwd.clone();
        let outcome = ProcessSupervisor::new()
            .supervise(SupervisionRequest::new(launch.clone()), move || {
                run_cleanup(cleanup_counter.as_deref(), cleanup_fails, &cleanup_cwd)
            });
        decisions.push(map_exit(&outcome));
        outcomes.push(outcome);
    }
    if let Some(path) = env::var_os(OUTCOME_ENV) {
        if runs == 1 {
            fs::write(
                path,
                format!("outcome={:#?}\ndecision={:#?}\n", outcomes[0], decisions[0]),
            )?;
        } else {
            fs::write(
                path,
                format!("outcomes={outcomes:#?}\ndecisions={decisions:#?}\n"),
            )?;
        }
    }
    Ok(decisions
        .iter()
        .find(|decision| decision.code != 0)
        .map_or(0, |decision| decision.code))
}

#[cfg(unix)]
fn spawn_self_interrupts() -> io::Result<()> {
    use signal_hook::consts::signal::SIGINT;

    let Some(count) = env::var_os(SELF_INTERRUPTS_ENV) else {
        return Ok(());
    };
    let count = count
        .into_string()
        .map_err(|_| io::Error::new(io::ErrorKind::InvalidData, "interrupt count is not UTF-8"))?
        .parse::<usize>()
        .map_err(invalid_data)?;
    let record = required_os("SKILLMOUNT_FAKE_RECORD").map(PathBuf::from)?;
    thread::spawn(move || {
        let started = Instant::now();
        while started.elapsed() < Duration::from_secs(10) {
            if fs::read_to_string(&record).is_ok_and(|text| text.contains("event=ready\n")) {
                for _ in 0..count {
                    signal_hook::low_level::raise(SIGINT)
                        .expect("raise configured harness interrupt");
                }
                return;
            }
            thread::sleep(Duration::from_millis(5));
        }
        panic!("timed out waiting to raise configured harness interrupts");
    });
    Ok(())
}

fn run_cleanup(
    counter: Option<&std::path::Path>,
    configured_failure: bool,
    cwd: &std::path::Path,
) -> Result<(), CleanupFailure> {
    if let Some(path) = counter {
        let result = OpenOptions::new()
            .create(true)
            .append(true)
            .open(path)
            .and_then(|mut file| file.write_all(b"cleanup\n"));
        if let Err(error) = result {
            return Err(cleanup_failure(
                cwd,
                format!("could not record cleanup: {error}"),
            ));
        }
    }
    if let Some(delay) = env::var_os(CLEANUP_DELAY_ENV) {
        let delay = delay
            .into_string()
            .map_err(|_| "configured cleanup delay is not UTF-8")
            .and_then(|value| value.parse::<u64>().map_err(|_| "invalid cleanup delay"));
        match delay {
            Ok(delay) => std::thread::sleep(std::time::Duration::from_millis(delay)),
            Err(reason) => return Err(cleanup_failure(cwd, reason.to_owned())),
        }
    }
    if configured_failure {
        Err(cleanup_failure(
            cwd,
            "configured cleanup failure".to_owned(),
        ))
    } else {
        Ok(())
    }
}

fn cleanup_failure(cwd: &std::path::Path, reason: String) -> CleanupFailure {
    CleanupFailure {
        reason,
        failed_paths: vec![cwd.join("retained-mount")],
        retained_journal: Some(cwd.join("retained-journal")),
        recovery_command: vec![OsString::from("asm"), OsString::from("cleanup")],
    }
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
