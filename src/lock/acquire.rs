//! Operating-system advisory locks over hashed resource keys.
//!
//! The property that matters is what counts as evidence that another session is alive. A lock
//! *file* is not evidence: it survives a crash, so treating its existence as liveness would make
//! every force-kill permanently block the next session, and treating its absence as death would let
//! two sessions mutate one store. Only a held advisory lock answers the question, because the
//! kernel releases it when the holder's handle closes for any reason, including `SIGKILL` and power
//! loss.
//!
//! The file's *contents* are diagnostics and nothing else. They say who took the lock so an
//! operator can find the process; no code path reads them to decide anything.
//!
//! Acquisition is always in sorted key order. Two sessions that discover the same resources in
//! opposite orders therefore request them in the same order and cannot deadlock against each other.

use std::collections::BTreeMap;
use std::fmt::Write as _;
use std::fs::{File, OpenOptions};
use std::io::Read as _;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use fs4::{FileExt, TryLockError};
use sha2::{Digest, Sha256};

use crate::error::AppError;
use crate::state;

use super::{LockAccess, LockKey, LockResource};

/// Environment variable that overrides how long acquisition waits.
///
/// Concurrency behaviour is only observable across processes, so an integration test has to be able
/// to make a contended lock fail promptly instead of blocking for the production timeout.
pub const LOCK_WAIT_OVERRIDE: &str = "SKILLMOUNT_LOCK_WAIT_MS";

/// How long acquisition waits for a contended lock before reporting a temporary failure.
///
/// Long enough that a session finishing its cleanup is waited out, short enough that a stuck one is
/// reported rather than hung on.
const DEFAULT_WAIT: Duration = Duration::from_secs(10);

/// How often a contended lock is retried.
const POLL_INTERVAL: Duration = Duration::from_millis(50);

/// Maximum owner records inspected for one contended key.
const MAX_HOLDER_RECORDS: usize = 8;

/// Maximum bytes accepted from one advisory owner record.
const MAX_HOLDER_BYTES: u64 = 1024;

/// How long acquisition waits, and how often it retries.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct LockPolicy {
    /// Total time to spend waiting for one contended lock.
    pub wait: Duration,
    /// Interval between attempts.
    pub poll: Duration,
}

impl LockPolicy {
    /// Returns the policy a session uses, honouring [`LOCK_WAIT_OVERRIDE`].
    #[must_use]
    pub fn from_env() -> Self {
        let wait = std::env::var_os(LOCK_WAIT_OVERRIDE)
            .and_then(|value| value.to_str().and_then(|text| text.parse::<u64>().ok()))
            .map_or(DEFAULT_WAIT, Duration::from_millis);
        Self {
            wait,
            poll: POLL_INTERVAL,
        }
    }

    /// Returns a policy that never waits, used where contention means "not eligible" rather than
    /// "try harder".
    ///
    /// Recovery uses it: a lock it cannot take immediately belongs to a session that is still
    /// running, and waiting for that session would only delay reporting the fact.
    #[must_use]
    pub const fn immediate() -> Self {
        Self {
            wait: Duration::ZERO,
            poll: POLL_INTERVAL,
        }
    }
}

/// Who took a lock, written into its owner sidecar for diagnostics only.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LockOwner {
    /// Transaction the lock belongs to, or a placeholder before one is opened.
    pub transaction: String,
    /// Process holding the lock.
    pub pid: u32,
}

impl LockOwner {
    /// Describes a lock taken before a transaction id exists.
    #[must_use]
    pub fn preliminary() -> Self {
        Self {
            transaction: "<pending>".to_owned(),
            pid: std::process::id(),
        }
    }

    /// Describes a lock taken on behalf of a transaction.
    #[must_use]
    pub fn for_transaction(transaction: &crate::journal::TransactionId) -> Self {
        Self {
            transaction: transaction.to_string(),
            pid: std::process::id(),
        }
    }
}

/// One held advisory lock.
///
/// Dropping it releases the lock. The handle is retained rather than the lock being released
/// explicitly at each exit point, so an early return or a panic cannot leave a resource locked.
#[derive(Debug)]
struct HeldLock {
    file: File,
    access: LockAccess,
    description: PathBuf,
}

impl Drop for HeldLock {
    fn drop(&mut self) {
        clear_holder(&self.description);
        // Closing the handle releases the lock on both platforms; unlocking first makes the release
        // explicit and independent of when the handle is finalized.
        let _ = FileExt::unlock(&self.file);
    }
}

