//! Audited Windows console-control and Job Object boundary.

#![allow(unsafe_code)]

use std::ffi::{OsStr, OsString, c_void};
use std::io;
use std::mem::size_of;
use std::os::windows::ffi::OsStringExt as _;
use std::os::windows::io::AsRawHandle;
use std::path::Path;
use std::process::Child;
use std::ptr;
use std::sync::OnceLock;

use windows_sys::Win32::Foundation::{
    CloseHandle, ERROR_NO_MORE_FILES, FILETIME, HANDLE, INVALID_HANDLE_VALUE,
};
#[cfg(feature = "test-fixtures")]
use windows_sys::Win32::Foundation::{ERROR_INVALID_PARAMETER, WAIT_OBJECT_0, WAIT_TIMEOUT};
#[cfg(feature = "test-fixtures")]
use windows_sys::Win32::Storage::FileSystem::SYNCHRONIZE;
#[cfg(feature = "test-fixtures")]
use windows_sys::Win32::System::Console::GenerateConsoleCtrlEvent;
use windows_sys::Win32::System::Console::{CTRL_BREAK_EVENT, CTRL_C_EVENT, SetConsoleCtrlHandler};
use windows_sys::Win32::System::Diagnostics::ToolHelp::{
    CreateToolhelp32Snapshot, PROCESSENTRY32W, Process32FirstW, Process32NextW, TH32CS_SNAPPROCESS,
};
use windows_sys::Win32::System::JobObjects::{
    AssignProcessToJobObject, CreateJobObjectW, JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE,
    JOBOBJECT_BASIC_ACCOUNTING_INFORMATION, JOBOBJECT_EXTENDED_LIMIT_INFORMATION,
    JobObjectBasicAccountingInformation, JobObjectExtendedLimitInformation,
    QueryInformationJobObject, SetInformationJobObject, TerminateJobObject,
};
#[cfg(feature = "test-fixtures")]
use windows_sys::Win32::System::Threading::WaitForSingleObject;
use windows_sys::Win32::System::Threading::{
    GetCurrentProcessId, GetProcessTimes, OpenProcess, PROCESS_QUERY_LIMITED_INFORMATION,
};

use super::{InterruptKind, InvocationShell};

static INSTALLATION: OnceLock<Result<(), StoredError>> = OnceLock::new();
const MAX_ANCESTOR_DEPTH: usize = 32;

/// Observes one bounded process ancestry from one Tool Help snapshot.
///
/// The snapshot is advisory. Capture errors are returned to the caller. A missing link, cycle,
/// duplicate PID, or overlong chain before a supported prompt boundary becomes `Unknown`.
pub(super) fn observe_invocation_shell() -> io::Result<InvocationShell> {
    let entries = process_snapshot()?;
    // SAFETY: `GetCurrentProcessId` takes no arguments and cannot fail.
    let current_process_id = unsafe { GetCurrentProcessId() };
    let Some(images) = ancestor_images(&entries, current_process_id) else {
        return Ok(InvocationShell::Unknown);
    };
    Ok(super::classify_invocation_shell(&images))
}

#[derive(Debug, Clone)]
struct ProcessEntry {
    process_id: u32,
    parent_process_id: u32,
    image: OsString,
}

fn process_snapshot() -> io::Result<Vec<ProcessEntry>> {
    // SAFETY: the flags request a system process snapshot and the ignored process-id argument is
    // zero. The returned owned handle is closed by `SnapshotHandle`.
    let handle = unsafe { CreateToolhelp32Snapshot(TH32CS_SNAPPROCESS, 0) };
    if handle == INVALID_HANDLE_VALUE {
        return Err(io::Error::last_os_error());
    }
    let snapshot = SnapshotHandle(handle);

    // SAFETY: `PROCESSENTRY32W` is a plain Windows ABI structure for which all-zero is a valid
    // initial state once `dwSize` is set as required by the API.
    let mut raw: PROCESSENTRY32W = unsafe { std::mem::zeroed() };
    raw.dwSize = structure_size::<PROCESSENTRY32W>();
    // SAFETY: `snapshot` owns a live Tool Help handle and `raw` points to writable storage with the
    // required size field. The API retains neither pointer.
    if unsafe { Process32FirstW(snapshot.0, &raw mut raw) } == 0 {
        return Err(io::Error::last_os_error());
    }

    let mut entries = Vec::new();
    loop {
        entries.push(decode_process_entry(&raw));
        // SAFETY: the same live snapshot and initialized writable structure remain valid for the
        // next entry, and the API retains neither pointer.
        if unsafe { Process32NextW(snapshot.0, &raw mut raw) } != 0 {
            continue;
        }
        let error = io::Error::last_os_error();
        if error.raw_os_error() == i32::try_from(ERROR_NO_MORE_FILES).ok() {
            break;
        }
        return Err(error);
    }
    Ok(entries)
}

