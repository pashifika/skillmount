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
use std::fs::{File, OpenOptions};
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use fs4::{FileExt, TryLockError};

use crate::error::AppError;
use crate::state;

use super::{LockKey, LockResource};

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
    /// Lock file protecting it.
    pub path: PathBuf,
    /// Resource paths this session mapped onto the key.
    pub resources: Vec<PathBuf>,
    /// Diagnostics the holder recorded, when they are readable.
    pub holder: Option<String>,
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
            "another SkillMount session holds {resources}{holder}; nothing was changed for this session"
        )
    }
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

    /// Takes any lock in `resources` that is not already held, keeping the same sorted order.
    ///
    /// # Errors
    ///
    /// Returns the same errors as [`HeldLocks::acquire`].
    pub fn acquire_more(
        &mut self,
        resources: &[LockResource],
        policy: LockPolicy,
        owner: &LockOwner,
    ) -> Result<(), AppError> {
        let missing = self.missing_keys(resources);
        if self.requires_reacquire_for_keys(&missing) {
            return Err(AppError::Internal(
                "additional resource locks would violate the global acquisition order; release \
                 and reacquire the complete lock set before continuing"
                    .to_owned(),
            ));
        }
        for (key, paths) in missing {
            match take(&key, policy, owner)? {
                Taken::Held(lock) => {
                    self.locks.insert(key, lock);
                }
                Taken::Busy { path, holder } => {
                    return Err(AppError::Temporary(
                        LockContention {
                            key,
                            path,
                            resources: paths,
                            holder,
                        }
                        .describe(),
                    ));
                }
            }
        }
        Ok(())
    }

    /// Returns whether acquiring the missing keys for `resources` would move backwards in the
    /// global order.
    ///
    /// A caller that receives `true` must drop this set, reacquire the union in a fresh sorted
    /// pass, then repeat any recovery and filesystem inspection performed across the unlocked
    /// interval. Merely unlocking and continuing with a plan built before the gap would make the
    /// order safe while leaving the plan racy.
    #[must_use]
    pub fn requires_reacquire(&self, resources: &[LockResource]) -> bool {
        self.requires_reacquire_for_keys(&self.missing_keys(resources))
    }

    /// Takes every lock in `resources` only if all of them are free right now.
    ///
    /// Returns the contention that stopped it, without holding any of the locks it managed to take.
    /// This is the lock-availability part of recovery eligibility: free locks prove there is no
    /// live wrapper owner. Journal status separately decides whether child-domain uncertainty still
    /// requires quarantine.
    ///
    /// # Errors
    ///
    /// Returns [`AppError::Filesystem`] when a lock file cannot be created.
    pub fn try_acquire_all(
        resources: &[LockResource],
        owner: &LockOwner,
    ) -> Result<Result<Self, LockContention>, AppError> {
        Self::default().try_acquire_missing(resources, owner)
    }

    /// Takes exactly the keys in `resources` that this set does not already hold.
    ///
    /// The returned set contains only newly acquired keys. Resource paths remain attached to each
    /// key so a contention still names the journal resource an operator recognises. This is the
    /// recovery claim operation: passing whole partly-overlapping resources to
    /// [`HeldLocks::try_acquire_all`] would attempt to lock an already-held key again and falsely
    /// classify the stale transaction as active.
    ///
    /// # Errors
    ///
    /// Returns [`AppError::Filesystem`] when a lock file cannot be created.
    pub fn try_acquire_missing(
        &self,
        resources: &[LockResource],
        owner: &LockOwner,
    ) -> Result<Result<Self, LockContention>, AppError> {
        let mut held = Self::default();
        for (key, paths) in self.missing_keys(resources) {
            match take(&key, LockPolicy::immediate(), owner)? {
                Taken::Held(lock) => {
                    held.locks.insert(key, lock);
                }
                Taken::Busy { path, holder } => {
                    return Ok(Err(LockContention {
                        key,
                        path,
                        resources: paths,
                        holder,
                    }));
                }
            }
        }
        Ok(Ok(held))
    }

    /// Absorbs another set, so recovery locks stay held for the rest of the session.
    pub fn absorb(&mut self, other: Self) {
        self.locks.extend(other.locks);
    }

    /// Returns whether `key` is currently held by this session.
    #[must_use]
    pub fn holds(&self, key: &LockKey) -> bool {
        self.locks.contains_key(key)
    }

    /// Returns whether every key the resources need is currently held.
    #[must_use]
    pub fn holds_all(&self, resources: &[LockResource]) -> bool {
        resources
            .iter()
            .flat_map(LockResource::lock_keys)
            .all(|key| self.holds(&key))
    }

    /// Returns the held keys in acquisition order.
    pub fn keys(&self) -> impl Iterator<Item = &LockKey> {
        self.locks.keys()
    }

    fn missing_keys(&self, resources: &[LockResource]) -> Vec<(LockKey, Vec<PathBuf>)> {
        sorted_keys(resources)
            .into_iter()
            .filter(|(key, _)| !self.locks.contains_key(key))
            .collect()
    }

    fn requires_reacquire_for_keys(&self, missing: &[(LockKey, Vec<PathBuf>)]) -> bool {
        let Some(highest_held) = self.locks.last_key_value().map(|(key, _)| key) else {
            return false;
        };
        missing
            .first()
            .is_some_and(|(lowest_missing, _)| lowest_missing < highest_held)
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

/// Collapses resources onto their keys, deduplicated and sorted.
///
/// Deduplication is what makes a Codex layout whose discovery entry links to its own backing store
/// take one lock rather than deadlocking against itself, and sorting is what keeps two sessions
/// that discovered the resources in different orders from deadlocking against each other. The
/// resource paths are retained per key so a contention message names something the operator
/// recognises rather than a digest.
fn sorted_keys(resources: &[LockResource]) -> Vec<(LockKey, Vec<PathBuf>)> {
    let mut grouped: BTreeMap<LockKey, Vec<PathBuf>> = BTreeMap::new();
    for resource in resources {
        for key in resource.lock_keys() {
            let paths = grouped.entry(key).or_default();
            if !paths.contains(&resource.path) {
                paths.push(resource.path.clone());
            }
        }
    }
    grouped.into_iter().collect()
}

/// Exposes the acquisition order so a test can assert it without taking real locks.
#[cfg(test)]
pub(crate) fn sorted_keys_for_test(resources: &[LockResource]) -> Vec<(LockKey, Vec<PathBuf>)> {
    sorted_keys(resources)
}

fn take(key: &LockKey, policy: LockPolicy, owner: &LockOwner) -> Result<Taken, AppError> {
    let directory = state::lock_base()?;
    state::ensure_private_directory(&directory)?;
    let path = directory.join(key.file_name());

    // The lock file stays empty; the holder description lives beside it. Windows byte-range locks
    // are mandatory, so a locked file cannot be read through any other handle — including by the
    // process that holds it. Writing the diagnostics into the locked file would therefore make them
    // unreadable exactly when they are wanted, which is while somebody else holds the lock.
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
        // Fully qualified so this always reaches `fs4`. `std::fs::File` grew an inherent
        // `try_lock` in Rust 1.89, and an inherent method wins over a trait one — which would
        // silently raise the crate's minimum supported version above the pinned 1.85.0.
        match FileExt::try_lock(&file) {
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
                    "cannot lock {}: {error}",
                    path.display()
                )));
            }
        }
    }

    state::restrict_to_owner(&path)?;
    write_holder(key, owner);
    Ok(Taken::Held(HeldLock {
        file,
        description: holder_path(key)?,
    }))
}