/// Every advisory lock a session currently holds.
///
/// The set is append-only while it is live. A later addition is accepted only when every new key
/// sorts after the keys already held. When discovery expands the set with an earlier key, the
/// application drops this set and reacquires the complete union in one sorted pass before it
/// re-runs recovery and filesystem inspection.
#[derive(Debug, Default)]
pub struct HeldLocks {
    locks: BTreeMap<LockKey, HeldLock>,
}

/// Why a lock could not be taken.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LockContention {
    /// Key that is unavailable.
    pub key: LockKey,
    /// Access this session tried to acquire.
    pub access: LockAccess,
    /// Lock file protecting it.
    pub path: PathBuf,
    /// Resource paths this session mapped onto the key.
    pub resources: Vec<PathBuf>,
    /// Diagnostics the holder recorded, when they are readable.
    pub holder: Option<String>,
}

/// Kernel-backed state observed without creating a lock file or holder record.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum AdvisoryLockState {
    /// No process holds this key, or its lock file has never existed.
    Free,
    /// The operating system refused the advisory lock because another handle holds it.
    Held {
        /// Best-effort owner text; diagnostic only and never liveness evidence.
        holder: Option<String>,
    },
}

/// One resource key observed by `doctor` without changing lock state.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct AdvisoryLockObservation {
    pub(crate) key: LockKey,
    pub(crate) access: LockAccess,
    pub(crate) path: PathBuf,
    pub(crate) resources: Vec<PathBuf>,
    pub(crate) state: AdvisoryLockState,
}

/// Observes every deduplicated resource lock without creating files or owner sidecars.
pub(crate) fn observe(
    resources: &[LockResource],
) -> Result<Vec<AdvisoryLockObservation>, AppError> {
    let directory = state::lock_base()?;
    let mut observations = Vec::new();
    for request in sorted_requests(resources) {
        let path = directory.join(request.key.file_name());
        let file = match OpenOptions::new().read(true).write(true).open(&path) {
            Ok(file) => file,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                observations.push(AdvisoryLockObservation {
                    key: request.key,
                    access: request.access,
                    path,
                    resources: request.resources,
                    state: AdvisoryLockState::Free,
                });
                continue;
            }
            Err(error) => {
                return Err(AppError::Filesystem(format!(
                    "cannot open existing lock file {} for observation: {error}",
                    path.display()
                )));
            }
        };
        // An exclusive probe conflicts with either a shared observer or an exclusive mutator.
        // This reads kernel state without creating a lock file or owner record.
        let state = match FileExt::try_lock(&file) {
            Ok(()) => {
                FileExt::unlock(&file).map_err(|error| {
                    AppError::Filesystem(format!(
                        "cannot release observed lock {}: {error}",
                        path.display()
                    ))
                })?;
                AdvisoryLockState::Free
            }
            Err(TryLockError::WouldBlock) => AdvisoryLockState::Held {
                holder: read_holder(&request.key),
            },
            Err(TryLockError::Error(error)) => {
                return Err(AppError::Filesystem(format!(
                    "cannot observe advisory lock {}: {error}",
                    path.display()
                )));
            }
        };
        observations.push(AdvisoryLockObservation {
            key: request.key,
            access: request.access,
            path,
            resources: request.resources,
            state,
        });
    }
    Ok(observations)
}

impl LockContention {
    /// Renders the operator-facing explanation.
    #[must_use]
    pub fn describe(&self) -> String {
        let resources = self
            .resources
            .iter()
            .map(|path| path.display().to_string())
            .collect::<Vec<_>>()
            .join(", ");
        let holder = self
            .holder
            .as_deref()
            .map_or_else(String::new, |holder| format!(" (held by {holder})"));
        format!(
            "another SkillMount session blocks {} access to {resources}{holder}; nothing was changed for this session",
            self.access.label()
        )
    }
}

/// Result of an immediate attempt to claim the locks this set does not already satisfy.
#[derive(Debug)]
pub enum MissingLockOutcome {
    /// Every missing key was acquired.
    Acquired(HeldLocks),
    /// Another process holds a required key.
    Contended(LockContention),
    /// This set holds a key too weakly and must be released before reacquisition.
    RequiresReacquire,
}

impl HeldLocks {
    /// Takes every lock the resources need, in sorted key order, waiting per `policy`.
    ///
    /// # Errors
    ///
    /// Returns [`AppError::Temporary`] when a lock stays unavailable for the whole wait, naming the
    /// contended resource, and [`AppError::Filesystem`] when the lock directory or file cannot be
    /// created. Locks taken before the failure are released as the partially built set is dropped.
    pub fn acquire(
        resources: &[LockResource],
        policy: LockPolicy,
        owner: &LockOwner,
    ) -> Result<Self, AppError> {
        let mut held = Self::default();
        held.acquire_more(resources, policy, owner)?;
        Ok(held)
    }

