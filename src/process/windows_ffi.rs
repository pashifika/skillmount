//! Audited Windows console-control and Job Object boundary.

#![allow(unsafe_code)]

use std::ffi::c_void;
use std::io;
use std::mem::size_of;
use std::os::windows::io::AsRawHandle;
use std::process::Child;
use std::ptr;
use std::sync::OnceLock;

use windows_sys::Win32::Foundation::{CloseHandle, HANDLE};
#[cfg(feature = "test-fixtures")]
use windows_sys::Win32::Foundation::{ERROR_INVALID_PARAMETER, WAIT_OBJECT_0, WAIT_TIMEOUT};
#[cfg(feature = "test-fixtures")]
use windows_sys::Win32::Storage::FileSystem::SYNCHRONIZE;
#[cfg(feature = "test-fixtures")]
use windows_sys::Win32::System::Console::GenerateConsoleCtrlEvent;
use windows_sys::Win32::System::Console::{CTRL_BREAK_EVENT, CTRL_C_EVENT, SetConsoleCtrlHandler};
use windows_sys::Win32::System::JobObjects::{
    AssignProcessToJobObject, CreateJobObjectW, JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE,
    JOBOBJECT_BASIC_ACCOUNTING_INFORMATION, JOBOBJECT_EXTENDED_LIMIT_INFORMATION,
    JobObjectBasicAccountingInformation, JobObjectExtendedLimitInformation,
    QueryInformationJobObject, SetInformationJobObject, TerminateJobObject,
};
#[cfg(feature = "test-fixtures")]
use windows_sys::Win32::System::Threading::{OpenProcess, WaitForSingleObject};

use super::InterruptKind;

static INSTALLATION: OnceLock<Result<(), StoredError>> = OnceLock::new();

pub(super) fn install_console_handler() -> io::Result<()> {
    match INSTALLATION.get_or_init(register_console_handler) {
        Ok(()) => Ok(()),
        Err(error) => Err(error.to_io()),
    }
}

fn register_console_handler() -> Result<(), StoredError> {
    // SAFETY: the callback has process lifetime, uses the exact system ABI, and performs only a
    // lock-free atomic state transition. It never allocates, locks, or unwinds across the FFI
    // boundary. The registration intentionally remains installed until process exit.
    let registered = unsafe { SetConsoleCtrlHandler(Some(console_handler), 1) };
    if registered == 0 {
        Err(StoredError::from_io(&io::Error::last_os_error()))
    } else {
        Ok(())
    }
}

unsafe extern "system" fn console_handler(raw_event: u32) -> windows_sys::core::BOOL {
    decode_console_event(raw_event)
        .is_some_and(super::windows::record_console_event)
        .into()
}

fn decode_console_event(raw_event: u32) -> Option<InterruptKind> {
    match raw_event {
        CTRL_C_EVENT => Some(InterruptKind::Interrupt),
        CTRL_BREAK_EVENT => Some(InterruptKind::Break),
        _ => None,
    }
}

#[cfg(feature = "test-fixtures")]
pub(super) fn generate_console_break(process_group_id: u32) -> io::Result<()> {
    // SAFETY: `GenerateConsoleCtrlEvent` receives two integer values and retains no borrowed
    // memory. The fixture supplies the live process-group identifier created by its controller.
    let generated = unsafe { GenerateConsoleCtrlEvent(CTRL_BREAK_EVENT, process_group_id) };
    bool_result(generated)
}

#[cfg(feature = "test-fixtures")]
pub(super) fn process_is_running(process_id: u32) -> io::Result<bool> {
    // SAFETY: the integer PID comes from the fixture's own process record. The returned handle is
    // either null or owned locally and is closed exactly once before returning.
    let process = unsafe { OpenProcess(SYNCHRONIZE, 0, process_id) };
    if process.is_null() {
        let error = io::Error::last_os_error();
        return if error.raw_os_error() == i32::try_from(ERROR_INVALID_PARAMETER).ok() {
            Ok(false)
        } else {
            Err(error)
        };
    }

    // SAFETY: `process` is a live synchronization handle. A zero timeout performs a nonblocking
    // state query and retains no borrowed memory.
    let wait = unsafe { WaitForSingleObject(process, 0) };
    // SAFETY: `process` is the owned handle returned by `OpenProcess` and is closed exactly once.
    unsafe {
        CloseHandle(process);
    }
    match wait {
        WAIT_OBJECT_0 => Ok(false),
        WAIT_TIMEOUT => Ok(true),
        _ => Err(io::Error::last_os_error()),
    }
}

pub(super) struct JobObject {
    handle: HANDLE,
}

