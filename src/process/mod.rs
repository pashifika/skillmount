//! Shell-free child launch, interruption, cleanup coordination, and exit mapping.

use std::ffi::OsString;
use std::io;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::thread;
use std::time::Duration;

use crate::error::ExitCategory;
use crate::mount::LaunchPlan;

#[cfg(unix)]
mod unix;
#[cfg(unix)]
use unix as platform;
#[cfg(windows)]
mod windows;
#[cfg(windows)]
use windows as platform;
#[cfg(windows)]
mod windows_ffi;

#[cfg(not(any(unix, windows)))]
compile_error!("child process supervision supports only Unix and Windows targets");

const POLL_INTERVAL: Duration = Duration::from_millis(5);

/// Native helpers exposed only to feature-gated integration fixtures.
#[cfg(all(windows, feature = "test-fixtures"))]
pub mod test_support {
    use std::io;

    /// Sends `CTRL_BREAK_EVENT` to a console process group created by a test controller.
    ///
    /// # Errors
    ///
    /// Returns the operating-system error when the console or target group is unavailable.
    pub fn send_console_break(process_group_id: u32) -> io::Result<()> {
        super::windows_ffi::generate_console_break(process_group_id)
    }
}

/// A completed child-launch request.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SupervisionRequest {
    launch: LaunchPlan,
}

impl SupervisionRequest {
    /// Creates a request from the launch description produced by an agent adapter.
    #[must_use]
    pub const fn new(launch: LaunchPlan) -> Self {
        Self { launch }
    }

    /// Returns the platform-native launch description.
    #[must_use]
    pub const fn launch(&self) -> &LaunchPlan {
        &self.launch
    }
}

/// A single-use shell-free child lifecycle boundary.
#[derive(Debug, Default)]
pub struct ProcessSupervisor {
    _single_use: (),
}

impl ProcessSupervisor {
    /// Creates a supervisor for one child lifecycle.
    #[must_use]
    pub const fn new() -> Self {
        Self { _single_use: () }
    }

    /// Launches one child, waits through supported interrupts, and invokes cleanup exactly once.
    ///
    /// The child inherits all three standard streams. Tests that need captured evidence must
    /// redirect the process running the supervisor, not alter this launch boundary.
    pub fn supervise<F>(self, request: SupervisionRequest, cleanup: F) -> SupervisionOutcome
    where
        F: FnOnce() -> CleanupOutcome,
    {
        let Self { _single_use: () } = self;
        let mut cleanup = CleanupGuard::new(cleanup);
        let launch = request.launch;
        let mut platform = match platform::Platform::install() {
            Ok(platform) => platform,
            Err(error) => {
                return finish(
                    &mut cleanup,
                    ChildOutcome::Failed(ProcessFailure::from_io(
                        ProcessStage::InterruptSetup,
                        &launch.executable,
                        &error,
                    )),
                    InterruptPath::None,
                );
            }
        };

        let mut command = Command::new(&launch.executable);
        command
            .args(&launch.injected_args)
            .args(&launch.passthrough_args)
            .current_dir(&launch.cwd)
            .stdin(Stdio::inherit())
            .stdout(Stdio::inherit())
            .stderr(Stdio::inherit());
        platform.configure_command(&mut command);

        let mut child = match command.spawn() {
            Ok(child) => child,
            Err(error) => {
                return finish(
                    &mut cleanup,
                    ChildOutcome::Failed(ProcessFailure::from_io(
                        ProcessStage::Spawn,
                        &launch.executable,
                        &error,
                    )),
                    InterruptPath::None,
                );
            }
        };

        let (child, interrupt) = wait_for_child(&mut child, &mut platform, &launch.executable);
        finish(&mut cleanup, child, interrupt)
    }
}

/// The process operation that failed.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ProcessStage {
    /// Installing safe parent interrupt observation before spawn.
    InterruptSetup,
    /// Starting the child executable.
    Spawn,
    /// Observing the final child status.
    Wait,
    /// Relaying the first interrupt to the child.
    ForwardInterrupt,
    /// Applying the second-interrupt force path.
    ForceTermination,
}