    /// Takes any request in `resources` that is not already satisfied, preserving global order.
    ///
    /// # Errors
    ///
    /// Returns the same errors as [`HeldLocks::acquire`]. A missing earlier key or an access
    /// promotion is an internal error: the application must release and reacquire the complete
    /// strongest set instead.
    pub fn acquire_more(
        &mut self,
        resources: &[LockResource],
        policy: LockPolicy,
        owner: &LockOwner,
    ) -> Result<(), AppError> {
        let missing = self.missing_requests(resources);
        if self.requires_reacquire_for_requests(&missing) {
            return Err(AppError::Internal(
                "additional resource locks would violate global acquisition order or require an \
                 access promotion; release and reacquire the complete strongest lock set before \
                 continuing"
                    .to_owned(),
            ));
        }
        for request in missing {
            match take(&request.key, request.access, policy, owner)? {
                Taken::Held(lock) => {
                    self.locks.insert(request.key, lock);
                }
                Taken::Busy { path, holder } => {
                    return Err(AppError::Temporary(
                        LockContention {
                            key: request.key,
                            access: request.access,
                            path,
                            resources: request.resources,
                            holder,
                        }
                        .describe(),
                    ));
                }
            }
        }
        Ok(())
    }

    /// Returns whether satisfying `resources` requires moving backwards or strengthening a key.
    ///
    /// A caller that receives `true` must drop this set, reacquire the strongest union in a fresh
    /// sorted pass, then repeat recovery and filesystem inspection across the unlocked interval.
    #[must_use]
    pub fn requires_reacquire(&self, resources: &[LockResource]) -> bool {
        self.requires_reacquire_for_requests(&self.missing_requests(resources))
    }

    /// Takes every lock in `resources` only if all of them are free right now.
    ///
    /// Returns the contention that stopped it, without holding any locks acquired earlier in the
    /// attempt.
    ///
    /// # Errors
    ///
    /// Returns [`AppError::Filesystem`] when a lock file cannot be created.
    pub fn try_acquire_all(
        resources: &[LockResource],
        owner: &LockOwner,
    ) -> Result<Result<Self, LockContention>, AppError> {
        match Self::default().try_acquire_missing(resources, owner)? {
            MissingLockOutcome::Acquired(held) => Ok(Ok(held)),
            MissingLockOutcome::Contended(contention) => Ok(Err(contention)),
            MissingLockOutcome::RequiresReacquire => Err(AppError::Internal(
                "an empty lock set cannot require access promotion".to_owned(),
            )),
        }
    }

    /// Immediately takes requests this set does not already satisfy.
    ///
    /// The returned set contains only newly acquired keys. If this set already holds a key with
    /// observation access while mutation is required, no in-place upgrade is attempted and
    /// [`MissingLockOutcome::RequiresReacquire`] is returned.
    ///
    /// # Errors
    ///
    /// Returns [`AppError::Filesystem`] when a lock file cannot be created.
    pub fn try_acquire_missing(
        &self,
        resources: &[LockResource],
        owner: &LockOwner,
    ) -> Result<MissingLockOutcome, AppError> {
        let missing = self.missing_requests(resources);
        if self.requires_reacquire_for_requests(&missing) {
            // Probe only genuinely new keys before asking the caller to release anything. A live
            // transaction commonly shares observation keys with this session while holding a
            // distinct mutation key; reporting that contention preserves independent-session
            // concurrency. Promoted keys are excluded because our own observation would make an
            // exclusive probe contend with itself.
            let mut probes = Self::default();
            for request in missing
                .iter()
                .filter(|request| !self.locks.contains_key(&request.key))
            {
                match take(&request.key, request.access, LockPolicy::immediate(), owner)? {
                    Taken::Held(lock) => {
                        probes.locks.insert(request.key.clone(), lock);
                    }
                    Taken::Busy { path, holder } => {
                        return Ok(MissingLockOutcome::Contended(LockContention {
                            key: request.key.clone(),
                            access: request.access,
                            path,
                            resources: request.resources.clone(),
                            holder,
                        }));
                    }
                }
            }
            return Ok(MissingLockOutcome::RequiresReacquire);
        }

        let mut held = Self::default();
        for request in missing {
            match take(&request.key, request.access, LockPolicy::immediate(), owner)? {
                Taken::Held(lock) => {
                    held.locks.insert(request.key, lock);
                }
                Taken::Busy { path, holder } => {
                    return Ok(MissingLockOutcome::Contended(LockContention {
                        key: request.key,
                        access: request.access,
                        path,
                        resources: request.resources,
                        holder,
                    }));
                }
            }
        }
        Ok(MissingLockOutcome::Acquired(held))
    }

