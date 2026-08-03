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

use std::path::PathBuf;

use crate::error::AppError;
use crate::journal::store::{self, JournalScan, RejectedJournal};
use crate::lock::LockResource;
use crate::lock::acquire::{HeldLocks, LockOwner};

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
    pub quarantined: Vec<PathBuf>,
    /// Journals that exist but cannot be interpreted, and are therefore retained untouched.
    pub unreadable: Vec<RejectedJournal>,
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
    #[must_use]
    pub fn describe(&self) -> Vec<String> {
        let mut lines = Vec::new();
        for entry in &self.reconciled {
            lines.push(format!(
                "recovered transaction {} from {}: {} entr{} removed",
                entry.transaction,
                entry.journal.display(),
                entry.report.removed.len(),
                if entry.report.removed.len() == 1 {
                    "y"
                } else {
                    "ies"
                }
            ));
            lines.extend(entry.report.describe());
        }
        for path in &self.active {
            lines.push(format!(
                "transaction journal {} belongs to a session that still holds its locks and was left alone",
                path.display()
            ));
        }
        for path in &self.quarantined {
            lines.push(format!(
                "transaction journal {} may still belong to a live child process domain and was quarantined without cleanup",
                path.display()
            ));
        }
        for rejected in &self.unreadable {
            lines.push(format!(
                "transaction journal {} cannot be interpreted ({}), so it is retained and nothing beneath it was removed",
                rejected.path.display(),
                rejected.reason
            ));
        }
        lines
    }
}

/// One transaction that was reconciled.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ReconciledTransaction {
    /// Identity of the recovered transaction.
    pub transaction: String,
    /// Journal that described it.
    pub journal: PathBuf,
    /// What the removal pass did.
    pub report: CleanupReport,
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
    let scan: JournalScan = store::scan()?;
    let mut report = RecoveryReport {
        unreadable: scan.rejected.clone(),
        ..RecoveryReport::default()
    };
    if !report.unreadable.is_empty() {
        return Ok(report);
    }

    // Quarantined liveness is a global fail-closed condition, like an unreadable journal. Detect
    // every free one before reconciling a healthy neighbor so no path is removed first merely
    // because its journal sorted earlier.
    for scanned in scan
        .incomplete()
        .filter(|scanned| !scanned.journal.status.is_automatically_recoverable())
    {
        let resources = scanned.journal.lock_resources();
        let Some(taken) = claim(&resources, already_held)? else {
            report.active.push(scanned.path.clone());
            continue;
        };
        already_held.absorb(taken);
        report.quarantined.push(scanned.path.clone());
    }
    if !report.quarantined.is_empty() {
        return Ok(report);
    }

    for scanned in scan
        .incomplete()
        .filter(|scanned| scanned.journal.status.is_automatically_recoverable())
    {
        let resources = scanned.journal.lock_resources();
        let Some(taken) = claim(&resources, already_held)? else {
            report.active.push(scanned.path.clone());
            continue;
        };
        already_held.absorb(taken);

        // The file the journal was read from, never a path re-derived from its recorded id. The
        // two agree for every journal this crate wrote; reconciling the derived one instead would
        // leave a mismatched file behind to be recovered again on every later run.
        let mut transaction =
            Transaction::adopt(scanned.journal.clone(), scanned.path.clone(), already_held)?;
        let outcome = transaction.cleanup_recovered()?;
        report.reconciled.push(ReconciledTransaction {
            transaction: scanned.journal.transaction_id.to_string(),
            journal: scanned.path.clone(),
            report: outcome,
        });
    }

    Ok(report)
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
                rejected.path.display(),
                rejected.reason
            )
        })
        .collect::<Vec<_>>();

    for scanned in scan.incomplete() {
        let resources = scanned.journal.lock_resources();
        // A transaction whose locks are held is somebody else's live session, not unrecovered
        // state. It is not this run's business and does not block it; the ordinary lock wait will
        // report it if the two sessions actually contend.
        if claim(&resources, already_held)?.is_some() {
            blocking.push(format!(
                "transaction {} is incomplete ({}) and --no-recover forbids handling it",
                scanned.journal.transaction_id,
                scanned.journal.status.label()
            ));
        }
    }
    Ok(blocking)
}

/// Takes every lock a journal names, or nothing at all.
///
/// A lock this session already holds counts as taken: the current run reached those resources
/// through its own plan, which is exactly the overlap that makes the stale transaction this run's
/// business.
fn claim(
    resources: &[LockResource],
    already_held: &HeldLocks,
) -> Result<Option<HeldLocks>, AppError> {
    if already_held.holds_all(resources) {
        return Ok(Some(HeldLocks::default()));
    }
    Ok(already_held
        .try_acquire_missing(resources, &LockOwner::preliminary())?
        .ok())
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
    use super::claim;
    use crate::lock::acquire::{HeldLocks, LockOwner, LockPolicy};
    use crate::lock::{LockResource, LockResourceKind};
    use crate::state::testing::StateRootGuard;
    use crate::test_support::TestDir;

    #[test]
    fn a_partial_overlap_claims_only_the_unheld_physical_key() {
        let fixture = TestDir::new("recovery-partial-lock");
        let _guard = StateRootGuard::set(fixture.path());
        let root = std::fs::canonicalize(fixture.path()).unwrap();
        let store = root.join("store");
        std::fs::create_dir_all(&store).expect("store fixture");
        let full = LockResource::describe(LockResourceKind::BackingStore, &root, &store).unwrap();
        assert_eq!(
            full.lock_keys().len(),
            2,
            "the resource must have both keys"
        );

        let mut logical_only = full.clone();
        logical_only.identity.physical = None;
        let mut current = HeldLocks::acquire(
            &[logical_only],
            LockPolicy::immediate(),
            &LockOwner::preliminary(),
        )
        .expect("the current session holds the logical key");

        let newly_claimed = claim(std::slice::from_ref(&full), &current)
            .expect("the missing lock file is available")
            .expect("the journal is stale, not active");
        assert_eq!(
            newly_claimed.keys().count(),
            1,
            "claim must not reacquire the logical key this process already holds"
        );
        current.absorb(newly_claimed);
        assert!(current.holds_all(&[full]));
    }
}
