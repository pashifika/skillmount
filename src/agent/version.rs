//! Bounded, advisory Agent version observation.
//!
//! Version banners are dated compatibility evidence, not launch authorization. This module owns
//! the single shell-free observation and its diagnostics; it has no access to mount plans, locks,
//! journals, transactions, or cleanup identity.

#[cfg(debug_assertions)]
use std::ffi::OsStr;
use std::io::{self, Read};
use std::path::Path;
use std::process::{Command, Stdio};

use crate::render::{path_value, text_value};

/// Maximum bytes retained from either output stream of an Agent `--version` process.
const VERSION_OUTPUT_LIMIT: usize = 1024;

/// Agent-specific evidence needed by the shared observer.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct VersionSpec {
    display_name: &'static str,
    last_tested_banner: &'static str,
    debug_override: &'static str,
}

impl VersionSpec {
    /// Creates one immutable Agent evidence description.
    pub(crate) const fn new(
        display_name: &'static str,
        last_tested_banner: &'static str,
        debug_override: &'static str,
    ) -> Self {
        Self {
            display_name,
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
    /// Returns the stable evidence class used by sessions and doctor.
    pub(crate) const fn kind(&self) -> VersionEvidenceKind {
        match self {
            Self::LastTested { .. } => VersionEvidenceKind::LastTested,
            Self::Untested { .. } => VersionEvidenceKind::Untested,
            Self::Unavailable { .. } => VersionEvidenceKind::Unavailable,
        }
    }

    /// Renders the one advisory session warning, if this is not last-tested evidence.
    pub(crate) fn session_warning(&self, executable: &Path) -> Option<String> {
        let executable = path_value(executable, true);
        match self {
            Self::LastTested { .. } => None,
            Self::Untested { spec, banner } => Some(format!(
                "{} version compatibility is unverified: {executable} reported {:?}, while the last-tested banner is {:?}; continuing because version evidence is advisory. Review docs/compatibility.md and run the opt-in live-agent smoke before claiming compatibility",
                spec.display_name,
                text_value(banner),
                spec.last_tested_banner,
            )),
            Self::Unavailable { spec, reason } => Some(format!(
                "{} version compatibility is unverified: {executable} did not provide usable --version evidence ({}); the last-tested banner is {:?}; continuing because version evidence is advisory. Review docs/compatibility.md and run the opt-in live-agent smoke before claiming compatibility",
                spec.display_name,
                text_value(reason),
                spec.last_tested_banner,
            )),
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

fn capture(executable: &Path, invocation_cwd: &Path) -> Result<CapturedVersion, String> {
    let mut child = Command::new(executable)
        .arg("--version")
        .current_dir(invocation_cwd)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .map_err(|error| format!("cannot start --version: {error}"))?;
    let stdout = child
        .stdout
        .take()
        .ok_or_else(|| "cannot capture --version standard output".to_owned())?;
    let stderr = child
        .stderr
        .take()
        .ok_or_else(|| "cannot capture --version standard error".to_owned())?;

    let (status, stdout, stderr) = std::thread::scope(|scope| {
        let stdout = scope.spawn(move || read_bounded(stdout));
        let stderr = scope.spawn(move || read_bounded(stderr));
        let status = child.wait();
        (status, stdout.join(), stderr.join())
    });
    let status = status.map_err(|error| format!("cannot wait for --version: {error}"))?;
    let success = status.success();
    let status = status
        .code()
        .map_or_else(|| status.to_string(), |code| format!("exit code {code}"));
    let stdout = stdout
        .map_err(|_| "the --version stdout reader stopped unexpectedly".to_owned())?
        .map_err(|error| format!("cannot read --version standard output: {error}"))?;
    let stderr = stderr
        .map_err(|_| "the --version stderr reader stopped unexpectedly".to_owned())?
        .map_err(|error| format!("cannot read --version standard error: {error}"))?;

    Ok(CapturedVersion {
        success,
        status,
        stdout,
        stderr,
    })
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
    io::copy(&mut reader, &mut io::sink())?;
    Ok(BoundedBytes { bytes, exceeded })
}

fn classify_capture(spec: VersionSpec, output: CapturedVersion) -> VersionObservation {
    if !output.success {
        return VersionObservation::Unavailable {
            spec,
            reason: format!("--version exited with {}", output.status),
        };
    }
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

    const SPEC: VersionSpec = VersionSpec::new(
        "Fixture Agent",
        "fixture-agent 1.0.0",
        "SKILLMOUNT_UNUSED_VERSION_OVERRIDE",
    );

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
        assert!(exact.session_warning(Path::new("fixture-agent")).is_none());
        assert_eq!(different.kind(), VersionEvidenceKind::Untested);
        let warning = different
            .session_warning(Path::new("fixture-agent"))
            .expect("untested evidence warns");
        assert!(warning.contains("fixture-agent 1.1.0"));
        assert!(warning.contains("fixture-agent 1.0.0"));
        assert!(warning.contains("docs/compatibility.md"));
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
        let warning = observation
            .session_warning(&missing)
            .expect("unavailable evidence warns");
        assert!(warning.contains("cannot start --version"));
        assert!(warning.contains("last-tested"));
    }

    #[test]
    fn oversized_output_is_drained_but_never_retained_past_the_bound() {
        let bounded = read_bounded(Cursor::new(vec![b'x'; VERSION_OUTPUT_LIMIT + 512]))
            .expect("bounded read");
        assert!(bounded.exceeded);
        assert_eq!(bounded.bytes.len(), VERSION_OUTPUT_LIMIT);

        let observation = classify_capture(
            SPEC,
            CapturedVersion {
                success: true,
                status: "exit status: 0".to_owned(),
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
        let warning = observation
            .session_warning(Path::new("fixture-agent"))
            .expect("invalid evidence warns");
        assert!(warning.contains("not valid UTF-8"));
        assert!(!warning.contains(char::REPLACEMENT_CHARACTER));
    }

    #[test]
    fn warning_rendering_escapes_controls_and_is_bounded_by_captured_output() {
        let banner = "\u{1b}\n".repeat(VERSION_OUTPUT_LIMIT / 2);
        let observation = VersionObservation::Untested { spec: SPEC, banner };

        let warning = observation
            .session_warning(Path::new("fixture-agent"))
            .expect("untested evidence warns");

        assert!(!warning.contains('\u{1b}'));
        assert!(!warning.contains('\n'));
        assert!(warning.len() < VERSION_OUTPUT_LIMIT * 8);
        assert!(warning.contains("fixture-agent 1.0.0"));
    }
}
