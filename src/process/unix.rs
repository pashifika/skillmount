use std::io::{self, IsTerminal};
use std::os::unix::process::{CommandExt, ExitStatusExt};
use std::path::Path;
use std::process::{Child, Command, ExitStatus};

use nix::errno::Errno;
use nix::sys::signal::{Signal, kill, killpg};
use nix::unistd::Pid;
use signal_hook::consts::signal::{SIGINT, SIGTERM};

use super::event::{EventLedger, EventSession};
use super::unix_ffi;
use super::{
    ChildStatus, ForceTermination, InterruptDelivery, InterruptKind, ProcessFailure, ProcessStage,
};

static EVENTS: EventLedger = EventLedger::new();

// The shared platform seam is fallible because Windows rejects implicit-shell executables.
#[allow(clippy::unnecessary_wraps)]
pub(super) fn validate_executable(_executable: &Path) -> io::Result<()> {
    Ok(())
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Grouping {
    SharedForeground,
    Dedicated,
}

pub(super) struct Platform {
    events: EventSession,
    grouping: Grouping,
}

impl Platform {
    pub(super) fn install() -> io::Result<Self> {
        unix_ffi::install()?;
        let events = EVENTS.acquire()?;
        let grouping = if io::stdin().is_terminal() {
            Grouping::SharedForeground
        } else {
            Grouping::Dedicated
        };
        Ok(Self { events, grouping })
    }

    pub(super) fn configure_command(&self, command: &mut Command) {
        if self.grouping == Grouping::Dedicated {
            command.process_group(0);
        }
    }

    // The shared platform seam prepares a fallible Windows containment object before spawn.
    #[allow(clippy::unnecessary_wraps, clippy::unused_self)]
    pub(super) fn prepare_containment(&mut self) -> io::Result<()> {
        Ok(())
    }

    // The shared platform seam attaches a child to Windows containment immediately after spawn.
    #[allow(clippy::unnecessary_wraps, clippy::unused_self)]
    pub(super) fn attach(&mut self, _child: &Child) -> io::Result<()> {
        Ok(())
    }

    pub(super) fn pending_interrupts(&mut self) -> Vec<Interrupt> {
        self.events
            .pending()
            .into_iter()
            .filter_map(|kind| self.interrupt(kind))
            .collect()
    }

    pub(super) fn begin_finalization(&mut self) -> Result<(), Vec<Interrupt>> {
        self.events.begin_finalization().map_err(|events| {
            events
                .into_iter()
                .filter_map(|kind| self.interrupt(kind))
                .collect()
        })
    }

    pub(super) fn forward_first(
        &self,
        child: &mut Child,
        interrupt: Interrupt,
        executable: &Path,
    ) -> InterruptDelivery {
        if interrupt.delivered_by_platform {
            return InterruptDelivery::DeliveredByPlatform;
        }
        if self.grouping == Grouping::SharedForeground && child_has_exited(child) {
            return InterruptDelivery::ChildAlreadyExited;
        }

        let signal = match Signal::try_from(interrupt.signal) {
            Ok(signal) => signal,
            Err(error) => {
                return InterruptDelivery::Failed(ProcessFailure::from_io(
                    ProcessStage::ForwardInterrupt,
                    executable,
                    None,
                    &io::Error::new(io::ErrorKind::InvalidInput, error.to_string()),
                ));
            }
        };
        let result = match self.grouping {
            Grouping::SharedForeground => {
                child_pid(child).and_then(|pid| kill(pid, signal).map_err(errno_to_io))
            }
            Grouping::Dedicated => {
                child_pid(child).and_then(|pid| killpg(pid, signal).map_err(errno_to_io))
            }
        };

        delivery_result(result, executable)
    }

    pub(super) fn force(&self, child: &mut Child, executable: &Path) -> ForceTermination {
        if self.grouping == Grouping::SharedForeground && child_has_exited(child) {
            return ForceTermination::ChildAlreadyExited;
        }

        let result = match self.grouping {
            Grouping::SharedForeground => child.kill(),
            Grouping::Dedicated => {
                child_pid(child).and_then(|pid| killpg(pid, Signal::SIGKILL).map_err(errno_to_io))
            }
        };

        match result {
            Ok(()) => ForceTermination::Terminated,
            Err(error) if error.raw_os_error() == Some(Errno::ESRCH as i32) => {
                ForceTermination::ChildAlreadyExited
            }
            Err(error) => ForceTermination::Failed(ProcessFailure::from_io(
                ProcessStage::ForceTermination,
                executable,
                None,
                &error,
            )),
        }
    }

    pub(super) fn domain_is_empty(&self, child: &Child) -> io::Result<bool> {
        if self.grouping == Grouping::SharedForeground {
            return Ok(true);
        }

        let pid = child_pid(child)?;
        match killpg(pid, None) {
            Ok(()) | Err(Errno::EPERM) => Ok(false),
            Err(Errno::ESRCH) => Ok(true),
            Err(error) => Err(errno_to_io(error)),
        }
    }

    fn interrupt(&self, kind: InterruptKind) -> Option<Interrupt> {
        let signal = match kind {
            InterruptKind::Interrupt => SIGINT,
            InterruptKind::Terminate => SIGTERM,
            InterruptKind::Break => return None,
        };
        Some(Interrupt {
            kind,
            signal,
            delivered_by_platform: self.grouping == Grouping::SharedForeground
                && kind == InterruptKind::Interrupt,
        })
    }
}

pub(super) fn record_signal(signal: i32, kind: InterruptKind) {
    if !EVENTS.record(kind) {
        let _ = signal_hook::low_level::emulate_default_handler(signal);
    }
}

#[derive(Clone, Copy)]
pub(super) struct Interrupt {
    kind: InterruptKind,
    signal: i32,
    delivered_by_platform: bool,
}

impl Interrupt {
    pub(super) const fn kind(self) -> InterruptKind {
        self.kind
    }
}

pub(super) fn child_status(status: ExitStatus) -> ChildStatus {
    if let Some(code) = status.code().and_then(|code| u8::try_from(code).ok()) {
        ChildStatus::Exited(code)
    } else if let Some(signal) = status.signal() {
        ChildStatus::Signaled {
            signal,
            core_dumped: status.core_dumped(),
        }
    } else {
        ChildStatus::ExceptionalUnix {
            raw_status: status.into_raw(),
        }
    }
}

fn child_pid(child: &Child) -> io::Result<Pid> {
    i32::try_from(child.id())
        .map(Pid::from_raw)
        .map_err(|error| io::Error::new(io::ErrorKind::InvalidData, error))
}

fn child_has_exited(child: &mut Child) -> bool {
    matches!(child.try_wait(), Ok(Some(_)))
}

fn delivery_result(result: io::Result<()>, executable: &Path) -> InterruptDelivery {
    match result {
        Ok(()) => InterruptDelivery::Forwarded,
        Err(error) if error.raw_os_error() == Some(Errno::ESRCH as i32) => {
            InterruptDelivery::ChildAlreadyExited
        }
        Err(error) => InterruptDelivery::Failed(ProcessFailure::from_io(
            ProcessStage::ForwardInterrupt,
            executable,
            None,
            &error,
        )),
    }
}

fn errno_to_io(error: Errno) -> io::Error {
    io::Error::from_raw_os_error(error as i32)
}

#[cfg(test)]
mod tests {
    use std::process::Stdio;

    use super::*;

    #[test]
    fn forwarding_tolerates_a_child_that_is_already_reaped() {
        let platform = Platform::install().expect("install Unix signal observation");
        let mut command = Command::new(std::env::current_exe().expect("current test executable"));
        command
            .arg("--help")
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null());
        platform.configure_command(&mut command);
        let mut child = command.spawn().expect("spawn short-lived child");
        child.wait().expect("reap short-lived child");

        let delivery = platform.forward_first(
            &mut child,
            Interrupt {
                kind: InterruptKind::Interrupt,
                signal: SIGINT,
                delivered_by_platform: false,
            },
            Path::new("test executable"),
        );

        assert_eq!(delivery, InterruptDelivery::ChildAlreadyExited);
    }
}