    /// Absorbs another set, so recovery locks stay held for the rest of the session.
    pub fn absorb(&mut self, other: Self) {
        for (key, lock) in other.locks {
            if let Some(existing) = self.locks.get(&key) {
                debug_assert!(existing.access.satisfies(lock.access));
                drop(lock);
            } else {
                self.locks.insert(key, lock);
            }
        }
    }

    /// Returns whether `key` is currently held by this session.
    #[must_use]
    pub fn holds(&self, key: &LockKey) -> bool {
        self.locks.contains_key(key)
    }

    /// Returns whether `key` is held with access sufficient for `required`.
    #[must_use]
    pub fn holds_at_least(&self, key: &LockKey, required: LockAccess) -> bool {
        self.locks
            .get(key)
            .is_some_and(|held| held.access.satisfies(required))
    }

    /// Returns whether every key the resources need is held strongly enough.
    #[must_use]
    pub fn holds_all(&self, resources: &[LockResource]) -> bool {
        sorted_requests(resources)
            .iter()
            .all(|request| self.holds_at_least(&request.key, request.access))
    }

    /// Returns the held keys in acquisition order.
    pub fn keys(&self) -> impl Iterator<Item = &LockKey> {
        self.locks.keys()
    }

    fn missing_requests(&self, resources: &[LockResource]) -> Vec<LockRequest> {
        sorted_requests(resources)
            .into_iter()
            .filter(|request| !self.holds_at_least(&request.key, request.access))
            .collect()
    }

    fn requires_reacquire_for_requests(&self, missing: &[LockRequest]) -> bool {
        if missing
            .iter()
            .any(|request| self.locks.contains_key(&request.key))
        {
            return true;
        }
        let Some(highest_held) = self.locks.last_key_value().map(|(key, _)| key) else {
            return false;
        };
        missing
            .first()
            .is_some_and(|request| &request.key < highest_held)
    }
}

/// Outcome of one attempt on one key.
enum Taken {
    Held(HeldLock),
    Busy {
        path: PathBuf,
        holder: Option<String>,
    },
}

/// One deduplicated lock request.
#[derive(Debug, Clone, PartialEq, Eq)]
struct LockRequest {
    key: LockKey,
    access: LockAccess,
    resources: Vec<PathBuf>,
}

/// Collapses resources onto their keys, retaining paths and the strongest access.
///
/// Deduplication is what makes a layout whose discovery entry links to its backing store take one
/// physical lock rather than deadlocking against itself. Access is deliberately not part of the
/// key: observation and mutation requests must meet on the same lock file.
fn sorted_requests(resources: &[LockResource]) -> Vec<LockRequest> {
    let mut grouped: BTreeMap<LockKey, (LockAccess, Vec<PathBuf>)> = BTreeMap::new();
    for resource in resources {
        for key in resource.lock_keys() {
            let (access, paths) = grouped
                .entry(key)
                .or_insert_with(|| (resource.access, Vec::new()));
            *access = (*access).max(resource.access);
            if !paths.contains(&resource.path) {
                paths.push(resource.path.clone());
            }
        }
    }
    grouped
        .into_iter()
        .map(|(key, (access, resources))| LockRequest {
            key,
            access,
            resources,
        })
        .collect()
}

/// Exposes acquisition order and folded access to unit tests without taking real locks.
#[cfg(test)]
pub(crate) fn sorted_keys_for_test(
    resources: &[LockResource],
) -> Vec<(LockKey, LockAccess, Vec<PathBuf>)> {
    sorted_requests(resources)
        .into_iter()
        .map(|request| (request.key, request.access, request.resources))
        .collect()
}