impl JobObject {
    pub(super) fn create() -> io::Result<Self> {
        // SAFETY: both optional pointer arguments are null, requesting an unnamed Job Object with
        // default security. The returned owned handle is closed by `Drop` or on setup failure.
        let handle = unsafe { CreateJobObjectW(ptr::null(), ptr::null()) };
        if handle.is_null() {
            return Err(io::Error::last_os_error());
        }

        let mut limits = JOBOBJECT_EXTENDED_LIMIT_INFORMATION::default();
        limits.BasicLimitInformation.LimitFlags = JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE;
        // SAFETY: `limits` has the exact layout required by
        // `JobObjectExtendedLimitInformation`; the pointer remains valid for the call and the
        // kernel does not retain it. `handle` is a live Job Object owned by this function.
        let configured = unsafe {
            SetInformationJobObject(
                handle,
                JobObjectExtendedLimitInformation,
                ptr::from_ref(&limits).cast::<c_void>(),
                structure_size::<JOBOBJECT_EXTENDED_LIMIT_INFORMATION>(),
            )
        };
        if configured == 0 {
            let error = io::Error::last_os_error();
            // SAFETY: `handle` is the still-owned live handle returned above and is closed exactly
            // once on this error path.
            unsafe {
                CloseHandle(handle);
            }
            return Err(error);
        }

        Ok(Self { handle })
    }

    pub(super) fn assign(&self, child: &Child) -> io::Result<()> {
        let process = child.as_raw_handle();
        // SAFETY: `self.handle` owns a live Job Object and `process` is borrowed from a live
        // `Child`. Windows retains a reference to the process, not the Rust borrow.
        let assigned = unsafe { AssignProcessToJobObject(self.handle, process) };
        bool_result(assigned)
    }

    pub(super) fn terminate(&self, exit_code: u32) -> io::Result<()> {
        // SAFETY: `self.handle` remains live for this call. `TerminateJobObject` retains no Rust
        // memory and applies the supplied numeric status to processes in the job.
        let terminated = unsafe { TerminateJobObject(self.handle, exit_code) };
        bool_result(terminated)
    }

    pub(super) fn active_processes(&self) -> io::Result<u32> {
        let mut accounting = JOBOBJECT_BASIC_ACCOUNTING_INFORMATION::default();
        // SAFETY: `accounting` has the exact queried layout and remains writable for the duration
        // of the call. The return-length pointer is optional and the kernel retains no pointer.
        let queried = unsafe {
            QueryInformationJobObject(
                self.handle,
                JobObjectBasicAccountingInformation,
                ptr::from_mut(&mut accounting).cast::<c_void>(),
                structure_size::<JOBOBJECT_BASIC_ACCOUNTING_INFORMATION>(),
                ptr::null_mut(),
            )
        };
        bool_result(queried)?;
        Ok(accounting.ActiveProcesses)
    }
}

impl Drop for JobObject {
    fn drop(&mut self) {
        // SAFETY: `self.handle` is owned by this value, remains live until this call, and is closed
        // exactly once because `JobObject` is neither `Clone` nor `Copy`.
        unsafe {
            CloseHandle(self.handle);
        }
    }
}

fn structure_size<T>() -> u32 {
    u32::try_from(size_of::<T>()).expect("Windows structure size fits in a 32-bit API length")
}

fn bool_result(result: windows_sys::core::BOOL) -> io::Result<()> {
    if result == 0 {
        Err(io::Error::last_os_error())
    } else {
        Ok(())
    }
}

struct StoredError {
    kind: io::ErrorKind,
    raw_os_error: Option<i32>,
    reason: String,
}

impl StoredError {
    fn from_io(error: &io::Error) -> Self {
        Self {
            kind: error.kind(),
            raw_os_error: error.raw_os_error(),
            reason: error.to_string(),
        }
    }

    fn to_io(&self) -> io::Error {
        self.raw_os_error.map_or_else(
            || io::Error::new(self.kind, self.reason.clone()),
            io::Error::from_raw_os_error,
        )
    }
}

#[cfg(test)]
mod tests {
    use windows_sys::Win32::System::Console::{
        CTRL_CLOSE_EVENT, CTRL_LOGOFF_EVENT, CTRL_SHUTDOWN_EVENT,
    };

    use super::*;

    #[test]
    fn only_interrupt_and_break_events_enter_supervision() {
        assert_eq!(
            decode_console_event(CTRL_C_EVENT),
            Some(InterruptKind::Interrupt)
        );
        assert_eq!(
            decode_console_event(CTRL_BREAK_EVENT),
            Some(InterruptKind::Break)
        );
        assert_eq!(decode_console_event(CTRL_CLOSE_EVENT), None);
        assert_eq!(decode_console_event(CTRL_LOGOFF_EVENT), None);
        assert_eq!(decode_console_event(CTRL_SHUTDOWN_EVENT), None);
    }
}
