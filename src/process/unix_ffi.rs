//! Audited Unix signal-registration boundary.

#![allow(unsafe_code)]

use std::io;
use std::sync::OnceLock;

use signal_hook::SigId;
use signal_hook::consts::signal::{SIGINT, SIGTERM};

use super::InterruptKind;

static INSTALLATION: OnceLock<Result<Registrations, StoredError>> = OnceLock::new();

pub(super) fn install() -> io::Result<()> {
    match INSTALLATION.get_or_init(register_handlers) {
        Ok(_) => Ok(()),
        Err(error) => Err(error.to_io()),
    }
}

fn register_handlers() -> Result<Registrations, StoredError> {
    let interrupt =
        register(SIGINT, InterruptKind::Interrupt).map_err(|error| StoredError::from_io(&error))?;
    let terminate = register(SIGTERM, InterruptKind::Terminate)
        .map_err(|error| StoredError::from_io(&error))?;
    Ok(Registrations {
        _interrupt: interrupt,
        _terminate: terminate,
    })
}

fn register(signal: i32, kind: InterruptKind) -> io::Result<SigId> {
    // SAFETY: the callback first snapshots the active event-session token, then performs only
    // lock-free atomic operations through `record_signal` or invokes signal-hook's
    // async-signal-safe default-action emulation for SIGINT/SIGTERM. It does not allocate, lock,
    // perform Rust I/O, or unwind across the signal boundary.
    unsafe {
        signal_hook::low_level::register(signal, move || {
            let token = super::unix::signal_token();
            super::unix::record_signal(token, signal, kind);
        })
    }
}

struct Registrations {
    _interrupt: SigId,
    _terminate: SigId,
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
