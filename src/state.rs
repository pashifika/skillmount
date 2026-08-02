//! Locations `SkillMount` uses outside the project for session, lock, and recovery state.
//!
//! Resolution is separate from creation on purpose. [`transaction_base`], [`lock_base`], and
//! [`session_root_base`] only compute where state *would* live, which is what read-only planning
//! needs in order to describe a staging layout and to look for stale transactions without
//! recovering them. A caller that is past the mutation boundary calls
//! [`ensure_private_directory`] explicitly, so no read-only path can create one by accident.

use std::fs;
use std::path::{Path, PathBuf};

use crate::error::AppError;

/// Directory component that stands in for a session identifier before a transaction opens.
///
/// A preliminary plan has no session identifier: one is minted when the transaction starts, and
/// inventing a value here would make `--dry-run` output differ between two identical runs. The
/// angle brackets are deliberate. They are invalid in a Windows filename, so the placeholder can
/// never collide with a real directory on the platform where most sessions run.
pub const PENDING_SESSION: &str = "<session-id>";

/// Environment variable that redirects every application-state location at once.
///
/// Recovery and locking are only observable through state that outlives one process, so a test
/// that cannot redirect that state either pollutes the developer's real application-support
/// directory or contends with a concurrent test run. Redirecting `HOME` is not enough: it changes
/// meaning per platform and a Windows host resolves `LOCALAPPDATA` instead.
///
/// The variable is read on every call rather than cached, because integration tests set it per
/// child process and a cached value would leak between them.
pub const STATE_ROOT_OVERRIDE: &str = "SKILLMOUNT_STATE_DIR";

/// Returns the user's home directory.
///
/// # Errors
///
/// Returns [`AppError::MissingInput`] when the platform's home variable is unset.
pub fn user_home() -> Result<PathBuf, AppError> {
    let variable = if cfg!(windows) { "USERPROFILE" } else { "HOME" };
    required_directory_var(variable)
}

/// Returns the root of every persistent application-state location.
///
/// Recovery state must survive a reboot and a cache sweep, so macOS uses application support
/// rather than `~/Library/Caches`: a cleanup utility that erases caches would otherwise delete the
/// journals that prove which entries `SkillMount` owns. Windows has one local application-data
/// location and uses it for both.
///
/// # Errors
///
/// Returns [`AppError::MissingInput`] when the platform's state variable is unset.
pub fn state_root() -> Result<PathBuf, AppError> {
    if let Some(override_root) = override_root() {
        return Ok(override_root);
    }
    #[cfg(windows)]
    {
        Ok(local_app_data()?.join("skillmount"))
    }
    #[cfg(not(windows))]
    {
        Ok(user_home()?.join("Library/Application Support/skillmount"))
    }
}

/// Returns the base directory that holds one staging root per session.
///
/// Sessions are disposable, so they live under cache storage on macOS. Windows has no separate
/// cache location, so local application data is used. An explicit state override folds sessions
/// back under it, because a test that redirects state expects one directory to inspect.
///
/// # Errors
///
/// Returns [`AppError::MissingInput`] when the platform's state variable is unset.
pub fn session_root_base() -> Result<PathBuf, AppError> {
    if let Some(override_root) = override_root() {
        return Ok(override_root.join("sessions"));
    }
    #[cfg(windows)]
    {
        Ok(local_app_data()?.join("skillmount").join("sessions"))
    }
    #[cfg(not(windows))]
    {
        Ok(user_home()?.join("Library/Caches/skillmount/sessions"))
    }
}

/// Returns the directory that holds transaction journals.
///
/// # Errors
///
/// Returns [`AppError::MissingInput`] when the platform's state variable is unset.
pub fn transaction_base() -> Result<PathBuf, AppError> {
    Ok(state_root()?.join("transactions"))
}

/// Returns the directory that holds advisory lock files.
///
/// Lock files sit beside journals rather than under a temporary directory: a host that clears
/// temporary storage while a session is active would let a second session take a lock the first
/// one still holds, which is exactly the serialization the lock exists to provide.
///
/// # Errors
///
/// Returns [`AppError::MissingInput`] when the platform's state variable is unset.
pub fn lock_base() -> Result<PathBuf, AppError> {
    Ok(state_root()?.join("locks"))
}