fn decode_process_entry(raw: &PROCESSENTRY32W) -> ProcessEntry {
    let length = raw
        .szExeFile
        .iter()
        .position(|unit| *unit == 0)
        .unwrap_or(raw.szExeFile.len());
    ProcessEntry {
        process_id: raw.th32ProcessID,
        parent_process_id: raw.th32ParentProcessID,
        image: OsString::from_wide(&raw.szExeFile[..length]),
    }
}

fn ancestor_images(entries: &[ProcessEntry], current_process_id: u32) -> Option<Vec<OsString>> {
    ancestor_images_with(entries, current_process_id, |process_id| {
        process_creation_time(process_id).ok()
    })
}

fn ancestor_images_with(
    entries: &[ProcessEntry],
    current_process_id: u32,
    mut creation_time: impl FnMut(u32) -> Option<u64>,
) -> Option<Vec<OsString>> {
    let current = unique_process(entries, current_process_id)?;
    let mut child_creation_time = creation_time(current_process_id)?;
    let mut parent_process_id = current.parent_process_id;
    let mut visited = vec![current_process_id];
    let mut images = Vec::new();

    for _ in 0..MAX_ANCESTOR_DEPTH {
        if parent_process_id == 0 || visited.contains(&parent_process_id) {
            return None;
        }
        let parent = unique_process(entries, parent_process_id)?;
        let parent_creation_time = creation_time(parent_process_id)?;
        // A process can only create a child after it exists. A newer alleged parent means the
        // numeric PID was reused after the real parent exited, so the snapshot does not prove this
        // process instance belongs in the chain.
        if parent_creation_time >= child_creation_time {
            return None;
        }
        visited.push(parent_process_id);
        if is_invocation_boundary(&parent.image) {
            return Some(images);
        }
        images.push(parent.image.clone());
        parent_process_id = parent.parent_process_id;
        child_creation_time = parent_creation_time;
    }
    None
}

fn unique_process(entries: &[ProcessEntry], process_id: u32) -> Option<&ProcessEntry> {
    let mut matching = entries
        .iter()
        .filter(|entry| entry.process_id == process_id);
    let entry = matching.next()?;
    matching.next().is_none().then_some(entry)
}

fn process_creation_time(process_id: u32) -> io::Result<u64> {
    // SAFETY: the PID is data from the bounded Tool Help snapshot. The call retains no borrowed
    // memory, and a successful non-null handle is owned by `ProcessHandle`.
    let process = unsafe { OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, 0, process_id) };
    if process.is_null() {
        return Err(io::Error::last_os_error());
    }
    let process = ProcessHandle(process);
    let mut creation = FILETIME {
        dwLowDateTime: 0,
        dwHighDateTime: 0,
    };
    let mut exit = creation;
    let mut kernel = creation;
    let mut user = creation;
    // SAFETY: every pointer names distinct writable `FILETIME` storage for the duration of the
    // call, the API retains none of them, and `process` keeps the queried process object alive.
    let observed = unsafe {
        GetProcessTimes(
            process.0,
            &raw mut creation,
            &raw mut exit,
            &raw mut kernel,
            &raw mut user,
        )
    };
    if observed == 0 {
        return Err(io::Error::last_os_error());
    }
    Ok((u64::from(creation.dwHighDateTime) << 32) | u64::from(creation.dwLowDateTime))
}

/// Returns whether ancestors above this process cannot be the prompt that regains control.
///
/// Windows terminal and session bootstrap processes commonly retain the PID of a short-lived
/// launcher. Treating that expected dead launcher as an incomplete shell chain would make every
/// direct invocation unknown. The names are presentation evidence only: they grant no cleanup or
/// process authority.
fn is_invocation_boundary(image: &OsStr) -> bool {
    let name = Path::new(image).file_name().unwrap_or(OsStr::new(""));
    let Some(name) = name.to_str() else {
        return false;
    };
    [
        "windowsterminal.exe",
        "windowsterminalpreview.exe",
        "openconsole.exe",
        "explorer.exe",
        "services.exe",
        "winlogon.exe",
        "wininit.exe",
    ]
    .iter()
    .any(|boundary| name.eq_ignore_ascii_case(boundary))
}