/// An operating-system failure with enough context for a caller to diagnose the launch.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ProcessFailure {
    stage: ProcessStage,
    executable: PathBuf,
    kind: io::ErrorKind,
    raw_os_error: Option<i32>,
    reason: String,
}

impl ProcessFailure {
    fn from_io(stage: ProcessStage, executable: &Path, error: &io::Error) -> Self {
        Self {
            stage,
            executable: executable.to_path_buf(),
            kind: error.kind(),
            raw_os_error: error.raw_os_error(),
            reason: error.to_string(),
        }
    }

    /// Returns which process operation failed.
    #[must_use]
    pub const fn stage(&self) -> ProcessStage {
        self.stage
    }

    /// Returns the executable associated with the failed supervision attempt.
    #[must_use]
    pub fn executable(&self) -> &Path {
        &self.executable
    }

    /// Returns the portable I/O error classification.
    #[must_use]
    pub const fn kind(&self) -> io::ErrorKind {
        self.kind
    }

    /// Returns the original platform error code when the operating system supplied one.
    #[must_use]
    pub const fn raw_os_error(&self) -> Option<i32> {
        self.raw_os_error
    }

    /// Returns the operating-system explanation captured at the failure boundary.
    #[must_use]
    pub fn reason(&self) -> &str {
        &self.reason
    }
}

/// A child status interpreted without discarding platform-specific termination evidence.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ChildStatus {
    /// A normal process exit representable by `SkillMount`'s public status range.
    Exited(u8),
    /// A Unix child was terminated by a signal.
    Signaled {
        /// Signal number reported by `wait`.
        signal: i32,
        /// Whether the platform reports that a core file was produced.
        core_dumped: bool,
    },
    /// A Windows status cannot be represented as a normal public process exit code.
    ExceptionalWindows {
        /// Original unsigned status returned by Windows.
        raw_status: u32,
    },
    /// A Unix status was neither a public exit byte nor a reported terminating signal.
    ExceptionalUnix {
        /// Original wait status returned by Unix.
        raw_status: i32,
    },
}

/// Whether the child ran and how its final status was obtained.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ChildOutcome {
    /// The child was reaped and returned a platform status.
    Exited(ChildStatus),
    /// Supervision failed before or while the child was running.
    Failed(ProcessFailure),
}

/// One supported parent interruption source.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum InterruptKind {
    /// Unix `SIGINT` or a Windows console interrupt.
    Interrupt,
    /// Unix `SIGTERM`.
    Terminate,
}

/// What happened when the first interrupt reached the child boundary.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum InterruptDelivery {
    /// The terminal or console delivered the event to the shared foreground group directly.
    DeliveredByPlatform,
    /// The wrapper relayed the event to the child or its dedicated group.
    Forwarded,
    /// The child had already exited before forwarding was necessary.
    ChildAlreadyExited,
    /// The wrapper could not relay the event.
    Failed(ProcessFailure),
}

/// What happened when a second interrupt requested forceful termination.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ForceTermination {
    /// The child or dedicated child group was forcefully terminated.
    Terminated,
    /// The child had already exited before the force path ran.
    ChildAlreadyExited,
    /// The force operation failed.
    Failed(ProcessFailure),
}

/// The interruption path taken during one supervision attempt.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum InterruptPath {
    /// No supported interrupt was observed.
    None,
    /// One interrupt requested graceful child shutdown.
    Graceful {
        /// First observed interrupt.
        interrupt: InterruptKind,
        /// How that interrupt reached the child boundary.
        delivery: InterruptDelivery,
    },
    /// A second interrupt requested the force path.
    Forced {
        /// First observed interrupt.
        first: InterruptKind,
        /// How the first interrupt reached the child boundary.
        delivery: InterruptDelivery,
        /// Second observed interrupt.
        second: InterruptKind,
        /// Result of the force operation.
        termination: ForceTermination,
    },
}

