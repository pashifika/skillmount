use std::io;
use std::path::Path;
use std::process::{Child, Command, ExitStatus};

use super::event::{EventLedger, EventSession, EventToken};
use super::windows_ffi::{self, JobObject};
use super::{
    ChildStatus, ForceTermination, InterruptDelivery, InterruptKind, ProcessFailure, ProcessStage,
};

static EVENTS: EventLedger = EventLedger::new();

pub(super) fn validate_executable(executable: &Path) -> io::Result<()> {
    let is_batch = executable.extension().is_some_and(|extension| {
        extension.eq_ignore_ascii_case("cmd") || extension.eq_ignore_ascii_case("bat")
    });
    if is_batch {
        Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "Windows batch files require implicit cmd.exe execution and are not shell-free",
        ))
    } else {
        Ok(())
    }
}

pub(super) struct CaptureDomain {
    job: JobObject,
    attached: bool,
}

impl CaptureDomain {
    #[allow(clippy::unused_self)]
    pub(super) fn prepare(_command: &mut Command) -> io::Result<Self> {
        Ok(Self {
            job: JobObject::create()?,
            attached: false,
        })
    }

    pub(super) fn attach(&mut self, child: &Child) -> io::Result<()> {
        self.job.assign(child)?;
        self.attached = true;
        Ok(())
    }

    pub(super) fn terminate(&self, child: &mut Child) -> io::Result<()> {
        if !self.attached {
            return child.kill();
        }
        match self.job.active_processes() {
            Ok(0) => Ok(()),
            Ok(_) | Err(_) => self.job.terminate(1).or_else(|job_error| {
                child.kill().map_err(|child_error| {
                    io::Error::other(format!(
                        "cannot terminate capture Job Object ({job_error}) or root process ({child_error})"
                    ))
                })
            }),
        }
    }

    #[allow(clippy::unused_self)]
    pub(super) fn mark_root_reaped(&mut self) {}

    pub(super) fn is_empty(&self) -> io::Result<bool> {
        if self.attached {
            self.job.active_processes().map(|count| count == 0)
        } else {
            Ok(true)
        }
    }
}

pub(super) struct Platform {
    events: EventSession,
    job: Option<JobObject>,
    attached: bool,
}

impl Platform {
    pub(super) fn install() -> io::Result<Self> {
        windows_ffi::install_console_handler()?;
        let events = EVENTS.acquire()?;
        Ok(Self {
            events,
            job: None,
            attached: false,
        })
    }

    // The shared platform seam configures Unix grouping; Windows inherits the wrapper group.
    #[allow(clippy::unused_self)]
    pub(super) fn configure_command(&self, _command: &mut Command) {}

    pub(super) fn prepare_containment(&mut self) -> io::Result<()> {
        if self.job.is_some() {
            return Err(io::Error::new(
                io::ErrorKind::AlreadyExists,
                "a Windows Job Object is already prepared for this session",
            ));
        }
        self.job = Some(JobObject::create()?);
        Ok(())
    }

    pub(super) fn attach(&mut self, child: &Child) -> io::Result<()> {
        let job = self.job.as_ref().ok_or_else(|| {
            io::Error::new(
                io::ErrorKind::NotFound,
                "the Windows Job Object was not prepared before spawn",
            )
        })?;
        job.assign(child)?;
        self.attached = true;
        Ok(())
    }

    pub(super) fn activate(&mut self) -> io::Result<()> {
        self.events.activate()
    }

    pub(super) fn pending_interrupts(&mut self) -> Vec<Interrupt> {
        self.events
            .pending()
            .into_iter()
            .filter_map(Interrupt::from_kind)
            .collect()
    }

    pub(super) fn begin_finalization(&mut self) -> Result<(), Vec<Interrupt>> {
        self.events.begin_finalization().map_err(|events| {
            events
                .into_iter()
                .filter_map(Interrupt::from_kind)
                .collect()
        })
    }

    #[allow(clippy::unused_self)]
    pub(super) const fn classify_after_proof(&self, _interrupt: Interrupt) -> InterruptDelivery {
        InterruptDelivery::DeliveredByPlatform
    }

    // Windows console delivery already reached the shared group before this observation.
    #[allow(clippy::unused_self)]
    pub(super) fn forward_first(
        &self,
        _child: &mut Child,
        _interrupt: Interrupt,
        _executable: &Path,
        _cwd: &Path,
        _root_reaped: bool,
    ) -> InterruptDelivery {
        InterruptDelivery::DeliveredByPlatform
    }