struct SnapshotHandle(HANDLE);

impl Drop for SnapshotHandle {
    fn drop(&mut self) {
        // SAFETY: this is the owned non-invalid handle returned by `CreateToolhelp32Snapshot`, and
        // `Drop` runs exactly once.
        unsafe {
            CloseHandle(self.0);
        }
    }
}

struct ProcessHandle(HANDLE);

impl Drop for ProcessHandle {
    fn drop(&mut self) {
        // SAFETY: `self.0` is the owned non-null handle returned by `OpenProcess`, and this guard is
        // neither `Clone` nor `Copy`, so it closes exactly once.
        unsafe {
            CloseHandle(self.0);
        }
    }
}

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
    let token = super::windows::console_event_token();
    decode_console_event(raw_event)
        .is_some_and(|kind| super::windows::record_console_event(token, kind))
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
    fn process(process_id: u32, parent_process_id: u32, image: &str) -> ProcessEntry {
        ProcessEntry {
            process_id,
            parent_process_id,
            image: OsString::from(image),
        }
    }

    fn ancestors(entries: &[ProcessEntry], current_process_id: u32) -> Option<Vec<OsString>> {
        ancestor_images_with(entries, current_process_id, |process_id| {
            Some(u64::MAX - u64::from(process_id))
        })
    }

    #[test]
    fn ancestor_walk_requires_a_supported_prompt_boundary() {
        let entries = [
            process(10, 20, "asm.exe"),
            process(20, 30, "wrapper.exe"),
            process(30, 0, "pwsh.exe"),
        ];

        assert_eq!(ancestors(&entries, 10), None);
    }

    #[test]
    fn ancestor_walk_stops_at_a_supported_prompt_boundary() {
        let entries = [
            process(10, 20, "asm.exe"),
            process(20, 30, "pwsh.exe"),
            process(30, 40, "WindowsTerminal.exe"),
        ];

        assert_eq!(
            ancestors(&entries, 10),
            Some(vec![OsString::from("pwsh.exe")])
        );
    }

    #[test]
    fn ancestor_walk_rejects_missing_duplicate_and_cyclic_process_evidence() {
        assert_eq!(
            ancestors(
                &[process(10, 20, "asm.exe"), process(20, 30, "pwsh.exe")],
                10
            ),
            None
        );
        assert_eq!(
            ancestors(
                &[
                    process(10, 20, "asm.exe"),
                    process(20, 0, "pwsh.exe"),
                    process(20, 0, "reused.exe"),
                ],
                10
            ),
            None
        );
        assert_eq!(
            ancestors(
                &[process(10, 20, "asm.exe"), process(20, 10, "pwsh.exe")],
                10
            ),
            None
        );
    }

    #[test]
    fn ancestor_walk_rejects_a_reused_parent_process_id() {
        let entries = [
            process(10, 20, "asm.exe"),
            process(20, 30, "pwsh.exe"),
            process(30, 40, "WindowsTerminal.exe"),
        ];

        assert_eq!(
            ancestor_images_with(&entries, 10, |process_id| match process_id {
                10 => Some(100),
                // The alleged parent was created after its child, which can only describe a reused
                // numeric PID rather than the process instance that launched SkillMount.
                20 => Some(200),
                30 => Some(50),
                _ => None,
            }),
            None
        );
    }

    #[test]
    fn ancestor_walk_rejects_unavailable_process_time_evidence() {
        let entries = [
            process(10, 20, "asm.exe"),
            process(20, 30, "pwsh.exe"),
            process(30, 40, "WindowsTerminal.exe"),
        ];

        assert_eq!(
            ancestor_images_with(&entries, 10, |process_id| {
                (process_id == 10).then_some(100)
            }),
            None
        );
    }

    #[test]
    fn ancestor_walk_rejects_a_chain_beyond_the_bound() {
        let last = u32::try_from(MAX_ANCESTOR_DEPTH + 2).expect("test depth fits in u32");
        let entries = (1..=last)
            .map(|process_id| {
                let parent_process_id = if process_id == last {
                    0
                } else {
                    process_id + 1
                };
                process(process_id, parent_process_id, "wrapper.exe")
            })
            .collect::<Vec<_>>();

        assert_eq!(ancestors(&entries, 1), None);
    }
}
