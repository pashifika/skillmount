//! Bounded, advisory Agent version observation.
//!
//! Version banners are dated compatibility evidence, not launch authorization. This module owns
//! the single shell-free observation and its diagnostics; it has no access to mount plans, locks,
//! journals, transactions, or cleanup identity.

#[cfg(debug_assertions)]
use std::ffi::OsStr;
use std::io::{self, Read};
use std::path::Path;
use std::process::{Child, Command, Stdio};
use std::sync::mpsc::{self, Receiver, RecvTimeoutError, Sender};
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};

use crate::process::CaptureDomain;
use crate::render::{path_value, text_value};

/// Maximum bytes retained from either output stream of an Agent `--version` process.
const VERSION_OUTPUT_LIMIT: usize = 1024;
/// Maximum wall-clock lifetime of an Agent `--version` process and its inherited output handles.
const VERSION_OBSERVATION_TIMEOUT: Duration = Duration::from_secs(3);
/// Grace between captured-stream closure and forceful process-domain finalization.
const VERSION_EXIT_GRACE: Duration = Duration::from_millis(25);
/// Maximum time allowed for force, root reaping, reader shutdown, and domain-death proof.
const VERSION_SHUTDOWN_TIMEOUT: Duration = Duration::from_secs(1);
const CAPTURE_POLL_INTERVAL: Duration = Duration::from_millis(10);

/// Agent-specific evidence needed by the shared observer.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct VersionSpec {
    last_tested_banner: &'static str,
    debug_override: &'static str,
}

impl VersionSpec {
    /// Creates one immutable Agent evidence description.
    pub(crate) const fn new(
        last_tested_banner: &'static str,
        debug_override: &'static str,
    ) -> Self {
        Self {
            last_tested_banner,
            debug_override,
        }
    }

    /// Returns the exact banner attached to the adapter's last-tested evidence.
    pub(crate) const fn last_tested_banner(self) -> &'static str {
        self.last_tested_banner
    }
}

/// Stable classification of one ephemeral version observation.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum VersionEvidenceKind {
    /// The observed banner exactly matches the last-tested evidence.
    LastTested,
    /// A bounded UTF-8 banner was observed but has no matching live evidence.
    Untested,
    /// No bounded, successful UTF-8 banner was available.
    Unavailable,
}

/// One Agent banner observation, detached from transaction ownership state.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum VersionObservation {
    /// Exact last-tested evidence was observed.
    LastTested { spec: VersionSpec, banner: String },
    /// Another bounded UTF-8 banner was observed.
    Untested { spec: VersionSpec, banner: String },
    /// The process could not provide usable version evidence.
    Unavailable { spec: VersionSpec, reason: String },
}

impl VersionObservation {
    /// Returns the stable evidence class used by doctor.
    pub(crate) const fn kind(&self) -> VersionEvidenceKind {
        match self {
            Self::LastTested { .. } => VersionEvidenceKind::LastTested,
            Self::Untested { .. } => VersionEvidenceKind::Untested,
            Self::Unavailable { .. } => VersionEvidenceKind::Unavailable,
        }
    }

    /// Renders the doctor detail without changing its severity policy.
    pub(crate) fn doctor_detail(&self, executable: &Path) -> String {
        let executable = path_value(executable, true);
        match self {
            Self::LastTested { spec: _, banner } => format!(
                "{executable} reports {:?}, matching the last-tested evidence",
                text_value(banner)
            ),
            Self::Untested { spec, banner } => format!(
                "{executable} reports {:?}; the last-tested banner is {:?}. Compatibility is unverified; run the live-agent smoke and record it in docs/compatibility.md",
                text_value(banner),
                spec.last_tested_banner,
            ),
            Self::Unavailable { spec, reason } => format!(
                "{executable} did not provide usable --version evidence ({}); the last-tested banner is {:?}. Compatibility is unverified; run the live-agent smoke and record it in docs/compatibility.md",
                text_value(reason),
                spec.last_tested_banner,
            ),
        }
    }
}