/// Creates `path` and every missing parent, restricting access to the current user where the
/// platform supports it.
///
/// Journals name every directory a session owns and lock files carry session diagnostics, so both
/// are readable descriptions of what another user could interfere with. Unix applies `0o700` to
/// each directory this call creates. Windows relies on the per-user ACL that `LOCALAPPDATA`
/// already carries; tightening it further needs a security descriptor, which has no safe API and
/// would widen the crate's `unsafe` boundary beyond the two audited link modules of ADR 0011.
///
/// # Errors
///
/// Returns [`AppError::Filesystem`] when the directory cannot be created or its mode cannot be set.
pub fn ensure_private_directory(path: &Path) -> Result<(), AppError> {
    fs::create_dir_all(path).map_err(|error| {
        AppError::Filesystem(format!(
            "cannot create state directory {}: {error}",
            path.display()
        ))
    })?;
    restrict_to_owner(path)
}

/// Applies owner-only permissions to a directory or file that already exists.
///
/// # Errors
///
/// Returns [`AppError::Filesystem`] when the mode cannot be applied.
#[cfg(unix)]
pub fn restrict_to_owner(path: &Path) -> Result<(), AppError> {
    use std::os::unix::fs::PermissionsExt;

    let metadata = fs::symlink_metadata(path).map_err(|error| {
        AppError::Filesystem(format!("cannot inspect {}: {error}", path.display()))
    })?;
    let mode = if metadata.is_dir() { 0o700 } else { 0o600 };
    fs::set_permissions(path, fs::Permissions::from_mode(mode)).map_err(|error| {
        AppError::Filesystem(format!(
            "cannot restrict {} to its owner: {error}",
            path.display()
        ))
    })
}

/// Applies owner-only permissions where the platform supports it.
///
/// # Errors
///
/// Never fails on Windows; the signature matches the Unix implementation.
#[cfg(windows)]
#[allow(clippy::unnecessary_wraps)]
pub fn restrict_to_owner(_path: &Path) -> Result<(), AppError> {
    Ok(())
}

/// Returns the redirected state root, ignoring an empty value so an unset-looking variable behaves
/// as unset.
fn override_root() -> Option<PathBuf> {
    #[cfg(test)]
    if let Some(path) = testing::current_override() {
        return Some(path);
    }
    std::env::var_os(STATE_ROOT_OVERRIDE)
        .map(PathBuf::from)
        .filter(|value| !value.as_os_str().is_empty())
}

/// In-process state redirection for unit tests.
///
/// `std::env::set_var` is `unsafe` in this edition and the crate denies `unsafe_code` outside the
/// two audited link modules, so a unit test cannot set [`STATE_ROOT_OVERRIDE`] the way an
/// integration test does through `Command::env`. This module provides the same redirection through
/// a safe, serialized handle instead.
#[cfg(test)]
pub(crate) mod testing {
    use std::path::{Path, PathBuf};
    use std::sync::{Mutex, MutexGuard, PoisonError, RwLock};

    /// Serializes every test that redirects state, because the redirection is process-wide.
    static SERIAL: Mutex<()> = Mutex::new(());
    /// The redirection currently in force, read by [`super::override_root`].
    static CURRENT: RwLock<Option<PathBuf>> = RwLock::new(None);

    /// Redirects every state location for as long as the guard lives.
    pub(crate) struct StateRootGuard {
        /// Held for the guard's lifetime; releasing it lets the next redirecting test run.
        _serial: MutexGuard<'static, ()>,
    }

    impl StateRootGuard {
        /// Points every state location at `root` and blocks other redirecting tests meanwhile.
        pub(crate) fn set(root: &Path) -> Self {
            // A test that fails while holding the guard poisons the mutex. Recovering the inner
            // value keeps one genuine failure from cascading into every later test.
            let guard = SERIAL.lock().unwrap_or_else(PoisonError::into_inner);
            *CURRENT.write().unwrap_or_else(PoisonError::into_inner) = Some(root.to_path_buf());
            Self { _serial: guard }
        }
    }

