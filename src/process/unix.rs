use std::io::{self, IsTerminal};
use std::os::unix::process::{CommandExt, ExitStatusExt};
use std::path::Path;
use std::process::{Child, Command, ExitStatus};

use nix::errno::Errno;
use nix::sys::signal::{Signal, kill, killpg};
use nix::unistd::Pid;
use signal_hook::consts::signal::{SIGINT, SIGTERM};

use super::event::{EventLedger, EventSession, EventToken};
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

pub(super) struct CaptureDomain {
    group: Option<Pid>,
    root_reaped: bool,
}

impl CaptureDomain {
    #[allow(clippy::unnecessary_wraps)]
    pub(super) fn prepare(command: &mut Command) -> io::Result<Self> {
        command.process_group(0);
        Ok(Self {
            group: None,
            root_reaped: false,
        })
    }

    pub(super) fn attach(&mut self, child: &Child) -> io::Result<()> {
        self.group = Some(child_pid(child)?);
        Ok(())
    }

    pub(super) fn terminate(&self, child: &mut Child) -> io::Result<()> {
        let Some(group) = self.group else {
            return child.kill();
        };
        match killpg(group, Signal::SIGKILL) {
            Ok(()) | Err(Errno::ESRCH | Errno::EPERM) => Ok(()),
            Err(group_error) => child.kill().map_err(|child_error| {
                io::Error::other(format!(
                    "cannot terminate capture process group ({group_error}) or root process ({child_error})"
                ))
            }),
        }
    }

    pub(super) fn mark_root_reaped(&mut self) {
        self.root_reaped = true;
    }

    pub(super) fn is_empty(&self) -> io::Result<bool> {
        let Some(group) = self.group else {
            return Ok(true);
        };
        match killpg(group, None) {
            Err(Errno::ESRCH) => Ok(true),
            Ok(()) | Err(Errno::EPERM) => Ok(false),
            Err(error) => Err(errno_to_io(error)),
        }
    }
}

impl Drop for CaptureDomain {
    fn drop(&mut self) {
        if !self.root_reaped {
            if let Some(group) = self.group {
                let _ = killpg(group, Signal::SIGKILL);
            }
        }
    }
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
        let grouping = if io::stdin().is_terminal() {
            Grouping::SharedForeground
        } else {
            Grouping::Dedicated
        };
        let mut events = EVENTS.acquire()?;
        if grouping == Grouping::Dedicated {
            events.activate()?;
        }
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

    pub(super) fn activate(&mut self) -> io::Result<()> {
        self.events.activate()
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

    #[allow(clippy::unused_self)]
    pub(super) const fn classify_after_proof(&self, interrupt: Interrupt) -> InterruptDelivery {
        if interrupt.delivered_by_platform {
            InterruptDelivery::DeliveredByPlatform
        } else {
            InterruptDelivery::ChildAlreadyExited
        }
    }

    pub(super) fn forward_first(
        &self,
        child: &mut Child,
        interrupt: Interrupt,
        executable: &Path,
        cwd: &Path,
        root_reaped: bool,
    ) -> InterruptDelivery {
        if interrupt.delivered_by_platform {
            return InterruptDelivery::DeliveredByPlatform;
        }
        if !group_identity_is_safe(self.grouping, root_reaped) {
            return InterruptDelivery::Failed(group_identity_failure(
                ProcessStage::ForwardInterrupt,
                executable,
                cwd,
            ));
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
                    Some(cwd),
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

        delivery_result(result, executable, cwd)
    }

    pub(super) fn force(
        &self,
        child: &mut Child,
        executable: &Path,
        cwd: &Path,
        root_reaped: bool,
    ) -> ForceTermination {
        if !group_identity_is_safe(self.grouping, root_reaped) {
            return ForceTermination::Failed(group_identity_failure(
                ProcessStage::ForceTermination,
                executable,
                cwd,
            ));
        }
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
                Some(cwd),
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

    pub(super) const fn post_root_containment_is_stable(&self) -> bool {
        matches!(self.grouping, Grouping::SharedForeground)
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

pub(super) fn signal_token() -> EventToken {
    EVENTS.token()
}

pub(super) fn record_signal(token: EventToken, signal: i32, kind: InterruptKind) {
    if !EVENTS.record(token, kind) {
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

const fn group_identity_is_safe(grouping: Grouping, root_reaped: bool) -> bool {
    matches!(grouping, Grouping::SharedForeground) || !root_reaped
}

fn delivery_result(result: io::Result<()>, executable: &Path, cwd: &Path) -> InterruptDelivery {
    match result {
        Ok(()) => InterruptDelivery::Forwarded,
        Err(error) if error.raw_os_error() == Some(Errno::ESRCH as i32) => {
            InterruptDelivery::ChildAlreadyExited
        }
        Err(error) => InterruptDelivery::Failed(ProcessFailure::from_io(
            ProcessStage::ForwardInterrupt,
            executable,
            Some(cwd),
            &error,
        )),
    }
}

fn group_identity_failure(stage: ProcessStage, executable: &Path, cwd: &Path) -> ProcessFailure {
    ProcessFailure::from_io(
        stage,
        executable,
        Some(cwd),
        &io::Error::other(
            "the Unix process-group leader was reaped, so the numeric group identity is no longer safe to signal",
        ),
    )
}

fn errno_to_io(error: Errno) -> io::Error {
    io::Error::from_raw_os_error(error as i32)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn post_proof_delivery_never_touches_a_reaped_dedicated_group() {
        let platform = Platform::install().expect("install Unix signal observation");
        assert_eq!(
            platform.classify_after_proof(Interrupt {
                kind: InterruptKind::Interrupt,
                signal: SIGINT,
                delivered_by_platform: false,
            }),
            InterruptDelivery::ChildAlreadyExited
        );
        assert_eq!(
            platform.classify_after_proof(Interrupt {
                kind: InterruptKind::Interrupt,
                signal: SIGINT,
                delivered_by_platform: true,
            }),
            InterruptDelivery::DeliveredByPlatform
        );
    }

    #[test]
    fn dedicated_group_identity_is_not_safe_after_its_leader_is_reaped() {
        assert!(!group_identity_is_safe(Grouping::Dedicated, true));
        assert!(group_identity_is_safe(Grouping::Dedicated, false));
        assert!(group_identity_is_safe(Grouping::SharedForeground, true));
    }
}