/// Runs one shell-free, bounded `--version` observation in the invocation directory.
pub(crate) fn observe(
    executable: &Path,
    invocation_cwd: &Path,
    spec: VersionSpec,
) -> VersionObservation {
    #[cfg(debug_assertions)]
    if let Some(value) = std::env::var_os(spec.debug_override) {
        return classify_override(spec, &value);
    }

    match capture(executable, invocation_cwd) {
        Ok(output) => classify_capture(spec, output),
        Err(reason) => VersionObservation::Unavailable { spec, reason },
    }
}

#[cfg(debug_assertions)]
fn classify_override(spec: VersionSpec, value: &OsStr) -> VersionObservation {
    let Some(value) = value.to_str() else {
        return VersionObservation::Unavailable {
            spec,
            reason: "the deterministic version override is not valid UTF-8".to_owned(),
        };
    };
    if value.len() > VERSION_OUTPUT_LIMIT {
        return VersionObservation::Unavailable {
            spec,
            reason: format!(
                "the deterministic version override exceeds the {VERSION_OUTPUT_LIMIT}-byte observation bound"
            ),
        };
    }
    classify_banner(spec, value.trim().to_owned())
}

#[derive(Debug)]
struct CapturedVersion {
    success: bool,
    status: String,
    stdout: BoundedBytes,
    stderr: BoundedBytes,
}

#[derive(Debug)]
struct BoundedBytes {
    bytes: Vec<u8>,
    exceeded: bool,
}

#[derive(Debug, Clone, Copy)]
enum CapturedStream {
    Stdout,
    Stderr,
}

type ReaderMessage = (CapturedStream, io::Result<BoundedBytes>);

struct RunningCapture {
    domain: CaptureDomain,
    child: Child,
    receiver: Receiver<ReaderMessage>,
    _stdout_reader: JoinHandle<()>,
    _stderr_reader: JoinHandle<()>,
    stdout: Option<io::Result<BoundedBytes>>,
    stderr: Option<io::Result<BoundedBytes>>,
}

impl RunningCapture {
    fn start(executable: &Path, invocation_cwd: &Path) -> Result<Self, String> {
        let mut command = Command::new(executable);
        command
            .arg("--version")
            .current_dir(invocation_cwd)
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
        let mut domain = CaptureDomain::prepare(&mut command)
            .map_err(|error| format!("cannot prepare bounded --version containment: {error}"))?;
        let mut child = command
            .spawn()
            .map_err(|error| format!("cannot start --version: {error}"))?;
        if let Err(error) = domain.attach(&child) {
            stop_after_setup_failure(&mut domain, &mut child);
            return Err(format!(
                "cannot attach --version to bounded process containment: {error}"
            ));
        }
        let Some(stdout) = child.stdout.take() else {
            stop_after_setup_failure(&mut domain, &mut child);
            return Err("cannot capture --version standard output".to_owned());
        };
        let Some(stderr) = child.stderr.take() else {
            stop_after_setup_failure(&mut domain, &mut child);
            return Err("cannot capture --version standard error".to_owned());
        };

        let (sender, receiver) = mpsc::channel();
        let stdout_reader = match spawn_reader(
            "skillmount-version-stdout",
            CapturedStream::Stdout,
            stdout,
            sender.clone(),
        ) {
            Ok(reader) => reader,
            Err(error) => {
                stop_after_setup_failure(&mut domain, &mut child);
                return Err(format!(
                    "cannot start the --version standard-output reader: {error}"
                ));
            }
        };
        let stderr_reader = match spawn_reader(
            "skillmount-version-stderr",
            CapturedStream::Stderr,
            stderr,
            sender.clone(),
        ) {
            Ok(reader) => reader,
            Err(error) => {
                stop_after_setup_failure(&mut domain, &mut child);
                drop(stdout_reader);
                return Err(format!(
                    "cannot start the --version standard-error reader: {error}"
                ));
            }
        };
        drop(sender);

        Ok(Self {
            domain,
            child,
            receiver,
            _stdout_reader: stdout_reader,
            _stderr_reader: stderr_reader,
            stdout: None,
            stderr: None,
        })
    }

