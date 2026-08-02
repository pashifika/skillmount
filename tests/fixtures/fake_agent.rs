//! Cross-platform executable fixture for process-supervision integration tests.

use std::env;
use std::ffi::OsStr;
use std::fs::{self, OpenOptions};
use std::io::{self, BufRead, Write};
use std::path::{Path, PathBuf};
use std::process::{Command, ExitCode, Stdio};

const RECORD_ENV: &str = "SKILLMOUNT_FAKE_RECORD";
const DESCENDANT_RECORD_ENV: &str = "SKILLMOUNT_FAKE_DESCENDANT_RECORD";
const BEHAVIOR_ENV: &str = "SKILLMOUNT_FAKE_BEHAVIOR";
const EXIT_ENV: &str = "SKILLMOUNT_FAKE_EXIT";
const RAW_EXIT_ENV: &str = "SKILLMOUNT_FAKE_RAW_EXIT";

fn main() -> ExitCode {
    match run() {
        Ok(code) => code,
        Err(error) => {
            eprintln!("skillmount fake agent: {error}");
            ExitCode::from(70)
        }
    }
}

fn run() -> io::Result<ExitCode> {
    let record_path = required_path(RECORD_ENV)?;
    let recorder = Recorder::create(record_path)?;
    recorder.number("pid", std::process::id())?;
    for argument in env::args_os().skip(1) {
        recorder.os("arg", &argument)?;
    }
    recorder.os("cwd", &env::current_dir()?.into_os_string())?;

    let behavior = env::var(BEHAVIOR_ENV).unwrap_or_else(|_| "exit".to_owned());
    match behavior.as_str() {
        "exit" => recorder.event("ready")?,
        "streams" => use_inherited_streams(&recorder)?,
        "wait" => wait_for_interrupt(&recorder, Some(1))?,
        "ignore-first" => wait_for_interrupt(&recorder, Some(2))?,
        "ignore-all" => wait_for_interrupt(&recorder, None)?,
        "descendant-wait" => {
            let descendant_record = required_path(DESCENDANT_RECORD_ENV)?;
            spawn_descendant(&descendant_record, "wait")?;
            wait_for_interrupt(&recorder, Some(1))?;
        }
        "descendant-ignore-all" => {
            let descendant_record = required_path(DESCENDANT_RECORD_ENV)?;
            spawn_descendant(&descendant_record, "ignore-all")?;
            wait_for_interrupt(&recorder, None)?;
        }
        "orphan-descendant-ignore-all" => {
            let descendant_record = required_path(DESCENDANT_RECORD_ENV)?;
            spawn_descendant(&descendant_record, "ignore-all")?;
            recorder.event("ready")?;
        }
        other => {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                format!("unknown behavior {other:?}"),
            ));
        }
    }

    if let Ok(raw) = env::var(RAW_EXIT_ENV) {
        let raw = raw.parse::<u32>().map_err(invalid_data)?;
        std::process::exit(i32::from_ne_bytes(raw.to_ne_bytes()));
    }
    let code = env::var(EXIT_ENV)
        .unwrap_or_else(|_| "0".to_owned())
        .parse::<u8>()
        .map_err(invalid_data)?;
    Ok(ExitCode::from(code))
}

fn use_inherited_streams(recorder: &Recorder) -> io::Result<()> {
    recorder.event("ready")?;
    let mut input = Vec::new();
    io::stdin().lock().read_until(b'\n', &mut input)?;
    recorder.bytes("stdin", &input)?;

    let mut stdout = io::stdout().lock();
    stdout.write_all(b"fake-stdout:")?;
    stdout.write_all(&input)?;
    stdout.flush()?;

    let mut stderr = io::stderr().lock();
    stderr.write_all(b"fake-stderr:")?;
    stderr.write_all(&input)?;
    stderr.flush()
}

