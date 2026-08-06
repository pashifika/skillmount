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
use crate::journal::store::{self, JournalScan, RejectedJournal, ScannedJournal};
use crate::journal::{ActionOperation, TransactionJournal, TransactionStatus};
use crate::link::resolve::ComparablePath;
use crate::lock::LockResource;
use crate::lock::acquire::LockContention;
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
    pub quarantined: Vec<QuarantinedTransaction>,
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
        for entry in &self.quarantined {
            lines.push(format!(
                "transaction journal {} may still belong to a live child process domain and was quarantined without cleanup",
                entry.journal.display()
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
            Ok(locks) => locks,
        };
        let locks = match locks {
            Ok(locks) => locks,
            Err(contention) => {
                report.active.push(ActiveTransaction {
                    transaction,
                    agent,
                    journal: scanned.path,
                    contention,
                });
                continue;
            }
        };
        claimed_locks.absorb(locks);
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

fn reconcile_explicit_claims(
    claimed: Vec<ScannedJournal>,
    locks: &HeldLocks,
    report: &mut ExplicitCleanupReport,
) {
    let cleanup_order = explicit_cleanup_order(&claimed);
    let mut remaining = claimed.into_iter().map(Some).collect::<Vec<_>>();
    for index in cleanup_order {
        let scanned = remaining[index]
            .take()
            .expect("every claimed journal is cleaned exactly once");
        let transaction = scanned.journal.transaction_id.to_string();
        let agent = scanned.journal.agent;
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
        match adopted.cleanup_required() {
            Ok(outcome) => report.reconciled.push(ReconciledTransaction {
                transaction,
                agent,
                journal: scanned.path,
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

/// Orders a claimed batch so a transaction that owns an entry below another transaction's owned
/// helper directory is cleaned first.
///
/// Kept transactions can overlap legitimately: the first session owns the shared helper
/// directories, while later sessions reuse those directories and own only their mount links.
/// Cleaning the directory owner first would retain it as non-empty and strand its journal. The
/// journal actions provide the dependency evidence directly, without relying on transaction-id or
/// scan order as a proxy for which session created the helper.
fn explicit_cleanup_order(journals: &[ScannedJournal]) -> Vec<usize> {
    let mut prerequisites = vec![Vec::new(); journals.len()];
    for before in 0..journals.len() {
        for after in 0..journals.len() {
            if before != after
                && transaction_must_precede(&journals[before].journal, &journals[after].journal)
            {
                prerequisites[after].push(before);
            }
        }
    }

    let mut emitted = vec![false; journals.len()];
    let mut order = Vec::with_capacity(journals.len());
    while order.len() < journals.len() {
        let next = (0..journals.len()).find(|&index| {
            !emitted[index]
                && prerequisites[index]
                    .iter()
                    .all(|&required| emitted[required])
        });
        let Some(next) = next else {
            // Valid plans cannot form an ownership-containment cycle. Preserve deterministic
            // scan order if a future or externally edited journal set nevertheless does; the
            // ordinary ownership checks still retain rather than remove an uncertain entry.
            order.extend((0..journals.len()).filter(|&index| !emitted[index]));
            break;
        };
        emitted[next] = true;
        order.push(next);
    }
    order
}

fn transaction_must_precede(first: &TransactionJournal, second: &TransactionJournal) -> bool {
    first
        .actions
        .iter()
        .filter(|action| {
            action.operation.is_transaction_owned() && action.status.may_have_created_something()
        })
        .any(|first_action| {
            let first_path = ComparablePath::new(&first_action.final_path);
            second.actions.iter().any(|second_action| {
                if second_action.operation != ActionOperation::CreateDirectory
                    || !second_action.status.may_have_created_something()
                {
                    return false;
                }
                let directory = ComparablePath::new(&second_action.final_path);
                directory.contains(&first_path) && !directory.names_same_path(&first_path)
            })
        })
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
    crate::checkpoint::reached(crate::checkpoint::Checkpoint::JournalScanComplete, 1);
    let mut report = RecoveryReport {
        unreadable: scan.rejected.clone(),
        ..RecoveryReport::default()
    };
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
        let Some(taken) = claim(&resources, already_held)? else {
            report.active.push(scanned.path.clone());
            continue;
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
    if !report.unreadable.is_empty() || !report.quarantined.is_empty() {
        return Ok(report);
    }

    for scanned in recoverable {
        // The file the journal was read from, never a path re-derived from its recorded id. The
        // two agree for every journal this crate wrote; reconciling the derived one instead would
        // leave a mismatched file behind to be recovered again on every later run.
        let transaction_id = scanned.journal.transaction_id.to_string();
        let agent = scanned.journal.agent;
        let journal_path = scanned.path.clone();
        let mut transaction =
            Transaction::adopt(scanned.journal, journal_path.clone(), already_held)?;
        let outcome = transaction.cleanup_recovered()?;
        report.reconciled.push(ReconciledTransaction {
            transaction: transaction_id,
            agent,
            journal: journal_path,
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