    fn await_output_bound(&mut self) -> bool {
        let deadline = Instant::now() + VERSION_OBSERVATION_TIMEOUT;
        while self.stdout.is_none() || self.stderr.is_none() {
            if reader_requires_stop(self.stdout.as_ref())
                || reader_requires_stop(self.stderr.as_ref())
            {
                return false;
            }
            let remaining = deadline.saturating_duration_since(Instant::now());
            if remaining.is_zero() {
                return true;
            }
            match self.receiver.recv_timeout(remaining) {
                Ok(message) => self.store_reader_result(message),
                Err(RecvTimeoutError::Timeout) => return true,
                Err(RecvTimeoutError::Disconnected) => return false,
            }
        }

        if reader_completed_within_bound(self.stdout.as_ref())
            && reader_completed_within_bound(self.stderr.as_ref())
        {
            thread::sleep(VERSION_EXIT_GRACE);
        }
        false
    }

    fn finish(mut self, lifetime_exceeded: bool) -> Result<CapturedVersion, String> {
        let termination_error = self.domain.terminate(&mut self.child).err();
        let shutdown_deadline = Instant::now() + VERSION_SHUTDOWN_TIMEOUT;
        let mut status = None;
        let mut wait_error = None;
        let mut domain_empty = false;
        let mut domain_error = None;
        while Instant::now() < shutdown_deadline {
            while let Ok(message) = self.receiver.try_recv() {
                self.store_reader_result(message);
            }
            if status.is_none() && wait_error.is_none() {
                match self.child.try_wait() {
                    Ok(Some(observed)) => {
                        self.domain.mark_root_reaped();
                        status = Some(observed);
                    }
                    Ok(None) => {}
                    Err(error) => wait_error = Some(error),
                }
            }
            if status.is_some() && !domain_empty && domain_error.is_none() {
                match self.domain.is_empty() {
                    Ok(empty) => domain_empty = empty,
                    Err(error) => domain_error = Some(error),
                }
            }
            if status.is_some() && domain_empty && self.stdout.is_some() && self.stderr.is_some() {
                break;
            }
            let remaining = shutdown_deadline.saturating_duration_since(Instant::now());
            thread::sleep(remaining.min(CAPTURE_POLL_INTERVAL));
        }
        while let Ok(message) = self.receiver.try_recv() {
            self.store_reader_result(message);
        }

        if let Some(error) = termination_error {
            return Err(format!(
                "cannot terminate the bounded --version process domain: {error}"
            ));
        }
        if let Some(error) = wait_error {
            return Err(format!("cannot wait for --version: {error}"));
        }
        let Some(status) = status else {
            return Err(format!(
                "cannot reap --version within the {}-second shutdown bound",
                VERSION_SHUTDOWN_TIMEOUT.as_secs()
            ));
        };
        if let Some(error) = domain_error {
            return Err(format!(
                "cannot prove the --version process domain is empty: {error}"
            ));
        }
        if !domain_empty {
            return Err(format!(
                "the --version process domain did not become empty within the {}-second shutdown bound",
                VERSION_SHUTDOWN_TIMEOUT.as_secs()
            ));
        }
        if self.stdout.is_none() || self.stderr.is_none() {
            return Err(format!(
                "the --version output readers did not stop within the {}-second shutdown bound",
                VERSION_SHUTDOWN_TIMEOUT.as_secs()
            ));
        }
        if lifetime_exceeded {
            return Err(format!(
                "--version did not complete within the {}-second process/output lifetime bound",
                VERSION_OBSERVATION_TIMEOUT.as_secs()
            ));
        }

        let stdout = self
            .stdout
            .take()
            .expect("the stdout reader result was checked")
            .map_err(|error| format!("cannot read --version standard output: {error}"))?;
        let stderr = self
            .stderr
            .take()
            .expect("the stderr reader result was checked")
            .map_err(|error| format!("cannot read --version standard error: {error}"))?;
        let success = status.success();
        let status = status
            .code()
            .map_or_else(|| status.to_string(), |code| format!("exit code {code}"));
        Ok(CapturedVersion {
            success,
            status,
            stdout,
            stderr,
        })
    }

    fn store_reader_result(&mut self, (stream, result): ReaderMessage) {
        match stream {
            CapturedStream::Stdout => self.stdout = Some(result),
            CapturedStream::Stderr => self.stderr = Some(result),
        }
    }
}

fn capture(executable: &Path, invocation_cwd: &Path) -> Result<CapturedVersion, String> {
    let mut capture = RunningCapture::start(executable, invocation_cwd)?;
    let lifetime_exceeded = capture.await_output_bound();
    capture.finish(lifetime_exceeded)
}

