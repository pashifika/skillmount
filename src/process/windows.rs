use std::io;
use std::os::windows::process::CommandExt;
use std::path::Path;
use std::process::{Child, Command, ExitStatus};
use std::sync::mpsc::{self, Receiver};

use windows_sys::Win32::System::Threading::CREATE_NEW_PROCESS_GROUP;

use super::windows_ffi;
use super::{
    ChildStatus, ForceTermination, InterruptDelivery, InterruptKind, ProcessFailure, ProcessStage,
};

pub(super) struct Platform {
    interrupts: Receiver<()>,
    creation_flags: u32,
}

impl Platform {
    pub(super) fn install() -> io::Result<Self> {
        let (sender, interrupts) = mpsc::channel();
        ctrlc::try_set_handler(move || {
            let _ = sender.send(());
        })
        .map_err(|error| io::Error::other(error.to_string()))?;
        Ok(Self {
            interrupts,
            creation_flags: CREATE_NEW_PROCESS_GROUP,
        })
    }

    pub(super) fn configure_command(&self, command: &mut Command) {
        command.creation_flags(self.creation_flags);
    }

    pub(super) fn pending_interrupts(&mut self) -> Vec<Interrupt> {
        self.interrupts
            .try_iter()
            .map(|()| Interrupt {
                kind: InterruptKind::Interrupt,
            })
            .collect()
    }

    pub(super) fn forward_first(
        &self,
        child: &mut Child,
        _interrupt: &Interrupt,
        executable: &Path,
    ) -> InterruptDelivery {
        if child_has_exited(child) {
            return InterruptDelivery::ChildAlreadyExited;
        }

        match windows_ffi::generate_console_break(self.process_group_id(child)) {
            Ok(()) => InterruptDelivery::Forwarded,
            Err(error) => InterruptDelivery::Failed(ProcessFailure::from_io(
                ProcessStage::ForwardInterrupt,
                executable,
                &error,
            )),
        }
    }

    pub(super) fn force(&self, child: &mut Child, executable: &Path) -> ForceTermination {
        if child_has_exited(child) {
            return ForceTermination::ChildAlreadyExited;
        }

        match self.terminate(child) {
            Ok(()) => ForceTermination::Terminated,
            Err(error) if error.kind() == io::ErrorKind::InvalidInput => {
                ForceTermination::ChildAlreadyExited
            }
            Err(error) => ForceTermination::Failed(ProcessFailure::from_io(
                ProcessStage::ForceTermination,
                executable,
                &error,
            )),
        }
    }

    fn process_group_id(&self, child: &Child) -> u32 {
        debug_assert_eq!(self.creation_flags, CREATE_NEW_PROCESS_GROUP);
        child.id()
    }

    fn terminate(&self, child: &mut Child) -> io::Result<()> {
        debug_assert_eq!(self.creation_flags, CREATE_NEW_PROCESS_GROUP);
        child.kill()
    }
}

pub(super) struct Interrupt {
    kind: InterruptKind,
}

impl Interrupt {
    pub(super) const fn kind(&self) -> InterruptKind {
        self.kind
    }
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
    use std::os::windows::process::ExitStatusExt;
    use std::process::Stdio;

    use super::*;

    #[test]
    fn forwarding_tolerates_a_child_that_is_already_reaped() {
        let platform = Platform::install().expect("install Windows console observation");
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
            &Interrupt {
                kind: InterruptKind::Interrupt,
            },
            Path::new("test executable"),
        );

        assert_eq!(delivery, InterruptDelivery::ChildAlreadyExited);
    }

    #[test]
    fn exceptional_status_keeps_its_unsigned_windows_value() {
        let raw_status = 0xc000_013a;
        assert_eq!(
            child_status(ExitStatus::from_raw(raw_status)),
            ChildStatus::ExceptionalWindows { raw_status }
        );
    }
}