/// Structured evidence retained when orderly cleanup cannot finish.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CleanupFailure {
    /// Human-readable reason for the failed cleanup attempt.
    pub reason: String,
    /// Paths cleanup could not remove or prove safe to remove.
    pub failed_paths: Vec<PathBuf>,
    /// Durable journal retained for a later recovery attempt.
    pub retained_journal: Option<PathBuf>,
    /// Platform-native command arguments the caller can render as recovery guidance.
    pub recovery_command: Vec<OsString>,
}

/// The single orderly cleanup attempt performed after supervision.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CleanupOutcome {
    /// Cleanup completed under the caller's selected retention policy.
    Succeeded,
    /// Cleanup left durable recovery evidence.
    Failed(CleanupFailure),
}

/// Complete observable result of supervising one child and running cleanup once.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SupervisionOutcome {
    /// Final child or process-boundary outcome.
    pub child: ChildOutcome,
    /// Interrupt behavior observed while waiting.
    pub interrupt: InterruptPath,
    /// Result of the one orderly cleanup attempt.
    pub cleanup: CleanupOutcome,
}

/// One primary or secondary operator-facing supervision diagnostic.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SupervisionDiagnostic {
    /// A process operation failed.
    Process(ProcessFailure),
    /// A Windows child returned a status outside the public wrapper range.
    ExceptionalWindowsStatus {
        /// Original unsigned Windows status.
        raw_status: u32,
    },
    /// A Unix signal number could not be mapped to the conventional `128 + signal` range.
    ExceptionalUnixSignal {
        /// Signal number returned by the platform.
        signal: i32,
    },
    /// A Unix wait status could not be represented by the normal exit or signal variants.
    ExceptionalUnixStatus {
        /// Original wait status returned by Unix.
        raw_status: i32,
    },
    /// Cleanup failed and retained recovery evidence.
    Cleanup(CleanupFailure),
}

/// Public exit code plus ordered diagnostics derived from a supervision outcome.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ExitDecision {
    /// Process code returned by the wrapper.
    pub code: u8,
    /// Primary wrapper diagnostic, if the child status alone is not sufficient.
    pub primary: Option<SupervisionDiagnostic>,
    /// Additional failures that must not replace the primary child/process result.
    pub secondary: Vec<SupervisionDiagnostic>,
}

/// Maps a complete supervision outcome to stable wrapper exit behavior.
///
/// A cleanup failure replaces a successful child with exit 73. It never replaces a nonzero child
/// status or another process failure; in that case it remains structured secondary context.
#[must_use]
pub fn map_exit(outcome: &SupervisionOutcome) -> ExitDecision {
    let mut decision = child_exit(&outcome.child);

    for failure in interrupt_failures(&outcome.interrupt) {
        add_failure(
            &mut decision,
            ExitCategory::Internal.code(),
            SupervisionDiagnostic::Process(failure.clone()),
        );
    }

    if let CleanupOutcome::Failed(failure) = &outcome.cleanup {
        add_failure(
            &mut decision,
            ExitCategory::Filesystem.code(),
            SupervisionDiagnostic::Cleanup(failure.clone()),
        );
    }

    decision
}