fn spawn_reader(
    name: &str,
    stream: CapturedStream,
    reader: impl Read + Send + 'static,
    sender: Sender<ReaderMessage>,
) -> io::Result<JoinHandle<()>> {
    thread::Builder::new().name(name.to_owned()).spawn(move || {
        let _ = sender.send((stream, read_bounded(reader)));
    })
}

fn reader_requires_stop(result: Option<&io::Result<BoundedBytes>>) -> bool {
    match result {
        Some(Ok(bytes)) => bytes.exceeded,
        Some(Err(_)) => true,
        None => false,
    }
}

fn reader_completed_within_bound(result: Option<&io::Result<BoundedBytes>>) -> bool {
    matches!(result, Some(Ok(bytes)) if !bytes.exceeded)
}

fn stop_after_setup_failure(domain: &mut CaptureDomain, child: &mut Child) {
    let _ = domain.terminate(child);
    let deadline = Instant::now() + VERSION_SHUTDOWN_TIMEOUT;
    let mut root_reaped = false;
    while Instant::now() < deadline {
        if !root_reaped {
            match child.try_wait() {
                Ok(Some(_)) => {
                    domain.mark_root_reaped();
                    root_reaped = true;
                }
                Ok(None) => {}
                Err(_) => return,
            }
        }
        if root_reaped {
            match domain.is_empty() {
                Ok(true) | Err(_) => return,
                Ok(false) => {}
            }
        }
        thread::sleep(CAPTURE_POLL_INTERVAL);
    }
}

fn read_bounded(mut reader: impl Read) -> io::Result<BoundedBytes> {
    let mut bytes = Vec::with_capacity(VERSION_OUTPUT_LIMIT + 1);
    reader
        .by_ref()
        .take(u64::try_from(VERSION_OUTPUT_LIMIT + 1).unwrap_or(u64::MAX))
        .read_to_end(&mut bytes)?;
    let exceeded = bytes.len() > VERSION_OUTPUT_LIMIT;
    if exceeded {
        bytes.truncate(VERSION_OUTPUT_LIMIT);
    }
    Ok(BoundedBytes { bytes, exceeded })
}

fn classify_capture(spec: VersionSpec, output: CapturedVersion) -> VersionObservation {
    if output.stdout.exceeded || output.stderr.exceeded {
        let stream = match (output.stdout.exceeded, output.stderr.exceeded) {
            (true, true) => "standard output and standard error",
            (true, false) => "standard output",
            (false, true) => "standard error",
            (false, false) => unreachable!("an exceeded stream was already proved"),
        };
        return VersionObservation::Unavailable {
            spec,
            reason: format!(
                "--version {stream} exceeds the {VERSION_OUTPUT_LIMIT}-byte observation bound"
            ),
        };
    }
    if !output.success {
        return VersionObservation::Unavailable {
            spec,
            reason: format!("--version exited with {}", output.status),
        };
    }
    let Ok(banner) = String::from_utf8(output.stdout.bytes) else {
        return VersionObservation::Unavailable {
            spec,
            reason: "--version standard output is not valid UTF-8".to_owned(),
        };
    };
    classify_banner(spec, banner.trim().to_owned())
}

fn classify_banner(spec: VersionSpec, banner: String) -> VersionObservation {
    if banner == spec.last_tested_banner {
        VersionObservation::LastTested { spec, banner }
    } else {
        VersionObservation::Untested { spec, banner }
    }
}

#[cfg(test)]
mod tests {
    use std::io::Cursor;
    use std::path::Path;

    use super::{
        BoundedBytes, CapturedVersion, VERSION_OUTPUT_LIMIT, VersionEvidenceKind,
        VersionObservation, VersionSpec, classify_capture, observe, read_bounded,
    };
    use crate::test_support::{TestDir, assert_no_side_effects};

    const SPEC: VersionSpec =
        VersionSpec::new("fixture-agent 1.0.0", "SKILLMOUNT_UNUSED_VERSION_OVERRIDE");

