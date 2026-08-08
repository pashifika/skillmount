//! Reconciling transactions that no process is still driving.
//!
//! The hard question is not what to remove; it is how to know nobody else is using it. Before child
//! exposure the answer is the lock set. A journal's owner process id is recorded but never
//! consulted: the operating system reuses process ids, so "that pid is gone" and "that pid belongs
//! to something unrelated now" look identical. After child exposure no durable automatic proof is
//! available, so recovery retains rather than guesses.
//!
//! Before child exposure, eligibility is decided by trying to take every lock the journal says its
//! transaction held. If all are free, no wrapper is between apply and cleanup and the transaction
//! can be reconciled. After `supervising` is durable, free wrapper locks no longer prove the child
//! domain empty; that state is quarantined. If any lock is held, the transaction is actively driven
//! and is left completely alone.
//!
//! Recovery then reuses the ordinary rollback path, so there is exactly one implementation of
//! "prove ownership, then remove", and it is the one that has to be right.

use std::path::{Path, PathBuf};

use crate::domain::AgentId;
use crate::error::AppError;
use crate::journal::TransactionStatus;
use crate::journal::store::{self, JournalScan, RejectedJournal, ScannedJournal};
use crate::link::resolve::ComparablePath;
use crate::lock::LockResource;
use crate::lock::acquire::{HeldLocks, LockContention, LockOwner, MissingLockOutcome};

use super::Transaction;
use super::cleanup::CleanupReport;

/// What one recovery pass did.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct RecoveryReport {
    /// Transactions that were reconciled, with what each pass removed and retained.
    pub reconciled: Vec<ReconciledTransaction>,
    /// Transactions whose locks are still held, so a process is still driving them.
    pub active: Vec<PathBuf>,
    /// Journals whose locks are free but whose child-domain liveness was never proved.
    pub quarantined: Vec<QuarantinedTransaction>,
    /// Journals that exist but cannot be interpreted, and are therefore retained untouched.
    pub unreadable: Vec<RejectedJournal>,
    /// Journal resources that overlap held weaker access and require a full acquisition restart.
    pub(crate) reacquire: Vec<LockResource>,
    /// Journals that must still exist and be reloaded after the requested acquisition restart.
    pub(crate) reinspect: Vec<PathBuf>,
}

impl RecoveryReport {
    /// Returns whether anything needs an operator's attention.
    #[must_use]
    pub fn needs_attention(&self) -> bool {
        !self.unreadable.is_empty()
            || !self.quarantined.is_empty()
            || self
                .reconciled
                .iter()
                .any(|entry| entry.report.needs_attention())
    }

    /// Renders one line per outcome, for the diagnostics stream.
    ///
    /// Preserved scaffolding is included here even though a session's own cleanup stays silent about
    /// it. Recovery adopted somebody else's journal and is about to retire it, so this is the last
    /// moment anything records that a directory was deliberately left behind.
    #[must_use]
    pub fn describe(&self) -> Vec<String> {
        let mut lines = Vec::new();
        for entry in &self.reconciled {
            lines.push(format!(
                "recovered transaction {} from {}: {} entr{} removed",
                crate::render::text_value(&entry.transaction),
                crate::render::path_value(&entry.journal, true),
                entry.report.removed.len(),
                if entry.report.removed.len() == 1 {
                    "y"
                } else {
                    "ies"
                }
            ));
            lines.extend(entry.report.describe());
            lines.extend(entry.report.describe_preserved());
        }
        for path in &self.active {
            lines.push(format!(
                "transaction journal {} belongs to a session that still holds its locks and was left alone",
                crate::render::path_value(path, true)
            ));
        }
        for entry in &self.quarantined {
            lines.push(format!(
                "transaction journal {} may still belong to a live child process domain and was quarantined without cleanup",
                crate::render::path_value(&entry.journal, true)
            ));
        }
        for rejected in &self.unreadable {
            lines.push(format!(
                "transaction journal {} cannot be interpreted ({}), so it is retained and nothing beneath it was removed",
                crate::render::path_value(&rejected.path, true),
                crate::render::text_value(&rejected.reason)
            ));
        }
        lines
    }
}

/// A journal whose wrapper locks are free but whose child process domain may still be alive.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct QuarantinedTransaction {
    /// Journal retaining the transaction's ownership evidence.
    pub journal: PathBuf,
    /// Canonical project recorded by that transaction.
    pub project_root: PathBuf,
}

