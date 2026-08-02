//! Reverse-order rollback and ordinary cleanup.
//!
//! Both walk the same list in the same direction for the same reason, and both use the same
//! removal code. The direction is reverse plan order: a helper directory is created before the
//! links inside it, so undoing it first would leave a directory that is no longer empty and that
//! cleanup then has to refuse.
//!
//! Removal itself is deliberately timid. An entry is removed only when the live entry still matches
//! every piece of evidence the journal recorded — kind, target, and platform identity for a link;
//! identity and emptiness for a directory. Anything else is retained and reported. That asymmetry
//! is the point: a retained entry is a nuisance an operator or a later run can clear, while a
//! removed one that belonged to somebody else is gone.

use std::fmt;
use std::path::{Path, PathBuf};

use crate::checkpoint::{Checkpoint, reached};
use crate::error::AppError;
use crate::journal::{ActionStatus, JournalAction, RecordedKind, TransactionStatus, store};
use crate::link::{
    CreatedLink, CreatedLinkKind, EntryKind, OwnedDirectory, OwnershipMismatch, RemoveOutcome,
};

use super::Transaction;

/// One entry cleanup declined to remove.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RetainedEntry {
    /// Path that was left exactly as it is.
    pub path: PathBuf,
    /// Why it could not be proved to belong to this transaction.
    pub reason: String,
}

impl fmt::Display for RetainedEntry {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(formatter, "{}: {}", self.path.display(), self.reason)
    }
}

/// What one cleanup or rollback pass did.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct CleanupReport {
    /// Paths whose entries were verified and removed.
    pub removed: Vec<PathBuf>,
    /// Paths that were left alone, with the reason for each.
    pub retained: Vec<RetainedEntry>,
    /// Failures the operating system reported while removing a verified entry.
    pub errors: Vec<String>,
    /// Why the journal survives this pass.
    pub journal_retained: Option<JournalRetention>,
}

/// Why a cleanup report leaves its journal on disk.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum JournalRetention {
    /// An orderly session reached the durable keep boundary requested by `--keep-mounts`.
    RequestedKeep(PathBuf),
    /// Cleanup did not finish, so the journal remains recovery evidence.
    IncompleteCleanup(PathBuf),
}

impl JournalRetention {
    /// Returns the retained journal path independent of the disposition.
    #[must_use]
    pub fn path(&self) -> &Path {
        match self {
            Self::RequestedKeep(path) | Self::IncompleteCleanup(path) => path,
        }
    }
}

impl CleanupReport {
    /// Returns whether anything needs an operator's attention.
    #[must_use]
    pub fn needs_attention(&self) -> bool {
        !self.retained.is_empty() || !self.errors.is_empty()
    }

    /// Renders every retained path and error, one per line.
    #[must_use]
    pub fn describe(&self) -> Vec<String> {
        let mut lines = Vec::new();
        for entry in &self.retained {
            lines.push(format!("retained {entry}"));
        }
        for error in &self.errors {
            lines.push(format!("cleanup error: {error}"));
        }
        if let Some(retention) = &self.journal_retained {
            match retention {
                JournalRetention::RequestedKeep(path) => lines.push(format!(
                    "transaction journal {} and its mounts were retained because --keep-mounts \
                     was requested; they require an explicit cleanup policy",
                    path.display()
                )),
                JournalRetention::IncompleteCleanup(path) => lines.push(format!(
                    "transaction journal {} is retained because cleanup could not finish",
                    path.display()
                )),
            }
        }
        lines
    }
}