    fn captured(stdout: Vec<u8>) -> CapturedVersion {
        CapturedVersion {
            success: true,
            status: "exit status: 0".to_owned(),
            stdout: BoundedBytes {
                bytes: stdout,
                exceeded: false,
            },
            stderr: BoundedBytes {
                bytes: Vec::new(),
                exceeded: false,
            },
        }
    }

    #[test]
    fn exact_and_different_banners_have_distinct_evidence_outcomes() {
        let exact = classify_capture(SPEC, captured(b"fixture-agent 1.0.0\n".to_vec()));
        let different = classify_capture(SPEC, captured(b"fixture-agent 1.1.0\n".to_vec()));

        assert_eq!(exact.kind(), VersionEvidenceKind::LastTested);
        assert!(
            exact
                .doctor_detail(Path::new("fixture-agent"))
                .contains("matching the last-tested evidence")
        );
        assert_eq!(different.kind(), VersionEvidenceKind::Untested);
        let detail = different.doctor_detail(Path::new("fixture-agent"));
        assert!(detail.contains("fixture-agent 1.1.0"));
        assert!(detail.contains("fixture-agent 1.0.0"));
        assert!(detail.contains("docs/compatibility.md"));
    }

    #[test]
    fn nonzero_status_is_unavailable_evidence() {
        let mut output = captured(Vec::new());
        output.success = false;
        output.status = "exit code 7".to_owned();

        let observation = classify_capture(SPEC, output);

        assert_eq!(observation.kind(), VersionEvidenceKind::Unavailable);
        assert!(
            observation
                .doctor_detail(Path::new("fixture-agent"))
                .contains("exit code 7")
        );
    }

    #[test]
    fn spawn_failure_is_advisory_and_creates_no_ownership_state() {
        let fixture = TestDir::new("version-observer-spawn-failure");
        let missing = fixture.path().join("missing-agent");

        let observation = assert_no_side_effects(&[fixture.path()], || {
            observe(&missing, fixture.path(), SPEC)
        });

        assert_eq!(observation.kind(), VersionEvidenceKind::Unavailable);
        let detail = observation.doctor_detail(&missing);
        assert!(detail.contains("cannot start --version"));
        assert!(detail.contains("last-tested"));
    }

    #[test]
    fn oversized_output_closes_the_reader_and_outweighs_forced_status() {
        let mut source = Cursor::new(vec![b'x'; VERSION_OUTPUT_LIMIT + 512]);
        let bounded = read_bounded(&mut source).expect("bounded read");
        assert!(bounded.exceeded);
        assert_eq!(bounded.bytes.len(), VERSION_OUTPUT_LIMIT);
        assert_eq!(
            source.position(),
            u64::try_from(VERSION_OUTPUT_LIMIT + 1).expect("bound fits u64")
        );

        let observation = classify_capture(
            SPEC,
            CapturedVersion {
                success: false,
                status: "signal: 9 (SIGKILL)".to_owned(),
                stdout: bounded,
                stderr: BoundedBytes {
                    bytes: Vec::new(),
                    exceeded: false,
                },
            },
        );
        assert_eq!(observation.kind(), VersionEvidenceKind::Unavailable);
        assert!(
            observation
                .doctor_detail(Path::new("fixture-agent"))
                .contains("1024-byte observation bound")
        );
    }

    #[test]
    fn invalid_utf8_is_unavailable_without_rendering_agent_bytes() {
        let observation = classify_capture(SPEC, captured(vec![b'f', 0xff, b'o']));

        assert_eq!(observation.kind(), VersionEvidenceKind::Unavailable);
        let detail = observation.doctor_detail(Path::new("fixture-agent"));
        assert!(detail.contains("not valid UTF-8"));
        assert!(!detail.contains(char::REPLACEMENT_CHARACTER));
    }

    #[test]
    fn detail_rendering_escapes_controls_and_is_bounded_by_captured_output() {
        let banner = "\u{1b}\n".repeat(VERSION_OUTPUT_LIMIT / 2);
        let observation = VersionObservation::Untested { spec: SPEC, banner };

        let detail = observation.doctor_detail(Path::new("fixture-agent"));

        assert!(!detail.contains('\u{1b}'));
        assert!(!detail.contains('\n'));
        assert!(detail.len() < VERSION_OUTPUT_LIMIT * 8);
        assert!(detail.contains("fixture-agent 1.0.0"));
    }
}