/// One transaction that was reconciled.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ReconciledTransaction {
    /// Identity of the recovered transaction.
    pub transaction: String,
    /// Agent that owned it, as recorded in its journal.
    pub agent: AgentId,
    /// Journal that described it.
    pub journal: PathBuf,
    /// Canonical project recorded by that transaction.
    pub project_root: PathBuf,
    /// What the removal pass did.
    pub report: CleanupReport,
}

/// Results of an explicit, operator-authorized cleanup pass.
#[derive(Debug, Default)]
pub(crate) struct ExplicitCleanupReport {
    pub(crate) reconciled: Vec<ReconciledTransaction>,
    pub(crate) active: Vec<ActiveTransaction>,
    pub(crate) unreadable: Vec<RejectedJournal>,
    pub(crate) failures: Vec<ExplicitCleanupFailure>,
    pub(crate) completed: Vec<PathBuf>,
    pub(crate) out_of_scope: usize,
}

/// A selected transaction whose operating-system locks are still held.
#[derive(Debug)]
pub(crate) struct ActiveTransaction {
    pub(crate) transaction: String,
    pub(crate) agent: AgentId,
    pub(crate) journal: PathBuf,
    pub(crate) contention: LockContention,
}

/// A selected transaction that failed before or while running shared cleanup.
#[derive(Debug)]
pub(crate) struct ExplicitCleanupFailure {
    pub(crate) transaction: String,
    pub(crate) agent: AgentId,
    pub(crate) journal: PathBuf,
    pub(crate) error: AppError,
}

/// Reconciles selected journals after the operator explicitly asserts that no related child
/// process domain should still be using their mounts.
///
/// `project_root` selects one canonical project. `None` is the explicit `--all` scope. Unknown
/// journal state remains a global fail-closed condition: a rejected journal can name resources in
/// either scope, so no valid neighbor is touched until every journal is readable.
pub(crate) fn cleanup_explicit(
    project_root: Option<&std::path::Path>,
) -> Result<ExplicitCleanupReport, AppError> {
    let scan = store::scan()?;
    crate::checkpoint::reached(crate::checkpoint::Checkpoint::JournalScanComplete, 1);
    let mut report = ExplicitCleanupReport {
        unreadable: scan.rejected,
        ..ExplicitCleanupReport::default()
    };
    if !report.unreadable.is_empty() {
        return Ok(report);
    }

    let scope = project_root.map(ComparablePath::new);
    let mut claimed = Vec::new();
    let mut claimed_locks = HeldLocks::default();
    let mut claimed_resources = Vec::new();
    for scanned in scan.journals {
        let in_scope = scope.as_ref().is_none_or(|scope| {
            scope.names_same_path(&ComparablePath::new(&scanned.journal.project_root))
        });
        if !in_scope {
            report.out_of_scope += 1;
            continue;
        }
        if scanned.journal.status == crate::journal::TransactionStatus::Completed {
            report.completed.push(scanned.path);
            continue;
        }

        let transaction = scanned.journal.transaction_id.to_string();
        let agent = scanned.journal.agent;
        let resources = scanned.journal.lock_resources();
        let owner = LockOwner::for_transaction(&scanned.journal.transaction_id);
        let locks = match claimed_locks.try_acquire_missing(&resources, &owner) {
            Err(error) => {
                report.failures.push(ExplicitCleanupFailure {
                    transaction,
                    agent,
                    journal: scanned.path,
                    error,
                });
                continue;
            }
            Ok(MissingLockOutcome::Acquired(locks)) => locks,
            Ok(MissingLockOutcome::Contended(contention)) => {
                report.active.push(ActiveTransaction {
                    transaction,
                    agent,
                    journal: scanned.path,
                    contention,
                });
                continue;
            }
            Ok(MissingLockOutcome::RequiresReacquire) => {
                if !reacquire_explicit_locks(
                    &resources,
                    &scanned,
                    &claimed_resources,
                    &mut claimed_locks,
                    &mut claimed,
                    &mut report,
                ) {
                    return Ok(report);
                }
                HeldLocks::default()
            }
        };
        claimed_locks.absorb(locks);
        for resource in &resources {
            if !claimed_resources.contains(resource) {
                claimed_resources.push(resource.clone());
            }
        }
        let fresh = match reload_locked(&scanned, &claimed_locks) {
            Ok(fresh) => fresh,
            Err(rejected) => {
                report.unreadable.push(rejected);
                continue;
            }
        };
        if fresh.journal.status == TransactionStatus::Completed {
            report.completed.push(fresh.path);
            continue;
        }
        claimed.push(fresh);
    }

    // A journal that became unreadable or changed its immutable identity after the initial scan
    // has unknown ownership scope. Keep every successfully claimed neighbor untouched, just as the
    // initial corrupt-journal preflight does.
    if !report.unreadable.is_empty() {
        return Ok(report);
    }

    reconcile_explicit_claims(claimed, &claimed_locks, &mut report);
    Ok(report)
}