fn child_exit(outcome: &ChildOutcome) -> ExitDecision {
    match outcome {
        ChildOutcome::Exited(ChildStatus::Exited(code)) => ExitDecision {
            code: *code,
            primary: None,
            secondary: Vec::new(),
        },
        ChildOutcome::Exited(ChildStatus::Signaled { signal, .. }) => {
            let conventional = signal
                .checked_add(128)
                .and_then(|code| u8::try_from(code).ok());
            match conventional {
                Some(code) => ExitDecision {
                    code,
                    primary: None,
                    secondary: Vec::new(),
                },
                None => ExitDecision {
                    code: 1,
                    primary: Some(SupervisionDiagnostic::ExceptionalUnixSignal { signal: *signal }),
                    secondary: Vec::new(),
                },
            }
        }
        ChildOutcome::Exited(ChildStatus::ExceptionalWindows { raw_status }) => ExitDecision {
            code: 1,
            primary: Some(SupervisionDiagnostic::ExceptionalWindowsStatus {
                raw_status: *raw_status,
            }),
            secondary: Vec::new(),
        },
        ChildOutcome::Exited(ChildStatus::ExceptionalUnix { raw_status }) => ExitDecision {
            code: 1,
            primary: Some(SupervisionDiagnostic::ExceptionalUnixStatus {
                raw_status: *raw_status,
            }),
            secondary: Vec::new(),
        },
        ChildOutcome::Failed(failure) => ExitDecision {
            code: failure_exit(failure),
            primary: Some(SupervisionDiagnostic::Process(failure.clone())),
            secondary: Vec::new(),
        },
    }
}

fn failure_exit(failure: &ProcessFailure) -> u8 {
    if failure.stage == ProcessStage::Spawn
        && matches!(
            failure.kind,
            io::ErrorKind::NotFound | io::ErrorKind::PermissionDenied
        )
    {
        ExitCategory::MissingInput.code()
    } else {
        ExitCategory::Internal.code()
    }
}

fn interrupt_failures(path: &InterruptPath) -> Vec<&ProcessFailure> {
    match path {
        InterruptPath::Graceful {
            delivery: InterruptDelivery::Failed(failure),
            ..
        } => vec![failure],
        InterruptPath::Forced {
            delivery,
            termination,
            ..
        } => {
            let mut failures = Vec::with_capacity(2);
            if let InterruptDelivery::Failed(failure) = delivery {
                failures.push(failure);
            }
            if let ForceTermination::Failed(failure) = termination {
                failures.push(failure);
            }
            failures
        }
        _ => Vec::new(),
    }
}

fn add_failure(decision: &mut ExitDecision, code: u8, diagnostic: SupervisionDiagnostic) {
    if decision.code == 0 && decision.primary.is_none() {
        decision.code = code;
        decision.primary = Some(diagnostic);
    } else {
        decision.secondary.push(diagnostic);
    }
}

fn wait_for_child(
    child: &mut Child,
    platform: &mut platform::Platform,
    executable: &Path,
) -> (ChildOutcome, InterruptPath) {
    let mut interrupt = InterruptPath::None;

    loop {
        interrupt = observe_interrupts(child, platform, executable, interrupt);

        match child.try_wait() {
            Ok(Some(status)) => {
                interrupt = observe_interrupts(child, platform, executable, interrupt);
                return (
                    ChildOutcome::Exited(platform::child_status(status)),
                    interrupt,
                );
            }
            Ok(None) => thread::sleep(POLL_INTERVAL),
            Err(error) => {
                let failure = ProcessFailure::from_io(ProcessStage::Wait, executable, &error);
                let termination = platform.force(child, executable);
                if !matches!(termination, ForceTermination::Failed(_)) {
                    let _ = child.wait();
                }
                return (ChildOutcome::Failed(failure), interrupt);
            }
        }
    }
}

fn observe_interrupts(
    child: &mut Child,
    platform: &mut platform::Platform,
    executable: &Path,
    mut path: InterruptPath,
) -> InterruptPath {
    for event in platform.pending_interrupts() {
        let kind = event.kind();
        path = match path {
            InterruptPath::None => InterruptPath::Graceful {
                interrupt: kind,
                delivery: platform.forward_first(child, &event, executable),
            },
            InterruptPath::Graceful {
                interrupt: first,
                delivery,
            } => InterruptPath::Forced {
                first,
                delivery,
                second: kind,
                termination: platform.force(child, executable),
            },
            forced @ InterruptPath::Forced { .. } => forced,
        };
    }
    path
}

