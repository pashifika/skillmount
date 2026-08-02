//! Reconciling transactions that no process is still driving.
//!
//! The hard question is not what to remove; it is how to know nobody else is using it. The answer
//! is the lock set, and only the lock set. A journal's owner process id is recorded, but it is
//! never consulted: the operating system reuses process ids, so "that pid is gone" and "that pid
//! belongs to something unrelated now" look identical, and either reading can authorize deleting a
//! live session's mounts.
//!
//! So eligibility is decided by trying to take every lock the journal says its transaction held. If
//! all of them are free, no process is between apply and cleanup for that transaction and it can be
//! reconciled. If even one is held, the transaction is alive and is left completely alone — not
//! inspected, not reported as stale, not touched.
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
    /// Journals that exist but cannot be interpreted, and are therefore retained untouched.
    pub unreadable: Vec<RejectedJournal>,
}

impl RecoveryReport {
    /// Returns whether anything needs an operator's attention.
    #[must_use]
    pub fn needs_attention(&self) -> bool {
        !self.unreadable.is_empty()
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

/// Reconciles every incomplete transaction whose locks are all free.
///
/// `already_held` is the current session's lock set. Locks taken during recovery are absorbed into
/// it rather than released, so a transaction that was just reconciled cannot be resurrected by a
/// third session between recovery and this session's own apply.
///
/// # Errors
///
/// Returns [`AppError::Journal`] when the journal directory cannot be enumerated, and
/// [`AppError::Filesystem`] when a lock file cannot be opened. A journal that cannot be interpreted
/// is reported rather than returned as an error: it must not stop the healthy ones beside it from
/// being reconciled.
pub fn recover_stale(already_held: &mut HeldLocks) -> Result<RecoveryReport, AppError> {
    let scan: JournalScan = store::scan()?;
    let mut report = RecoveryReport {
        unreadable: scan.rejected.clone(),
        ..RecoveryReport::default()
    };

    for journal in scan.incomplete() {
        let resources = journal.lock_resources();
        let Some(taken) = claim(&resources, already_held)? else {
            report
                .active
                .push(store::journal_path(&journal.transaction_id)?);
            continue;
        };
        already_held.absorb(taken);

        let path = store::journal_path(&journal.transaction_id)?;
        let mut transaction = Transaction::adopt(journal.clone(), path.clone(), already_held)?;
        let outcome = transaction.cleanup_recovered()?;
        report.reconciled.push(ReconciledTransaction {
            transaction: journal.transaction_id.to_string(),
            journal: path,
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
pub fn blocking_state(already_held: &mut HeldLocks) -> Result<Vec<String>, AppError> {
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

    for journal in scan.incomplete() {
        let resources = journal.lock_resources();
        // A transaction whose locks are held is somebody else's live session, not unrecovered
        // state. It is not this run's business and does not block it; the ordinary lock wait will
        // report it if the two sessions actually contend.
        if claim(&resources, already_held)?.is_some() {
            blocking.push(format!(
                "transaction {} is incomplete ({}) and --no-recover forbids reconciling it",
                journal.transaction_id,
                journal.status.label()
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
    let missing = resources
        .iter()
        .filter(|resource| {
            !resource
                .lock_keys()
                .iter()
                .all(|key| already_held.holds(key))
        })
        .cloned()
        .collect::<Vec<_>>();
    if missing.is_empty() {
        return Ok(Some(HeldLocks::default()));
    }
    Ok(HeldLocks::try_acquire_all(&missing, &LockOwner::preliminary())?.ok())
}

impl Transaction {
    /// Reconciles an adopted journal, reusing the ordinary removal path.
    ///
    /// A terminal journal never reaches this function: [`recover_stale`] filters on
    /// [`crate::journal::TransactionStatus::is_incomplete`], so a `kept` transaction keeps its
    /// mounts and a `completed` one has nothing left to reconcile.
    fn cleanup_recovered(&mut self) -> Result<CleanupReport, AppError> {
        // `keep_mounts` is honoured across a crash. An operator who asked for the mounts to survive
        // the session did not ask for them to survive only an orderly exit.
        if self.journal.keep_mounts {
            self.advance(crate::journal::TransactionStatus::Kept)?;
            return Ok(CleanupReport {
                journal_retained: Some(self.journal_path().to_path_buf()),
                ..CleanupReport::default()
            });
        }
        self.cleanup()
    }
}