fn reacquire_explicit_locks(
    resources: &[LockResource],
    scanned: &ScannedJournal,
    claimed_resources: &[LockResource],
    claimed_locks: &mut HeldLocks,
    claimed: &mut Vec<ScannedJournal>,
    report: &mut ExplicitCleanupReport,
) -> bool {
    let mut strongest = claimed_resources.to_vec();
    for resource in resources {
        if !strongest.contains(resource) {
            strongest.push(resource.clone());
        }
    }

    drop(std::mem::take(claimed_locks));
    *claimed_locks = match HeldLocks::try_acquire_all(&strongest, &LockOwner::preliminary()) {
        Ok(Ok(locks)) => locks,
        Ok(Err(contention)) => {
            report.active.push(ActiveTransaction {
                transaction: scanned.journal.transaction_id.to_string(),
                agent: scanned.journal.agent,
                journal: scanned.path.clone(),
                contention,
            });
            return false;
        }
        Err(error) => {
            report.failures.push(ExplicitCleanupFailure {
                transaction: scanned.journal.transaction_id.to_string(),
                agent: scanned.journal.agent,
                journal: scanned.path.clone(),
                error,
            });
            return false;
        }
    };

    // The unlocked promotion gap invalidates every earlier journal snapshot. Refresh all of them
    // under the reacquired strongest union before authorizing removal.
    let mut refreshed = Vec::with_capacity(claimed.len());
    for previous in claimed.iter() {
        match reload_locked(previous, claimed_locks) {
            Ok(fresh) if fresh.journal.status == TransactionStatus::Completed => {
                report.completed.push(fresh.path);
            }
            Ok(fresh) => refreshed.push(fresh),
            Err(rejected) => report.unreadable.push(rejected),
        }
    }
    if !report.unreadable.is_empty() {
        return false;
    }
    *claimed = refreshed;
    true
}

/// Reconciles every claimed journal under the shared lock set, in deterministic scan order.
///
/// Overlapping journals used to need an ownership-derived order: cleaning a directory owner before
/// the transaction owning a link inside it stranded the directory owner's journal on a non-empty
/// directory. A helper directory is now best-effort scaffolding whose retention discharges its own
/// action, so no order can strand a journal and scan order carries no policy.
fn reconcile_explicit_claims(
    claimed: Vec<ScannedJournal>,
    locks: &HeldLocks,
    report: &mut ExplicitCleanupReport,
) {
    for scanned in claimed {
        let transaction = scanned.journal.transaction_id.to_string();
        let agent = scanned.journal.agent;
        let project_root = scanned.journal.project_root.clone();
        let mut adopted = match Transaction::adopt(scanned.journal, scanned.path.clone(), locks) {
            Ok(transaction) => transaction,
            Err(error) => {
                report.failures.push(ExplicitCleanupFailure {
                    transaction,
                    agent,
                    journal: scanned.path,
                    error,
                });
                continue;
            }
        };
        match adopted.cleanup_explicit() {
            Ok(outcome) => report.reconciled.push(ReconciledTransaction {
                transaction,
                agent,
                journal: scanned.path,
                project_root,
                report: outcome,
            }),
            Err(error) => report.failures.push(ExplicitCleanupFailure {
                transaction,
                agent,
                journal: scanned.path,
                error,
            }),
        }
    }
}

/// Reconciles automatically recoverable transactions whose locks are free and quarantines
/// post-launch uncertainty.
///
/// `already_held` is the current session's lock set. Locks taken during recovery are absorbed into
/// it rather than released, so a transaction that was just reconciled cannot be resurrected by a
/// third session between recovery and this session's own apply.
///
/// # Errors
///
/// Returns [`AppError::Journal`] when the journal directory cannot be enumerated, and
/// [`AppError::Filesystem`] when a lock file cannot be opened. A journal that cannot be interpreted
/// stops this pass before any healthy journal is reconciled. Unknown ownership state is a global
/// fail-closed condition for mutation; removing entries from a healthy transaction beside it would
/// violate the future-schema guarantee that no destination is touched.
pub fn recover_stale(already_held: &mut HeldLocks) -> Result<RecoveryReport, AppError> {
    recover_stale_after_reacquire(already_held, &[])
}

