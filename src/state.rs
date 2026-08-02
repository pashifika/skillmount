//! Locations `SkillMount` uses outside the project for session and recovery state.
//!
//! Nothing here creates a directory. These functions only compute where state *would* live, which
//! is what read-only planning needs in order to describe a staging layout and to look for stale
//! transactions without recovering them.

use std::path::PathBuf;

use crate::error::AppError;

/// Directory component that stands in for a session identifier before a transaction opens.
///
/// A preliminary plan has no session identifier: one is minted when the transaction starts, and
/// inventing a value here would make `--dry-run` output differ between two identical runs. The
/// angle brackets are deliberate. They are invalid in a Windows filename, so the placeholder can
/// never collide with a real directory on the platform where most sessions run.
pub const PENDING_SESSION: &str = "<session-id>";

/// Returns the user's home directory.
///
/// # Errors
///
/// Returns [`AppError::MissingInput`] when the platform's home variable is unset.
pub fn user_home() -> Result<PathBuf, AppError> {
    let variable = if cfg!(windows) { "USERPROFILE" } else { "HOME" };
    std::env::var_os(variable)
        .map(PathBuf::from)
        .filter(|value| !value.as_os_str().is_empty())
        .ok_or_else(|| AppError::MissingInput {
            path: PathBuf::from(format!("${variable}")),
            reason: "the environment variable is unset or empty".to_owned(),
        })
}

/// Returns the base directory that holds one staging root per session.
///
/// Sessions are disposable, so they live under cache storage on macOS. Windows has no separate
/// cache location, so local application data is used.
///
/// # Errors
///
/// Returns [`AppError::MissingInput`] when the platform's state variable is unset.
#[cfg(not(windows))]
pub fn session_root_base() -> Result<PathBuf, AppError> {
    Ok(user_home()?.join("Library/Caches/skillmount/sessions"))
}

/// Returns the base directory that holds one staging root per session.
///
/// # Errors
///
/// Returns [`AppError::MissingInput`] when `LOCALAPPDATA` is unset.
#[cfg(windows)]
pub fn session_root_base() -> Result<PathBuf, AppError> {
    Ok(local_app_data()?.join("skillmount").join("sessions"))
}

/// Returns the directory that holds transaction journals.
///
/// Transactions are recovery state rather than disposable cache data, so macOS stores them under
/// application support instead of the cache location used for session roots.
///
/// # Errors
///
/// Returns [`AppError::MissingInput`] when the platform's state variable is unset.
#[cfg(not(windows))]
pub fn transaction_base() -> Result<PathBuf, AppError> {
    Ok(user_home()?.join("Library/Application Support/skillmount/transactions"))
}

/// Returns the directory that holds transaction journals.
///
/// # Errors
///
/// Returns [`AppError::MissingInput`] when `LOCALAPPDATA` is unset.
#[cfg(windows)]
pub fn transaction_base() -> Result<PathBuf, AppError> {
    Ok(local_app_data()?.join("skillmount").join("transactions"))
}

#[cfg(windows)]
fn local_app_data() -> Result<PathBuf, AppError> {
    std::env::var_os("LOCALAPPDATA")
        .map(PathBuf::from)
        .filter(|value| !value.as_os_str().is_empty())
        .ok_or_else(|| AppError::MissingInput {
            path: PathBuf::from("$LOCALAPPDATA"),
            reason: "the environment variable is unset or empty".to_owned(),
        })
}