fn spawn_descendant(record_path: &Path, behavior: &str) -> io::Result<()> {
    Command::new(env::current_exe()?)
        .env(RECORD_ENV, record_path)
        .env(BEHAVIOR_ENV, behavior)
        .env_remove(DESCENDANT_RECORD_ENV)
        .stdin(Stdio::inherit())
        .stdout(Stdio::inherit())
        .stderr(Stdio::inherit())
        .spawn()
        .map(|_| ())
}

#[cfg(unix)]
fn wait_for_interrupt(recorder: &Recorder, exit_after: Option<usize>) -> io::Result<()> {
    use signal_hook::consts::signal::{SIGINT, SIGTERM};
    use signal_hook::iterator::Signals;

    let mut signals = Signals::new([SIGINT, SIGTERM])?;
    recorder.event("ready")?;
    for (index, signal) in (&mut signals).into_iter().enumerate() {
        recorder.event(&format!("signal:{signal}"))?;
        if exit_after.is_some_and(|limit| index + 1 >= limit) {
            return Ok(());
        }
    }
    Err(io::Error::new(
        io::ErrorKind::BrokenPipe,
        "signal iterator closed before an interrupt arrived",
    ))
}

#[cfg(windows)]
fn wait_for_interrupt(recorder: &Recorder, exit_after: Option<usize>) -> io::Result<()> {
    use std::sync::mpsc;

    let (sender, receiver) = mpsc::channel();
    ctrlc::try_set_handler(move || {
        let _ = sender.send(());
    })
    .map_err(|error| io::Error::other(error.to_string()))?;
    recorder.event("ready")?;
    for (index, ()) in receiver.into_iter().enumerate() {
        recorder.event("console")?;
        if exit_after.is_some_and(|limit| index + 1 >= limit) {
            return Ok(());
        }
    }
    Err(io::Error::new(
        io::ErrorKind::BrokenPipe,
        "console event channel closed before an interrupt arrived",
    ))
}

struct Recorder {
    path: PathBuf,
}

impl Recorder {
    fn create(path: PathBuf) -> io::Result<Self> {
        fs::write(&path, b"SMFAKE1\n")?;
        Ok(Self { path })
    }

    fn os(&self, name: &str, value: &OsStr) -> io::Result<()> {
        self.bytes(name, &os_bytes(value))
    }

    fn event(&self, event: &str) -> io::Result<()> {
        self.append(&format!("event={event}\n"))
    }

    fn number(&self, name: &str, value: u32) -> io::Result<()> {
        self.append(&format!("{name}={value}\n"))
    }

    fn bytes(&self, name: &str, value: &[u8]) -> io::Result<()> {
        self.append(&format!("{name}={}\n", hex(value)))
    }

    fn append(&self, value: &str) -> io::Result<()> {
        OpenOptions::new()
            .append(true)
            .open(&self.path)?
            .write_all(value.as_bytes())
    }
}

#[cfg(unix)]
fn os_bytes(value: &OsStr) -> Vec<u8> {
    use std::os::unix::ffi::OsStrExt;

    value.as_bytes().to_vec()
}

#[cfg(windows)]
fn os_bytes(value: &OsStr) -> Vec<u8> {
    use std::os::windows::ffi::OsStrExt;

    value.encode_wide().flat_map(u16::to_le_bytes).collect()
}

fn hex(value: &[u8]) -> String {
    use std::fmt::Write as _;

    value.iter().fold(
        String::with_capacity(value.len() * 2),
        |mut encoded, byte| {
            write!(encoded, "{byte:02x}").expect("writing into a String cannot fail");
            encoded
        },
    )
}

fn required_path(name: &str) -> io::Result<PathBuf> {
    env::var_os(name).map(PathBuf::from).ok_or_else(|| {
        io::Error::new(
            io::ErrorKind::InvalidInput,
            format!("required environment variable {name} is absent"),
        )
    })
}

fn invalid_data(error: impl std::error::Error + Send + Sync + 'static) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidData, error)
}