impl Transaction {
    /// Removes everything this transaction owns, after the child has finished with it.
    ///
    /// `--keep-mounts` short-circuits the whole pass: the journal reaches the terminal `kept` state
    /// and stays on disk, so no later invocation treats the retained entries as stale and removes
    /// them behind the operator's back.
    ///
    /// # Errors
    ///
    /// Returns [`AppError::Journal`] when a status transition cannot be made durable. A removal
    /// that fails or is refused is reported in the [`CleanupReport`] rather than as an error,
    /// because the remaining entries still have to be attempted.
    pub fn cleanup(&mut self) -> Result<CleanupReport, AppError> {
        // Enter cleanup durably before deciding whether this orderly session may terminalize a
        // keep request. A crash in planned, applying, active, failed, or even at this cleaning
        // boundary remains incomplete and recovery reconciles it; only the following durable
        // `kept` transition turns requested retention into a terminal state.
        self.advance(TransactionStatus::Cleaning)?;
        reached(Checkpoint::JournalCleaning, 1);
        if self.journal.keep_mounts {
            self.advance(TransactionStatus::Kept)?;
            return Ok(CleanupReport {
                journal_retained: Some(JournalRetention::RequestedKeep(self.path.clone())),
                ..CleanupReport::default()
            });
        }

        let mut report = self.remove_owned_entries()?;

        if report.needs_attention() {
            // A failed cleanup keeps its journal. The retained entries are still described by it,
            // and discarding the description would make them unrecoverable by anything but hand.
            for line in report.describe() {
                self.record_error(line);
            }
            self.advance(TransactionStatus::Failed)?;
            report.journal_retained = Some(JournalRetention::IncompleteCleanup(self.path.clone()));
            return Ok(report);
        }

        // Only now, with nothing left that anyone could need to reconcile, does the journal become
        // removable. Marking it completed first means a crash between the two leaves a terminal
        // journal that recovery correctly leaves alone.
        self.advance(TransactionStatus::Completed)?;
        store::remove(&self.path)?;
        Ok(report)
    }

    /// Undoes everything already created, then records the failure durably.
    pub(super) fn roll_back(&mut self, cause: AppError) -> Box<super::apply::ApplyFailure> {
        let mut retained = Vec::new();
        let mut rollback_errors = Vec::new();

        match self.remove_owned_entries() {
            Ok(report) => {
                retained = report.retained;
                rollback_errors = report.errors;
            }
            Err(error) => rollback_errors.push(error.to_string()),
        }

        self.record_error(cause.to_string());
        for entry in &retained {
            self.record_error(format!("retained {entry}"));
        }
        for error in &rollback_errors {
            self.record_error(format!("rollback error: {error}"));
        }
        if let Err(error) = self.advance(TransactionStatus::Failed) {
            rollback_errors.push(error.to_string());
        }

        Box::new(super::apply::ApplyFailure {
            cause,
            retained,
            rollback_errors,
        })
    }

    /// Walks owned actions newest-first, removing only what can still be proved.
    ///
    /// Each action is journalled as `rolled_back` immediately after its entry is gone, so a crash
    /// mid-pass never leaves the journal claiming ownership of something that no longer exists.
    fn remove_owned_entries(&mut self) -> Result<CleanupReport, AppError> {
        let mut report = CleanupReport::default();
        let order = self
            .journal
            .reversible_actions()
            .map(|action| action.id)
            .collect::<Vec<_>>();

        for (position, id) in order.into_iter().enumerate() {
            let sequence = u32::try_from(position + 1).unwrap_or(u32::MAX);
            let Some(index) = self.journal.actions.iter().position(|a| a.id == id) else {
                continue;
            };
            let action = self.journal.actions[index].clone();
            let outcome = self.clear_action(&action, sequence, &mut report);

            // The status is advanced only when nothing this transaction owns remains at either
            // candidate path. Leaving it otherwise is what keeps a retained entry described by the
            // journal, so a later run holding the same locks can try again.
            if outcome == Outcome::Cleared {
                self.journal.actions[index].status = ActionStatus::RolledBack;
                self.persist()?;
            }
        }
        Ok(report)
    }

    /// Tries every path an action's entry could occupy, temporary before final.
    ///
    /// The first path that holds anything decides the outcome. An absent path is not an outcome:
    /// a staged entry that was already placed is absent at its temporary path and present at its
    /// final one, and stopping at the first absence would leave the placed entry behind.
    fn clear_action(
        &self,
        action: &JournalAction,
        sequence: u32,
        report: &mut CleanupReport,
    ) -> Outcome {
        if action.kind == RecordedKind::Undecided {
            return self.inspect_undecided_candidates(action, report);
        }
        for path in action.candidate_paths() {
            match self.remove_candidate(action, &path) {
                Ok(RemoveOutcome::AlreadyAbsent) => {}
                Ok(RemoveOutcome::Removed) => {
                    report.removed.push(path);
                    reached(removal_checkpoint(action.kind), sequence);
                    return Outcome::Cleared;
                }
                Ok(RemoveOutcome::NotEmpty) => {
                    report.retained.push(RetainedEntry {
                        path,
                        reason: "the directory holds entries this transaction did not create, so \
                                 removing it would take them with it"
                            .to_owned(),
                    });
                    return Outcome::Kept;
                }
                Ok(RemoveOutcome::OwnershipMismatch(mismatch)) => {
                    report.retained.push(RetainedEntry {
                        path,
                        reason: mismatch_reason(mismatch),
                    });
                    return Outcome::Kept;
                }
                Err(error) => {
                    report.errors.push(error.to_string());
                    return Outcome::Kept;
                }
            }
        }
        // Nothing at either path. The process may have stopped before the entry was created, or a
        // previous pass may already have removed it; either way there is nothing left to own.
        Outcome::Cleared
    }