fn finish<F>(
    cleanup: &mut CleanupGuard<F>,
    child: ChildOutcome,
    interrupt: InterruptPath,
) -> SupervisionOutcome
where
    F: FnOnce() -> CleanupOutcome,
{
    SupervisionOutcome {
        child,
        interrupt,
        cleanup: cleanup.run(),
    }
}

struct CleanupGuard<F> {
    action: Option<F>,
}

impl<F> CleanupGuard<F>
where
    F: FnOnce() -> CleanupOutcome,
{
    const fn new(action: F) -> Self {
        Self {
            action: Some(action),
        }
    }

    fn run(&mut self) -> CleanupOutcome {
        let action = self
            .action
            .take()
            .expect("cleanup guard may be consumed only once");
        action()
    }
}

#[cfg(test)]
mod tests {
    use std::ffi::OsString;
    use std::io;
    use std::path::PathBuf;

    use super::{
        ChildOutcome, ChildStatus, CleanupFailure, CleanupOutcome, ExitDecision, ForceTermination,
        InterruptDelivery, InterruptKind, InterruptPath, ProcessFailure, ProcessStage,
        SupervisionDiagnostic, SupervisionOutcome, map_exit,
    };

    fn failure(stage: ProcessStage, kind: io::ErrorKind) -> ProcessFailure {
        let error = io::Error::new(kind, "fixture failure");
        ProcessFailure {
            stage,
            executable: PathBuf::from("agent"),
            kind: error.kind(),
            raw_os_error: error.raw_os_error(),
            reason: error.to_string(),
        }
    }

    fn cleanup_failure() -> CleanupFailure {
        CleanupFailure {
            reason: "cleanup fixture failed".to_owned(),
            failed_paths: vec![PathBuf::from("mounted-skill")],
            retained_journal: Some(PathBuf::from("transaction.yaml")),
            recovery_command: vec![OsString::from("asm"), OsString::from("cleanup")],
        }
    }

    fn outcome(child: ChildOutcome, cleanup: CleanupOutcome) -> SupervisionOutcome {
        SupervisionOutcome {
            child,
            interrupt: InterruptPath::None,
            cleanup,
        }
    }

    fn decision(
        code: u8,
        primary: Option<SupervisionDiagnostic>,
        secondary: Vec<SupervisionDiagnostic>,
    ) -> ExitDecision {
        ExitDecision {
            code,
            primary,
            secondary,
        }
    }

    struct Case {
        name: &'static str,
        outcome: SupervisionOutcome,
        expected: ExitDecision,
    }

    fn assert_cases(cases: Vec<Case>) {
        for case in cases {
            assert_eq!(map_exit(&case.outcome), case.expected, "{}", case.name);
        }
    }

    #[test]
    fn exit_mapping_preserves_child_statuses() {
        assert_cases(vec![
            Case {
                name: "successful child",
                outcome: outcome(
                    ChildOutcome::Exited(ChildStatus::Exited(0)),
                    CleanupOutcome::Succeeded,
                ),
                expected: decision(0, None, vec![]),
            },
            Case {
                name: "nonzero child",
                outcome: outcome(
                    ChildOutcome::Exited(ChildStatus::Exited(2)),
                    CleanupOutcome::Succeeded,
                ),
                expected: decision(2, None, vec![]),
            },
            Case {
                name: "Unix interrupt status",
                outcome: outcome(
                    ChildOutcome::Exited(ChildStatus::Signaled {
                        signal: 2,
                        core_dumped: false,
                    }),
                    CleanupOutcome::Succeeded,
                ),
                expected: decision(130, None, vec![]),
            },
            Case {
                name: "exceptional Windows status",
                outcome: outcome(
                    ChildOutcome::Exited(ChildStatus::ExceptionalWindows {
                        raw_status: 0xc000_013a,
                    }),
                    CleanupOutcome::Succeeded,
                ),
                expected: decision(
                    1,
                    Some(SupervisionDiagnostic::ExceptionalWindowsStatus {
                        raw_status: 0xc000_013a,
                    }),
                    vec![],
                ),
            },
            Case {
                name: "exceptional Unix status",
                outcome: outcome(
                    ChildOutcome::Exited(ChildStatus::ExceptionalUnix { raw_status: 0x7f }),
                    CleanupOutcome::Succeeded,
                ),
                expected: decision(
                    1,
                    Some(SupervisionDiagnostic::ExceptionalUnixStatus { raw_status: 0x7f }),
                    vec![],
                ),
            },
        ]);
    }