/// Repeats recovery after an access-promotion restart without forgetting earlier candidates.
pub(crate) fn recover_stale_after_reacquire(
    already_held: &mut HeldLocks,
    expected: &[PathBuf],
) -> Result<RecoveryReport, AppError> {
    let scan: JournalScan = store::scan()?;
    crate::checkpoint::reached(crate::checkpoint::Checkpoint::JournalScanComplete, 1);
    let mut report = RecoveryReport {
        unreadable: scan.rejected.clone(),
        ..RecoveryReport::default()
    };
    for path in expected {
        let still_present = scan.journals.iter().any(|scanned| scanned.path == *path)
            || scan.rejected.iter().any(|rejected| rejected.path == *path);
        if !still_present {
            report.unreadable.push(RejectedJournal {
                path: path.clone(),
                reason: "the journal disappeared during the full access-promotion restart"
                    .to_owned(),
            });
        }
    }
    if !report.unreadable.is_empty() {
        return Ok(report);
    }

    // Claim and refresh every free incomplete journal before removing anything. The scan is only
    // a candidate list: another session can advance a journal and release its locks while this
    // process waits. Keeping every successful claim and classifying the fresh, under-lock state
    // prevents a stale `planned` snapshot from erasing evidence for an applied mount, and prevents
    // a freshly `supervising` transaction from being cleaned automatically.
    let mut recoverable = Vec::new();
    for scanned in scan.incomplete() {
        let resources = scanned.journal.lock_resources();
        let taken = match claim(&resources, already_held)? {
            ClaimOutcome::Acquired(taken) => taken,
            ClaimOutcome::Contended => {
                report.active.push(scanned.path.clone());
                continue;
            }
            ClaimOutcome::RequiresReacquire => {
                for resource in resources {
                    if !report.reacquire.contains(&resource) {
                        report.reacquire.push(resource);
                    }
                }
                if !report.reinspect.contains(&scanned.path) {
                    report.reinspect.push(scanned.path.clone());
                }
                continue;
            }
        };
        already_held.absorb(taken);
        let fresh = match reload_locked(scanned, already_held) {
            Ok(fresh) => fresh,
            Err(rejected) => {
                report.unreadable.push(rejected);
                continue;
            }
        };
        if fresh.journal.status.is_terminal() {
            continue;
        }
        if fresh.journal.status.is_automatically_recoverable() {
            recoverable.push(fresh);
        } else {
            report.quarantined.push(QuarantinedTransaction {
                journal: fresh.path,
                project_root: fresh.journal.project_root,
            });
        }
    }
    if !report.unreadable.is_empty()
        || !report.quarantined.is_empty()
        || !report.reacquire.is_empty()
    {
        return Ok(report);
    }

    for scanned in recoverable {
        // The file the journal was read from, never a path re-derived from its recorded id. The
        // two agree for every journal this crate wrote; reconciling the derived one instead would
        // leave a mismatched file behind to be recovered again on every later run.
        let transaction_id = scanned.journal.transaction_id.to_string();
        let agent = scanned.journal.agent;
        let project_root = scanned.journal.project_root.clone();
        let journal_path = scanned.path.clone();
        let mut transaction =
            Transaction::adopt(scanned.journal, journal_path.clone(), already_held)?;
        let outcome = transaction.cleanup_recovered()?;
        report.reconciled.push(ReconciledTransaction {
            transaction: transaction_id,
            agent,
            journal: journal_path,
            project_root,
            report: outcome,
        });
    }

    Ok(report)
}

/// Reloads a scanned journal while all of its recorded resources are locked.
///
/// Status and action evidence may legitimately advance between scan and lock acquisition. The
/// transaction id, project scope, and complete lock set may not: changing any of those would mean
/// the acquired locks do not authorize the fresh record. Such drift is treated as unreadable state
/// and blocks mutation.
fn reload_locked(
    scanned: &ScannedJournal,
    locks: &HeldLocks,
) -> Result<ScannedJournal, RejectedJournal> {
    let journal =
        store::load_if_exists(&scanned.path).map_err(|error| rejected(&scanned.path, error))?;
    let Some(journal) = journal else {
        return Err(RejectedJournal {
            path: scanned.path.clone(),
            reason: "the journal disappeared while its recorded locks were being acquired; its ownership state cannot be proved"
                .to_owned(),
        });
    };
    if journal.transaction_id != scanned.journal.transaction_id {
        return Err(RejectedJournal {
            path: scanned.path.clone(),
            reason: "the transaction id changed while its recorded locks were being acquired"
                .to_owned(),
        });
    }
    if journal.project_root != scanned.journal.project_root {
        return Err(RejectedJournal {
            path: scanned.path.clone(),
            reason: "the recorded project root changed while its locks were being acquired"
                .to_owned(),
        });
    }
    let fresh_resources = journal.lock_resources();
    if fresh_resources != scanned.journal.lock_resources() {
        return Err(RejectedJournal {
            path: scanned.path.clone(),
            reason: "the complete lock set changed while its earlier set was being acquired"
                .to_owned(),
        });
    }
    if !locks.holds_all(&fresh_resources) {
        return Err(RejectedJournal {
            path: scanned.path.clone(),
            reason: "the fresh journal names a resource lock this recovery does not hold"
                .to_owned(),
        });
    }
    Ok(ScannedJournal {
        path: scanned.path.clone(),
        journal,
    })
}