    /// Inspects both candidates for an intent whose concrete implementation was not durable.
    ///
    /// An `auto` link can be created before the backend's selected kind and identity reach the
    /// journal. With no ownership proof it may not be removed, but its existence must not be
    /// rewritten as absence either. Both paths are inspected because a crash can leave the entry
    /// before or after the atomic placement boundary.
    fn inspect_undecided_candidates(
        &self,
        action: &JournalAction,
        report: &mut CleanupReport,
    ) -> Outcome {
        let mut kept = false;
        for path in action.candidate_paths() {
            match self.backend.inspect_no_follow(&path) {
                Ok(entry) if entry.kind == EntryKind::Missing => {}
                Ok(entry) => {
                    kept = true;
                    report.retained.push(RetainedEntry {
                        path,
                        reason: format!(
                            "the entry exists as {} but its concrete kind and identity were not \
                             durably recorded, so ownership cannot be proved",
                            entry.kind.label()
                        ),
                    });
                }
                Err(error) => {
                    kept = true;
                    report.errors.push(error.to_string());
                }
            }
        }
        if kept {
            Outcome::Kept
        } else {
            Outcome::Cleared
        }
    }

    /// Attempts one candidate path with the evidence recorded for the action.
    fn remove_candidate(
        &self,
        action: &JournalAction,
        path: &Path,
    ) -> Result<RemoveOutcome, AppError> {
        match action.kind {
            RecordedKind::Directory => {
                Ok(self.backend.remove_empty_directory(&OwnedDirectory {
                    path: path.to_path_buf(),
                    identity: action.identity.clone(),
                })?)
            }
            RecordedKind::Symlink | RecordedKind::Junction => {
                let Some(source_canonical) = action.source_canonical.clone() else {
                    return Ok(RemoveOutcome::OwnershipMismatch(
                        OwnershipMismatch::IdentityUnavailable,
                    ));
                };
                // A link recorded without its written target cannot be verified: the target is what
                // keeps a dangling link removable after its source disappears.
                let Some(target) = action.link_target.clone() else {
                    return Ok(RemoveOutcome::OwnershipMismatch(
                        OwnershipMismatch::IdentityUnavailable,
                    ));
                };
                Ok(self.backend.remove_link_entry(&CreatedLink {
                    path: path.to_path_buf(),
                    kind: if action.kind == RecordedKind::Junction {
                        CreatedLinkKind::Junction
                    } else {
                        CreatedLinkKind::Symlink
                    },
                    target,
                    source_canonical,
                    identity: action.identity.clone(),
                })?)
            }
            // Handled by `inspect_undecided_candidates`, which must inspect both paths together.
            RecordedKind::Undecided => Ok(RemoveOutcome::AlreadyAbsent),
        }
    }
}

/// Whether an action still owns something after one pass.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Outcome {
    /// Nothing this transaction owns remains at either candidate path.
    Cleared,
    /// Something remains and was deliberately left alone.
    Kept,
}

/// Returns the checkpoint that fires after a removal of this kind.
const fn removal_checkpoint(kind: RecordedKind) -> Checkpoint {
    match kind {
        RecordedKind::Directory => Checkpoint::DirectoryRemoved,
        _ => Checkpoint::EntryRemoved,
    }
}

/// Explains a mismatch in terms of what the operator now has to decide about.
fn mismatch_reason(mismatch: OwnershipMismatch) -> String {
    format!(
        "{}, so it cannot be proved to belong to this session and was left untouched",
        mismatch.label()
    )
}