fn take(
    key: &LockKey,
    access: LockAccess,
    policy: LockPolicy,
    owner: &LockOwner,
) -> Result<Taken, AppError> {
    let directory = state::lock_base()?;
    state::ensure_private_directory(&directory)?;
    let path = directory.join(key.file_name());

    // The lock file stays empty; holder descriptions live in a bounded per-key directory. Windows
    // byte-range locks are mandatory, so a locked file cannot be read through another handle.
    let file = OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .truncate(false)
        .open(&path)
        .map_err(|error| {
            AppError::Filesystem(format!("cannot open lock file {}: {error}", path.display()))
        })?;

    let deadline = Instant::now() + policy.wait;
    loop {
        // Fully qualified so these always reach `fs4`. `std::fs::File` gained inherent lock methods
        // after SkillMount's pinned Rust 1.85.0, and inherent methods would silently raise the MSRV.
        let attempt = match access {
            LockAccess::Observe => FileExt::try_lock_shared(&file),
            LockAccess::Mutate => FileExt::try_lock(&file),
        };
        match attempt {
            Ok(()) => break,
            Err(TryLockError::WouldBlock) => {
                if Instant::now() >= deadline {
                    return Ok(Taken::Busy {
                        holder: read_holder(key),
                        path,
                    });
                }
                std::thread::sleep(policy.poll);
            }
            Err(TryLockError::Error(error)) => {
                return Err(AppError::Filesystem(format!(
                    "cannot acquire {} access to {}: {error}",
                    access.label(),
                    path.display()
                )));
            }
        }
    }

    state::restrict_to_owner(&path)?;
    let description = holder_path(key, owner)?;
    write_holder(&description, owner);
    Ok(Taken::Held(HeldLock {
        file,
        access,
        description,
    }))
}

/// Returns the owner-record directory for `key`.
fn holder_directory(key: &LockKey) -> Result<PathBuf, AppError> {
    Ok(state::lock_base()?.join(format!("{key}.owners")))
}

/// Returns this holder's transaction-specific sidecar path.
fn holder_path(key: &LockKey, owner: &LockOwner) -> Result<PathBuf, AppError> {
    Ok(holder_directory(key)?.join(format!("{}.owner", holder_token(owner))))
}

fn holder_token(owner: &LockOwner) -> String {
    let mut hasher = Sha256::new();
    hasher.update((owner.transaction.len() as u64).to_be_bytes());
    hasher.update(owner.transaction.as_bytes());
    hasher.update(owner.pid.to_be_bytes());
    let mut rendered = String::with_capacity(64);
    for byte in hasher.finalize() {
        let _ = write!(rendered, "{byte:02x}");
    }
    rendered
}

/// Records one holder for diagnostics only.
fn write_holder(path: &Path, owner: &LockOwner) {
    let Some(directory) = path.parent() else {
        return;
    };
    if state::ensure_private_directory(directory).is_err() {
        return;
    }
    let description = format!(
        "transaction={} pid={}\nthis file records one advisory holder; neither it nor the lock file is evidence that anyone still holds it\n",
        owner.transaction, owner.pid
    );
    if std::fs::write(path, description).is_ok() {
        let _ = state::restrict_to_owner(path);
    }
}

/// Removes only this holder's description once its handle releases the lock.
fn clear_holder(path: &Path) {
    let _ = std::fs::remove_file(path);
}

/// Reads a bounded set of holder descriptions, all advisory and potentially stale.
fn read_holder(key: &LockKey) -> Option<String> {
    let entries = std::fs::read_dir(holder_directory(key).ok()?).ok()?;
    let mut holders = Vec::with_capacity(MAX_HOLDER_RECORDS + 1);
    for entry in entries.take(MAX_HOLDER_RECORDS + 1).flatten() {
        if !entry.file_type().ok().is_some_and(|kind| kind.is_file()) {
            continue;
        }
        if let Some(holder) = read_holder_record(&entry.path()) {
            holders.push(holder);
        }
    }
    holders.sort();
    holders.dedup();
    let omitted = holders.len() > MAX_HOLDER_RECORDS;
    holders.truncate(MAX_HOLDER_RECORDS);
    if omitted {
        holders.push("additional holder records omitted".to_owned());
    }
    (!holders.is_empty()).then(|| holders.join("; "))
}

fn read_holder_record(path: &Path) -> Option<String> {
    let file = File::open(path).ok()?;
    if file.metadata().ok()?.len() > MAX_HOLDER_BYTES {
        return None;
    }
    let mut bytes = Vec::with_capacity(
        usize::try_from(MAX_HOLDER_BYTES).expect("holder read bound fits usize"),
    );
    file.take(MAX_HOLDER_BYTES + 1)
        .read_to_end(&mut bytes)
        .ok()?;
    if bytes.len() as u64 > MAX_HOLDER_BYTES {
        return None;
    }
    let contents = String::from_utf8(bytes).ok()?;
    let first = contents.lines().next()?.trim();
    (!first.is_empty()).then(|| first.to_owned())
}
