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
const EXPECT_PATHS_ENV: &str = "SKILLMOUNT_FAKE_EXPECT_PATHS";
const CREATE_FILE_ENV: &str = "SKILLMOUNT_FAKE_CREATE_FILE";
const RECORD_CODEX_HOME_ENV: &str = "SKILLMOUNT_FAKE_RECORD_CODEX_HOME";
const VERSION_RECORD_ENV: &str = "SKILLMOUNT_FAKE_VERSION_RECORD";
const UNSUPPORTED_VERSION_AT_ENV: &str = "SKILLMOUNT_FAKE_UNSUPPORTED_VERSION_AT";
const CREATE_PLUGIN_MANIFEST_AT_ENV: &str = "SKILLMOUNT_FAKE_CREATE_PLUGIN_MANIFEST_AT";
const PLUGIN_MANIFEST_PATH_ENV: &str = "SKILLMOUNT_FAKE_PLUGIN_MANIFEST_PATH";

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
    if env::args_os().skip(1).eq([OsStr::new("--version")]) {
        return report_version();
    }
    let record_path = required_path(RECORD_ENV)?;
    let recorder = Recorder::create(record_path)?;
    recorder.number("pid", std::process::id())?;
    for argument in env::args_os().skip(1) {
        recorder.os("arg", &argument)?;
    }
    if let (Some(_), Some(codex_home)) = (
        env::var_os(RECORD_CODEX_HOME_ENV),
        env::var_os("CODEX_HOME"),
    ) {
        recorder.os("env:CODEX_HOME", &codex_home)?;
    }
    recorder.os("cwd", &env::current_dir()?.into_os_string())?;
    verify_expected_paths(&recorder)?;
    create_requested_file(&recorder)?;

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

fn report_version() -> io::Result<ExitCode> {
    let probe = if let Some(path) = env::var_os(VERSION_RECORD_ENV).map(PathBuf::from) {
        let prior = match fs::read_to_string(&path) {
            Ok(contents) => contents.lines().count(),
            Err(error) if error.kind() == io::ErrorKind::NotFound => 0,
            Err(error) => return Err(error),
        };
        OpenOptions::new()
            .create(true)
            .append(true)
            .open(path)?
            .write_all(b"probe\n")?;
        prior + 1
    } else {
        1
    };
    let unsupported_at = env::var(UNSUPPORTED_VERSION_AT_ENV)
        .ok()
        .and_then(|value| value.parse::<usize>().ok());
    let create_plugin_manifest_at = env::var(CREATE_PLUGIN_MANIFEST_AT_ENV)
        .ok()
        .and_then(|value| value.parse::<usize>().ok());
    if create_plugin_manifest_at == Some(probe) {
        let manifest = required_path(PLUGIN_MANIFEST_PATH_ENV)?;
        fs::create_dir_all(manifest.parent().ok_or_else(|| {
            io::Error::new(io::ErrorKind::InvalidInput, "plugin manifest has no parent")
        })?)?;
        fs::write(manifest, br#"{"name":"late-plugin"}"#)?;
    }
    if unsupported_at == Some(probe) {
        println!("codex-cli 0.147.0");
    } else {
        println!("codex-cli 0.146.0");
    }
    Ok(ExitCode::SUCCESS)
}

fn create_requested_file(recorder: &Recorder) -> io::Result<()> {
    let Some(path) = env::var_os(CREATE_FILE_ENV).map(PathBuf::from) else {
        return Ok(());
    };
    fs::write(&path, b"created by fake agent\n")?;
    recorder.os("created", path.as_os_str())
}

fn verify_expected_paths(recorder: &Recorder) -> io::Result<()> {
    let Some(encoded) = env::var_os(EXPECT_PATHS_ENV) else {
        return Ok(());
    };
    for path in env::split_paths(&encoded) {
        let metadata = fs::metadata(&path)?;
        if !metadata.is_dir() {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                format!("expected visible directory {}", path.display()),
            ));
        }
        recorder.os("visible", path.as_os_str())?;
        recorder.os("visible-target", fs::canonicalize(&path)?.as_os_str())?;
    }
    Ok(())
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