/// Returns the sidecar that carries the holder description for `key`.
///
/// # Errors
///
/// Returns [`AppError::MissingInput`] when the platform state location cannot be resolved.
fn holder_path(key: &LockKey) -> Result<PathBuf, AppError> {
    Ok(state::lock_base()?.join(format!("{key}.owner")))
}

/// Records who holds the lock, for diagnostics only.
///
/// Written to a sidecar rather than to the lock file, because a Windows byte-range lock is
/// mandatory and would make the description unreadable while the lock is held. Best effort in every
/// other respect too: a lock that is held but whose description could not be written is still held,
/// and failing the session over an unwritten diagnostic would trade a correct outcome for a
/// cosmetic one.
fn write_holder(key: &LockKey, owner: &LockOwner) {
    let Ok(path) = holder_path(key) else {
        return;
    };
    let description = format!(
        "transaction={} pid={}\nthis file records who last took the lock; neither it nor the lock file is evidence that anyone still holds it\n",
        owner.transaction, owner.pid
    );
    if std::fs::write(&path, description).is_ok() {
        let _ = state::restrict_to_owner(&path);
    }
}

/// Removes the holder description once the lock is released.
///
/// Leaving it behind would be harmless — nothing reads it as liveness — but removing it keeps a
/// contention message from naming a session that finished long ago.
fn clear_holder(path: &Path) {
    let _ = std::fs::remove_file(path);
}

/// Reads the holder description, which may be absent, stale, or unreadable.
///
/// Every one of those is expected. The description is written after the lock is taken and removed
/// when it is released, so a reader can always catch it mid-flight, and a crashed holder leaves one
/// behind. It is never treated as evidence that the lock is held.
fn read_holder(key: &LockKey) -> Option<String> {
    let contents = std::fs::read_to_string(holder_path(key).ok()?).ok()?;
    let first = contents.lines().next()?.trim();
    (!first.is_empty()).then(|| first.to_owned())
}