    impl Drop for StateRootGuard {
        fn drop(&mut self) {
            *CURRENT.write().unwrap_or_else(PoisonError::into_inner) = None;
        }
    }

    pub(super) fn current_override() -> Option<PathBuf> {
        CURRENT
            .read()
            .unwrap_or_else(PoisonError::into_inner)
            .clone()
    }
}

#[cfg(windows)]
fn local_app_data() -> Result<PathBuf, AppError> {
    required_directory_var("LOCALAPPDATA")
}

fn required_directory_var(variable: &str) -> Result<PathBuf, AppError> {
    std::env::var_os(variable)
        .map(PathBuf::from)
        .filter(|value| !value.as_os_str().is_empty())
        .ok_or_else(|| AppError::MissingInput {
            path: PathBuf::from(format!("${variable}")),
            reason: "the environment variable is unset or empty".to_owned(),
        })
}

#[cfg(test)]
mod tests {
    use super::testing::StateRootGuard;
    use super::{lock_base, session_root_base, state_root, transaction_base};
    use crate::test_support::TestDir;

    #[test]
    fn every_state_location_follows_one_override() {
        let fixture = TestDir::new("state-override");
        let _guard = StateRootGuard::set(fixture.path());

        let root = state_root().expect("an overridden root always resolves");

        assert_eq!(root, fixture.path());
        assert_eq!(transaction_base().unwrap(), root.join("transactions"));
        assert_eq!(lock_base().unwrap(), root.join("locks"));
        assert_eq!(
            session_root_base().unwrap(),
            root.join("sessions"),
            "a redirected run keeps sessions under the same root so one directory describes it"
        );
    }

    #[test]
    fn journals_and_locks_never_share_a_directory() {
        let fixture = TestDir::new("state-separate");
        let _guard = StateRootGuard::set(fixture.path());

        assert_ne!(transaction_base().unwrap(), lock_base().unwrap());
    }

    #[test]
    fn a_state_directory_is_created_with_every_missing_parent() {
        let fixture = TestDir::new("state-create");
        let nested = fixture.path().join("transactions/inner");

        super::ensure_private_directory(&nested).expect("state directories are creatable");
        // Creating one that already exists must stay a success: every mutating run calls this on
        // the same directory, and a second run failing would be an outage rather than a guard.
        super::ensure_private_directory(&nested).expect("creation is idempotent");

        assert!(nested.is_dir());
    }

    #[cfg(unix)]
    #[test]
    fn created_state_directories_are_owner_only() {
        use std::os::unix::fs::PermissionsExt;

        let fixture = TestDir::new("state-mode");
        let nested = fixture.path().join("transactions/inner");

        super::ensure_private_directory(&nested).expect("state directories are creatable");

        let mode = std::fs::metadata(&nested).unwrap().permissions().mode();
        assert_eq!(mode & 0o777, 0o700);
    }

    /// Windows has no mode to assert, but the restriction call must still be a total function.
    ///
    /// Tightening a Windows ACL needs a security descriptor, which has no safe API and would widen
    /// the crate's `unsafe` boundary past the two audited link modules of ADR 0011; `LOCALAPPDATA`
    /// already carries a per-user ACL. This test exists so the Windows branch is executed
    /// somewhere, rather than only the Unix one being covered.
    #[cfg(windows)]
    #[test]
    fn restricting_a_windows_state_directory_is_a_documented_no_op() {
        let fixture = TestDir::new("state-mode");
        let nested = fixture.path().join("transactions/inner");
        super::ensure_private_directory(&nested).expect("state directories are creatable");

        super::restrict_to_owner(&nested).expect("the Windows branch never fails");
        super::restrict_to_owner(&fixture.path().join("absent"))
            .expect("and never inspects the path");

        assert!(nested.is_dir());
    }
}