fn rejected(path: &Path, error: AppError) -> RejectedJournal {
    let reason = match error {
        AppError::Journal(error) => error.reason(),
        other => other.to_string(),
    };
    RejectedJournal {
        path: path.to_path_buf(),
        reason,
    }
}

/// Reports whether anything would block a `--no-recover` run, without touching a single entry.
///
/// Fail-closed by design: an incomplete journal whose locks are free describes entries this build
/// cannot account for, and continuing would plan against a store whose real contents are unknown.
/// An unreadable journal counts too, because "cannot be interpreted" is strictly less information
/// than "incomplete".
///
/// # Errors
///
/// Returns [`AppError::Journal`] when the journal directory cannot be enumerated.
pub fn blocking_state(already_held: &HeldLocks) -> Result<Vec<String>, AppError> {
    let scan = store::scan()?;
    let mut blocking = scan
        .rejected
        .iter()
        .map(|rejected| {
            format!(
                "transaction journal {} cannot be interpreted: {}",
                crate::render::path_value(&rejected.path, true),
                crate::render::text_value(&rejected.reason)
            )
        })
        .collect::<Vec<_>>();

    for scanned in scan.incomplete() {
        let resources = scanned.journal.lock_resources();
        // A transaction contended by another process is live. A transaction blocked only by this
        // run's weaker observation would become recoverable after a full acquisition restart, so
        // `--no-recover` must still report it as forbidden incomplete state.
        if !matches!(claim(&resources, already_held)?, ClaimOutcome::Contended) {
            blocking.push(format!(
                "transaction {} is incomplete ({}) and --no-recover forbids handling it",
                crate::render::text_value(scanned.journal.transaction_id.as_str()),
                scanned.journal.status.label()
            ));
        }
    }
    Ok(blocking)
}

enum ClaimOutcome {
    Acquired(HeldLocks),
    Contended,
    RequiresReacquire,
}

/// Takes every lock a journal names, reports contention, or asks the application to restart with
/// the complete strongest union.
///
/// A lock this session already holds counts only when its access is strong enough. A held
/// observation overlapping recorded mutation must not be upgraded in place because another reader
/// can have joined it; releasing the complete set is the only deadlock-safe promotion.
fn claim(resources: &[LockResource], already_held: &HeldLocks) -> Result<ClaimOutcome, AppError> {
    if already_held.holds_all(resources) {
        return Ok(ClaimOutcome::Acquired(HeldLocks::default()));
    }
    match already_held.try_acquire_missing(resources, &LockOwner::preliminary())? {
        MissingLockOutcome::Acquired(locks) => Ok(ClaimOutcome::Acquired(locks)),
        MissingLockOutcome::Contended(_) => Ok(ClaimOutcome::Contended),
        MissingLockOutcome::RequiresReacquire => Ok(ClaimOutcome::RequiresReacquire),
    }
}

impl Transaction {
    /// Reconciles an adopted journal, reusing the ordinary removal path.
    ///
    /// A terminal journal never reaches this function: [`recover_stale`] filters on
    /// [`crate::journal::TransactionStatus::is_incomplete`], so a `kept` transaction keeps its
    /// mounts and a `completed` one has nothing left to reconcile.
    fn cleanup_recovered(&mut self) -> Result<CleanupReport, AppError> {
        // `keep_mounts` becomes terminal only through the orderly cleanup entry in `cleanup`.
        // Reaching recovery proves the earlier process did not durably finish that boundary, so a
        // planned, applying, active, cleaning, or failed transaction must be reconciled exactly
        // like any other automatically recoverable transaction. `supervising` never reaches this
        // function. Persisting `cleaning` below also clears the flag, so a second crash cannot later
        // reinterpret the same partial apply as requested retention.
        self.journal.keep_mounts = false;
        self.cleanup()
    }
}