    #[test]
    fn exit_mapping_preserves_process_failure_categories() {
        let spawn_missing = failure(ProcessStage::Spawn, io::ErrorKind::NotFound);
        let wait_failure = failure(ProcessStage::Wait, io::ErrorKind::Other);
        let forward_failure = failure(ProcessStage::ForwardInterrupt, io::ErrorKind::Other);

        assert_cases(vec![
            Case {
                name: "missing executable",
                outcome: outcome(
                    ChildOutcome::Failed(spawn_missing.clone()),
                    CleanupOutcome::Succeeded,
                ),
                expected: decision(
                    66,
                    Some(SupervisionDiagnostic::Process(spawn_missing)),
                    vec![],
                ),
            },
            Case {
                name: "wait failure",
                outcome: outcome(
                    ChildOutcome::Failed(wait_failure.clone()),
                    CleanupOutcome::Succeeded,
                ),
                expected: decision(
                    70,
                    Some(SupervisionDiagnostic::Process(wait_failure)),
                    vec![],
                ),
            },
            Case {
                name: "forward failure after successful child",
                outcome: SupervisionOutcome {
                    child: ChildOutcome::Exited(ChildStatus::Exited(0)),
                    interrupt: InterruptPath::Graceful {
                        interrupt: InterruptKind::Interrupt,
                        delivery: InterruptDelivery::Failed(forward_failure.clone()),
                    },
                    cleanup: CleanupOutcome::Succeeded,
                },
                expected: decision(
                    70,
                    Some(SupervisionDiagnostic::Process(forward_failure)),
                    vec![],
                ),
            },
        ]);
    }

    #[test]
    fn exit_mapping_preserves_cleanup_precedence() {
        let cleanup = cleanup_failure();
        let forward_failure = failure(ProcessStage::ForwardInterrupt, io::ErrorKind::Other);

        assert_cases(vec![
            Case {
                name: "successful child and failed cleanup",
                outcome: outcome(
                    ChildOutcome::Exited(ChildStatus::Exited(0)),
                    CleanupOutcome::Failed(cleanup.clone()),
                ),
                expected: decision(
                    73,
                    Some(SupervisionDiagnostic::Cleanup(cleanup.clone())),
                    vec![],
                ),
            },
            Case {
                name: "failed child and failed cleanup",
                outcome: outcome(
                    ChildOutcome::Exited(ChildStatus::Exited(9)),
                    CleanupOutcome::Failed(cleanup.clone()),
                ),
                expected: decision(
                    9,
                    None,
                    vec![SupervisionDiagnostic::Cleanup(cleanup.clone())],
                ),
            },
            Case {
                name: "forward and cleanup failures preserve process precedence",
                outcome: SupervisionOutcome {
                    child: ChildOutcome::Exited(ChildStatus::Exited(0)),
                    interrupt: InterruptPath::Forced {
                        first: InterruptKind::Interrupt,
                        delivery: InterruptDelivery::Failed(forward_failure.clone()),
                        second: InterruptKind::Interrupt,
                        termination: ForceTermination::Terminated,
                    },
                    cleanup: CleanupOutcome::Failed(cleanup.clone()),
                },
                expected: decision(
                    70,
                    Some(SupervisionDiagnostic::Process(forward_failure)),
                    vec![SupervisionDiagnostic::Cleanup(cleanup)],
                ),
            },
        ]);
    }
}