    pub(super) fn force(
        &self,
        child: &mut Child,
        executable: &Path,
        cwd: &Path,
        _root_reaped: bool,
    ) -> ForceTermination {
        if self.attached {
            let Some(job) = &self.job else {
                return ForceTermination::Failed(ProcessFailure::from_io(
                    ProcessStage::ForceTermination,
                    executable,
                    Some(cwd),
                    &io::Error::new(
                        io::ErrorKind::NotFound,
                        "the attached Windows Job Object is unavailable",
                    ),
                ));
            };
            return force_attached_job(job, executable, cwd);
        }
        if child_has_exited(child) {
            return ForceTermination::ChildAlreadyExited;
        }

        let result = child.kill();

        match result {
            Ok(()) => ForceTermination::Terminated,
            Err(_error) if child_has_exited(child) => ForceTermination::ChildAlreadyExited,
            Err(error) => ForceTermination::Failed(ProcessFailure::from_io(
                ProcessStage::ForceTermination,
                executable,
                Some(cwd),
                &error,
            )),
        }
    }

    pub(super) fn domain_is_empty(&self, _child: &Child) -> io::Result<bool> {
        if !self.attached {
            return Err(io::Error::new(
                io::ErrorKind::NotConnected,
                "the child was not attached to the Windows Job Object",
            ));
        }
        self.job
            .as_ref()
            .ok_or_else(|| io::Error::other("the attached Windows Job Object is unavailable"))?
            .active_processes()
            .map(|count| count == 0)
    }

    pub(super) const fn post_root_containment_is_stable(&self) -> bool {
        self.attached
    }
}

fn force_attached_job(job: &JobObject, executable: &Path, cwd: &Path) -> ForceTermination {
    let result = job.active_processes().and_then(|count| {
        if count == 0 {
            Ok(false)
        } else {
            job.terminate(1).map(|()| true)
        }
    });
    attached_force_result(result, executable, cwd)
}

fn attached_force_result(
    result: io::Result<bool>,
    executable: &Path,
    cwd: &Path,
) -> ForceTermination {
    match result {
        Ok(true) => ForceTermination::Terminated,
        Ok(false) => ForceTermination::ChildAlreadyExited,
        Err(error) => ForceTermination::Failed(ProcessFailure::from_io(
            ProcessStage::ForceTermination,
            executable,
            Some(cwd),
            &error,
        )),
    }
}

#[derive(Clone, Copy)]
pub(super) struct Interrupt {
    kind: InterruptKind,
}

impl Interrupt {
    fn from_kind(kind: InterruptKind) -> Option<Self> {
        match kind {
            InterruptKind::Interrupt | InterruptKind::Break => Some(Self { kind }),
            InterruptKind::Terminate => None,
        }
    }

    pub(super) const fn kind(self) -> InterruptKind {
        self.kind
    }
}

pub(super) fn console_event_token() -> EventToken {
    EVENTS.token()
}

pub(super) fn record_console_event(token: EventToken, kind: InterruptKind) -> bool {
    EVENTS.record(token, kind)
}

pub(super) fn child_status(status: ExitStatus) -> ChildStatus {
    let code = status
        .code()
        .expect("Windows always exposes a process exit status");
    match u8::try_from(code) {
        Ok(code) => ChildStatus::Exited(code),
        Err(_) => ChildStatus::ExceptionalWindows {
            raw_status: u32::from_ne_bytes(code.to_ne_bytes()),
        },
    }
}

fn child_has_exited(child: &mut Child) -> bool {
    matches!(child.try_wait(), Ok(Some(_)))
}

#[cfg(test)]
mod tests {
    use std::ffi::OsStr;
    use std::os::windows::process::ExitStatusExt;

    use super::*;

    #[test]
    fn batch_extensions_are_rejected_case_insensitively() {
        assert!(validate_executable(Path::new("agent.CmD")).is_err());
        assert!(validate_executable(Path::new("agent.bAT")).is_err());
        assert!(validate_executable(Path::new("agent.exe")).is_ok());
        assert!(validate_executable(Path::new(OsStr::new("agent"))).is_ok());
    }

    #[test]
    fn exceptional_status_keeps_its_unsigned_windows_value() {
        let raw_status = 0xc000_013a;
        assert_eq!(
            child_status(ExitStatus::from_raw(raw_status)),
            ChildStatus::ExceptionalWindows { raw_status }
        );
    }

    #[test]
    fn attached_job_failure_is_not_downgraded_by_root_process_state() {
        let termination = attached_force_result(
            Err(io::Error::other("fixture Job Object failure")),
            Path::new("test executable"),
            Path::new("test cwd"),
        );

        let ForceTermination::Failed(failure) = termination else {
            panic!("the Job Object failure must remain observable");
        };
        assert_eq!(failure.cwd(), Some(Path::new("test cwd")));
    }
}