#[cfg(test)]
mod tests {
    use super::{ClaimOutcome, claim, recover_stale, reload_locked};
    use crate::domain::AgentId;
    use crate::journal::store::{self, ScannedJournal};
    use crate::journal::{JournalLock, TransactionId, TransactionJournal, TransactionStatus};
    use crate::lock::acquire::{HeldLocks, LockOwner, LockPolicy};
    use crate::lock::{LockAccess, LockResource, LockResourceKind};
    use crate::state::testing::StateRootGuard;
    use crate::test_support::TestDir;

    fn minimal_journal(
        root: &std::path::Path,
        id: &str,
        resources: &[LockResource],
    ) -> TransactionJournal {
        let discovery = resources
            .iter()
            .find(|resource| {
                resource.kind == LockResourceKind::DiscoveryEntry
                    && resource.access.satisfies(LockAccess::Mutate)
            })
            .expect("a recovery fixture records discovery mutation authority");
        let backing_store = resources
            .iter()
            .find(|resource| {
                resource.kind == LockResourceKind::BackingStore
                    && resource.access.satisfies(LockAccess::Mutate)
            })
            .expect("a recovery fixture records backing-store mutation authority");
        TransactionJournal {
            transaction_id: TransactionId::parse(id).unwrap(),
            agent: AgentId::Codex,
            owner_pid: 4242,
            status: TransactionStatus::Planned,
            project_root: root.to_path_buf(),
            launch_cwd: root.to_path_buf(),
            discovery_entry: discovery.path.clone(),
            backing_store: backing_store.path.clone(),
            keep_mounts: false,
            sources: Vec::new(),
            locks: resources.iter().map(JournalLock::from).collect(),
            actions: Vec::new(),
            errors: Vec::new(),
        }
    }

    fn persist_minimal(
        root: &std::path::Path,
        id: &str,
        resources: &[LockResource],
    ) -> std::path::PathBuf {
        let journal = minimal_journal(root, id, resources);
        let path = store::journal_path(&journal.transaction_id).unwrap();
        store::persist(&journal, &path).unwrap();
        path
    }

    fn complete_mutation_pair(resource: &LockResource) -> [LockResource; 2] {
        let mut discovery = resource.clone();
        discovery.kind = LockResourceKind::DiscoveryEntry;
        discovery.access = LockAccess::Mutate;
        let mut backing_store = resource.clone();
        backing_store.kind = LockResourceKind::BackingStore;
        backing_store.access = LockAccess::Mutate;
        [discovery, backing_store]
    }

    #[test]
    fn a_partial_overlap_claims_only_in_global_order() {
        let fixture = TestDir::new("recovery-partial-lock");
        let _guard = StateRootGuard::set(fixture.path());
        let root = std::fs::canonicalize(fixture.path()).unwrap();
        let store = root.join("store");
        std::fs::create_dir_all(&store).expect("store fixture");
        let full = LockResource::describe(
            LockResourceKind::BackingStore,
            LockAccess::Mutate,
            &root,
            &store,
        )
        .unwrap();
        assert_eq!(full.lock_keys().len(), 2);

        let mut logical_only = full.clone();
        logical_only.identity.physical = None;
        let mut current = HeldLocks::acquire(
            &[logical_only],
            LockPolicy::immediate(),
            &LockOwner::preliminary(),
        )
        .expect("the current session holds the logical key");
        let requires_reacquire = current.requires_reacquire(std::slice::from_ref(&full));

        match claim(std::slice::from_ref(&full), &current)
            .expect("the missing lock file is available")
        {
            ClaimOutcome::Acquired(newly_claimed) => {
                assert!(!requires_reacquire);
                assert_eq!(newly_claimed.keys().count(), 1);
                current.absorb(newly_claimed);
                assert!(current.holds_all(&[full]));
            }
            ClaimOutcome::RequiresReacquire => assert!(requires_reacquire),
            ClaimOutcome::Contended => panic!("the fixture has no competing process"),
        }
    }

