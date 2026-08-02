//! Audited Windows console-control boundary.

#![allow(unsafe_code)]

use std::io;

use windows_sys::Win32::System::Console::{CTRL_BREAK_EVENT, GenerateConsoleCtrlEvent};

pub(super) fn generate_console_break(process_group_id: u32) -> io::Result<()> {
    // SAFETY: `GenerateConsoleCtrlEvent` receives two integer values and retains no borrowed
    // memory. The caller supplies the live child PID created with `CREATE_NEW_PROCESS_GROUP`,
    // which Windows defines as that group's identifier.
    let generated = unsafe { GenerateConsoleCtrlEvent(CTRL_BREAK_EVENT, process_group_id) };
    if generated == 0 {
        Err(io::Error::last_os_error())
    } else {
        Ok(())
    }
}