    #[test]
    fn an_observation_never_claims_recorded_mutation_access() {
        let fixture = TestDir::new("recovery-access-promotion");
        let _guard = StateRootGuard::set(fixture.path());
        let root = std::fs::canonicalize(fixture.path()).unwrap();
        let observed = LockResource::describe(
            LockResourceKind::DiscoveryEntry,
            LockAccess::Observe,
            &root,
            &root.join("shared"),
        )
        .unwrap();
        let mut mutated = observed.clone();
        mutated.access = LockAccess::Mutate;
        let current = HeldLocks::acquire(
            std::slice::from_ref(&observed),
            LockPolicy::immediate(),
            &LockOwner::preliminary(),
        )
        .expect("observation lock");

        assert!(matches!(
            claim(std::slice::from_ref(&mutated), &current).unwrap(),
            ClaimOutcome::RequiresReacquire
        ));
        assert!(!current.holds_all(&[mutated]));
    }

    #[test]
    fn access_drift_during_claim_fails_closed() {
        let fixture = TestDir::new("recovery-access-drift");
        let _guard = StateRootGuard::set(fixture.path());
        let root = std::fs::canonicalize(fixture.path()).unwrap();
        let observed = LockResource::describe(
            LockResourceKind::DiscoveryEntry,
            LockAccess::Observe,
            &root,
            &root.join("shared"),
        )
        .unwrap();
        let backing_store = LockResource::describe(
            LockResourceKind::BackingStore,
            LockAccess::Mutate,
            &root,
            &root.join("store"),
        )
        .unwrap();
        let mut journal = TransactionJournal {
            transaction_id: TransactionId::parse("a11ce").unwrap(),
            agent: AgentId::Codex,
            owner_pid: 4242,
            status: TransactionStatus::Planned,
            project_root: root.clone(),
            launch_cwd: root.clone(),
            discovery_entry: root.join("shared"),
            backing_store: backing_store.path.clone(),
            keep_mounts: false,
            sources: Vec::new(),
            locks: vec![
                JournalLock::from(&observed),
                JournalLock::from(&backing_store),
            ],
            actions: Vec::new(),
            errors: Vec::new(),
        };
        let path = store::journal_path(&journal.transaction_id).unwrap();
        store::persist(&journal, &path).unwrap();
        let scanned = ScannedJournal {
            path: path.clone(),
            journal: journal.clone(),
        };
        let locks = HeldLocks::acquire(
            &[observed.clone(), backing_store],
            LockPolicy::immediate(),
            &LockOwner::preliminary(),
        )
        .unwrap();

        journal.locks[0].access = LockAccess::Mutate;
        store::persist(&journal, &path).unwrap();
        let rejected = reload_locked(&scanned, &locks)
            .expect_err("access is part of the immutable claimed lock set");

        assert!(rejected.reason.contains("complete lock set changed"));
        assert!(path.exists(), "drifted ownership evidence is retained");
    }

    #[test]
    fn live_journal_shares_observations_with_an_independent_session() {
        let fixture = TestDir::new("recovery-live-shared-observation");
        let _guard = StateRootGuard::set(fixture.path());
        let root = std::fs::canonicalize(fixture.path()).unwrap();
        let shared = LockResource::describe(
            LockResourceKind::DiscoveryEntry,
            LockAccess::Observe,
            &root,
            &root.join("shared"),
        )
        .unwrap();
        let live_mutation = LockResource::describe(
            LockResourceKind::BackingStore,
            LockAccess::Mutate,
            &root,
            &root.join("live"),
        )
        .unwrap();
        let live_authority = complete_mutation_pair(&live_mutation);
        let independent_mutation = LockResource::describe(
            LockResourceKind::BackingStore,
            LockAccess::Mutate,
            &root,
            &root.join("independent"),
        )
        .unwrap();
        let path = persist_minimal(
            &root,
            "11",
            &[
                shared.clone(),
                live_authority[0].clone(),
                live_authority[1].clone(),
            ],
        );
        let _live = HeldLocks::acquire(
            &[
                shared.clone(),
                live_authority[0].clone(),
                live_authority[1].clone(),
            ],
            LockPolicy::immediate(),
            &LockOwner {
                transaction: "live".to_owned(),
                pid: 1001,
            },
        )
        .expect("live transaction locks");
        let mut independent = HeldLocks::acquire(
            &[shared.clone(), independent_mutation],
            LockPolicy::immediate(),
            &LockOwner {
                transaction: "independent".to_owned(),
                pid: 1002,
            },
        )
        .expect("independent mutation may share observations");

        let report = recover_stale(&mut independent).expect("live journal is classified");

        assert_eq!(report.active, vec![path.clone()]);
        assert!(report.reacquire.is_empty(), "{report:?}");
        assert!(path.exists(), "a live journal is untouched");
        assert!(independent.holds_all(&[shared]));
    }

    #[test]
    fn stale_journal_is_recovered_beside_compatible_readers() {
        let fixture = TestDir::new("recovery-stale-shared-observation");
        let _guard = StateRootGuard::set(fixture.path());
        let root = std::fs::canonicalize(fixture.path()).unwrap();
        let shared = LockResource::describe(
            LockResourceKind::DiscoveryEntry,
            LockAccess::Observe,
            &root,
            &root.join("shared"),
        )
        .unwrap();
        let stale_mutation = LockResource::describe(
            LockResourceKind::BackingStore,
            LockAccess::Mutate,
            &root,
            &root.join("stale"),
        )
        .unwrap();
        let stale_authority = complete_mutation_pair(&stale_mutation);
        let path = persist_minimal(
            &root,
            "22",
            &[
                shared.clone(),
                stale_authority[0].clone(),
                stale_authority[1].clone(),
            ],
        );
        let reader = HeldLocks::acquire(
            std::slice::from_ref(&shared),
            LockPolicy::immediate(),
            &LockOwner {
                transaction: "reader".to_owned(),
                pid: 1003,
            },
        )
        .expect("compatible reader");
        let mut recovery_locks = HeldLocks::default();

        let report = recover_stale(&mut recovery_locks).expect("stale journal recovers");

        assert_eq!(report.reconciled.len(), 1, "{report:?}");
        assert!(report.active.is_empty(), "{report:?}");
        assert!(!path.exists());
        assert!(reader.holds_all(&[shared]));
    }

    #[test]
    fn current_reader_restarts_with_mutation_before_stale_cleanup() {
        let fixture = TestDir::new("recovery-reader-promotion");
        let _guard = StateRootGuard::set(fixture.path());
        let root = std::fs::canonicalize(fixture.path()).unwrap();
        let observed = LockResource::describe(
            LockResourceKind::DiscoveryEntry,
            LockAccess::Observe,
            &root,
            &root.join("shared"),
        )
        .unwrap();
        let mut mutated = observed.clone();
        mutated.access = LockAccess::Mutate;
        let authority = complete_mutation_pair(&mutated);
        let path = persist_minimal(&root, "33", &authority);
        let current = HeldLocks::acquire(
            std::slice::from_ref(&observed),
            LockPolicy::immediate(),
            &LockOwner::preliminary(),
        )
        .expect("current reader");
        let mut current = current;

        let first = recover_stale(&mut current).expect("promotion is surfaced");
        assert_eq!(first.reacquire, authority.to_vec());
        assert!(first.reconciled.is_empty());
        assert!(path.exists());

        drop(current);
        let mut promoted = HeldLocks::acquire(
            &first.reacquire,
            LockPolicy::immediate(),
            &LockOwner::preliminary(),
        )
        .expect("fresh strongest union");
        let second = recover_stale(&mut promoted).expect("promoted cleanup");

        assert_eq!(second.reconciled.len(), 1, "{second:?}");
        assert!(!path.exists());
        assert!(promoted.holds_all(&[mutated]));
    }

    #[test]
    fn legacy_schema_one_journal_recovers_with_mutation_authority() {
        let fixture = TestDir::new("recovery-legacy-access");
        let _guard = StateRootGuard::set(fixture.path());
        let root = std::fs::canonicalize(fixture.path()).unwrap();
        let observed = LockResource::describe(
            LockResourceKind::BackingStore,
            LockAccess::Observe,
            &root,
            &root.join("legacy"),
        )
        .unwrap();
        let mut mutated = observed.clone();
        mutated.access = LockAccess::Mutate;
        let authority = complete_mutation_pair(&mutated);
        let journal = minimal_journal(&root, "44", &authority);
        let path = store::journal_path(&journal.transaction_id).unwrap();
        store::persist_legacy_for_test(&journal, &path).unwrap();
        let mut current = HeldLocks::acquire(
            std::slice::from_ref(&observed),
            LockPolicy::immediate(),
            &LockOwner::preliminary(),
        )
        .expect("current reader");

        let first = recover_stale(&mut current).expect("legacy promotion is surfaced");
        assert_eq!(first.reacquire[0].access, LockAccess::Mutate);
        assert!(path.exists());

        drop(current);
        let mut promoted = HeldLocks::acquire(
            &first.reacquire,
            LockPolicy::immediate(),
            &LockOwner::preliminary(),
        )
        .unwrap();
        let second = recover_stale(&mut promoted).expect("legacy cleanup");

        assert_eq!(second.reconciled.len(), 1, "{second:?}");
        assert!(!path.exists());
    }
}
