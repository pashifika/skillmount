//! Reverse-order rollback and ordinary cleanup.
//!
//! Both walk the same list in the same direction for the same reason, and both use the same
//! removal code. The direction is reverse plan order: a helper directory is created before the
//! links inside it, so undoing it first would always find a directory that is not empty.
//!
//! Removal itself is deliberately timid. The removal observation must match every piece of evidence
//! the journal recorded — kind, target, and platform identity for a link; identity and emptiness for
//! a directory. Windows then retains that verified object handle through disposition; ADR 0016
//! records why its identity, rather than mutable reparse metadata, remains the authority. Anything
//! that already mismatches is left exactly as it is and reported. That asymmetry is the point: a
//! retained entry is a nuisance an operator or a later run can clear, while a removed one that
//! belonged to somebody else is gone.
//!
//! What differs between the two dispositions is only how much a refusal costs. A created Skill link
//! is the entry through which a selected external Skill is visible, so an unresolved one keeps its
//! journal and can replace child success. A helper directory is scaffolding the links needed: once
//! they are reconciled nothing selected is reachable through it, so a directory this pass cannot
//! prune is preserved, reported, and released from transaction responsibility. ADR 0037 records why
//! treating both as one obligation turned an unrelated file into a failed session.

use std::fmt;
use std::path::{Path, PathBuf};

use crate::checkpoint::{Checkpoint, reached};
use crate::error::AppError;
use crate::journal::{ActionStatus, JournalAction, RecordedKind, TransactionStatus, store};
use crate::link::{
    CreatedLink, CreatedLinkKind, EntryKind, OwnedDirectory, OwnershipMismatch, RemoveOutcome,
};
use crate::mount::CleanupDisposition;

use super::Transaction;

/// One entry cleanup declined to remove.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RetainedEntry {
    /// Path that was left exactly as it is.
    pub path: PathBuf,
    /// Why it could not be proved to belong to this transaction.
    pub reason: String,
}

/// Renders the pair as one already-escaped line.
///
/// Both halves are escaped here rather than at each use site: this value ends up in the durable
/// journal, in a multiline operator diagnostic, and in a report, and a path that could contribute a
/// newline would forge a line in the last two.
impl fmt::Display for RetainedEntry {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            formatter,
            "{}: {}",
            crate::render::path_value(&self.path, true),
            crate::render::text_value(&self.reason)
        )
    }
}

/// What one cleanup or rollback pass did.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct CleanupReport {
    /// Paths whose entries were verified and removed.
    pub removed: Vec<PathBuf>,
    /// Cleanup-critical paths that were left alone, with the reason for each.
    ///
    /// Every entry here is a created Skill link this pass could not prove gone. That is what keeps
    /// the journal on disk and lets cleanup replace a successful child status.
    pub retained: Vec<RetainedEntry>,
    /// Discovery scaffolding the pass deliberately left in place, with the reason for each.
    ///
    /// A helper directory that is non-empty, replaced, or unremovable is preserved rather than
    /// emptied. Once every cleanup-critical link is reconciled nothing selected is reachable through
    /// it, so these are observations for verbose and operator output, never a failure.
    pub preserved_scaffolding: Vec<RetainedEntry>,
    /// Failures the operating system reported while removing a verified cleanup-critical entry.
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
    ///
    /// Preserved scaffolding is deliberately excluded. A directory this pass declined to remove
    /// exposes no selected Skill once every cleanup-critical link is reconciled, so counting it here
    /// would turn another writer's file into a failed session.
    #[must_use]
    pub fn needs_attention(&self) -> bool {
        !self.retained.is_empty() || !self.errors.is_empty()
    }

    /// Renders every unresolved cleanup-critical path and error, one per line.
    #[must_use]
    pub fn describe(&self) -> Vec<String> {
        let mut lines = Vec::new();
        for entry in &self.retained {
            lines.push(format!("retained {entry}"));
        }
        for error in &self.errors {
            lines.push(format!(
                "cleanup error: {}",
                crate::render::text_value(error)
            ));
        }
        if let Some(retention) = &self.journal_retained {
            let path = crate::render::path_value(retention.path(), true);
            match retention {
                JournalRetention::RequestedKeep(_) => lines.push(format!(
                    "transaction journal {path} and its mounts were retained because --keep-mounts \
                     was requested; they require an explicit cleanup policy"
                )),
                JournalRetention::IncompleteCleanup(_) => lines.push(format!(
                    "transaction journal {path} is retained because cleanup could not finish"
                )),
            }
        }
        lines
    }

    /// Renders every preserved-scaffolding observation, one per line.
    ///
    /// Kept out of [`CleanupReport::describe`] because none of these lines is a failure: a normal
    /// session stays silent about them, while verbose session output and the explicit cleanup report
    /// show what was left behind and why.
    #[must_use]
    pub fn describe_preserved(&self) -> Vec<String> {
        self.preserved_scaffolding
            .iter()
            .map(|entry| format!("preserved scaffolding {entry}"))
            .collect()
    }
}

impl Transaction {
    /// Reconciles every entry this transaction created, even when the terminal policy was keep.
    ///
    /// This is the pre-launch failure path: no child was allowed to use the mounts, so a failed
    /// compatibility or supervision-intent check must not turn `--keep-mounts` into retained state.
    /// Clearing the request before the durable `cleaning` transition persists that decision before
    /// the first removal.
    ///
    /// # Errors
    ///
    /// Returns the same errors and evidence-rich partial report as [`Self::cleanup`].
    pub fn cleanup_required(&mut self) -> Result<CleanupReport, AppError> {
        self.journal.keep_mounts = false;
        self.cleanup()
    }

    /// Removes everything this transaction owns when an orderly session releases it.
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
        // keep request. A crash in planned, applying, active, supervising, failed, or even at this
        // cleaning boundary remains incomplete. Once `cleaning` is durable it is automatically
        // recoverable; only the following durable `kept` transition makes retention terminal.
        self.advance(TransactionStatus::Cleaning)?;
        reached(Checkpoint::JournalCleaning, 1);
        if self.journal.keep_mounts {
            self.advance(TransactionStatus::Kept)?;
            return Ok(CleanupReport {
                journal_retained: Some(JournalRetention::RequestedKeep(self.path.clone())),
                ..CleanupReport::default()
            });
        }

        let mut report = self.reconcile_pending_actions()?;

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

        // Only now, with nothing cleanup-critical left that anyone could need to reconcile, does the
        // journal become removable. Preserved scaffolding is deliberately not a reason to keep it:
        // no selected Skill is reachable through a directory once every link is gone. Marking the
        // journal completed first means a crash between the two leaves a terminal journal that
        // recovery correctly leaves alone.
        self.advance(TransactionStatus::Completed)?;
        reached(Checkpoint::JournalCompleted, 1);
        store::remove(&self.path)?;
        Ok(report)
    }

    /// Undoes everything already created, then records the failure durably.
    pub(super) fn roll_back(&mut self, cause: AppError) -> super::apply::ApplyFailure {
        let mut retained = Vec::new();
        let mut preserved = Vec::new();
        let mut rollback_errors = Vec::new();

        match self.reconcile_pending_actions() {
            Ok(report) => {
                retained = report.retained;
                preserved = report.preserved_scaffolding;
                rollback_errors = report.errors;
            }
            Err(error) => rollback_errors.push(error.to_string()),
        }

        self.record_error(cause.to_string());
        for entry in &retained {
            self.record_error(format!("retained {entry}"));
        }
        // Scaffolding the pass preserved is durable evidence for an operator reading the journal of
        // a failed apply, but it is not part of the user-facing residue: the failure to explain is
        // the one that stopped the apply, not a directory nobody can reach a Skill through.
        for entry in &preserved {
            self.record_error(format!("preserved scaffolding {entry}"));
        }
        for error in &rollback_errors {
            self.record_error(format!("rollback error: {error}"));
        }
        if let Err(error) = self.advance(TransactionStatus::Failed) {
            rollback_errors.push(error.to_string());
        }

        super::apply::ApplyFailure {
            cause,
            retained,
            rollback_errors,
        }
    }

    /// Walks pending create actions newest-first, reconciling only what it can still prove.
    ///
    /// Both dispositions travel the same pass in the same direction and use the same sealed removal
    /// proofs; only the answer to "may this action stop being my responsibility?" differs. A
    /// cleanup-critical link must be gone; a best-effort helper directory may also be preserved.
    /// Either way the transition is journalled before the next action is touched, so a crash
    /// mid-pass never leaves the journal claiming an obligation it already discharged.
    fn reconcile_pending_actions(&mut self) -> Result<CleanupReport, AppError> {
        let mut report = CleanupReport::default();
        let order = self
            .journal
            .cleanup_candidates()
            .map(|action| action.id)
            .collect::<Vec<_>>();

        for (position, id) in order.into_iter().enumerate() {
            let sequence = u32::try_from(position + 1).unwrap_or(u32::MAX);
            let Some(index) = self.journal.actions.iter().position(|a| a.id == id) else {
                continue;
            };
            let action = self.journal.actions[index].clone();
            let disposition = action.operation.cleanup_disposition();
            let placement_residue = self.placement_residue.get(&id).cloned();
            if let Some(residue) = &placement_residue {
                record_left_alone(&mut report, disposition, residue.clone());
            }
            let outcome = self.clear_action(
                &action,
                sequence,
                &mut report,
                placement_residue.as_ref().map(|entry| entry.path.as_path()),
            );

            // An unresolved cleanup-critical entry keeps its action pending, which is what keeps it
            // described by the journal so a later run holding the same locks can try again. A
            // preserved directory is reconciled instead: nothing selected is reachable through it,
            // and revisiting it could only ever reach the same decision.
            if outcome != Outcome::Unresolved {
                self.journal.actions[index].status = ActionStatus::RolledBack;
                self.persist()?;
                if outcome == Outcome::Preserved {
                    reached(Checkpoint::ScaffoldingReconciled, sequence);
                }
            }
        }
        Ok(report)
    }

    /// Tries every path an action's entry could occupy, temporary before final.
    ///
    /// A candidate that cannot be proved is left alone and reported, but it does not hide a later
    /// candidate that may still hold the entry this action created. This matters when handle-bound
    /// placement moves the verified object while another actor installs a replacement at the old
    /// staged pathname. A placement residue has already classified one path, so only that exact
    /// candidate is skipped; it cannot hide the action's other candidate. Once one entry is removed
    /// the scan stops, because an action can own at most one object and a second removal would
    /// exceed the journal's evidence.
    fn clear_action(
        &self,
        action: &JournalAction,
        sequence: u32,
        report: &mut CleanupReport,
        residue_path: Option<&Path>,
    ) -> Outcome {
        let disposition = action.operation.cleanup_disposition();
        if action.kind == RecordedKind::Undecided {
            return self.inspect_undecided_candidates(action, disposition, report);
        }
        let mut left_alone = residue_path.is_some();
        for path in action.candidate_paths() {
            if residue_path == Some(path.as_path()) {
                continue;
            }
            match self.remove_candidate(action, &path) {
                Ok(RemoveOutcome::AlreadyAbsent) => {}
                Ok(RemoveOutcome::Removed) => {
                    report.removed.push(path);
                    reached(removal_checkpoint(action.kind), sequence);
                    return outcome_for(disposition, left_alone);
                }
                Ok(RemoveOutcome::NotEmpty) => {
                    record_left_alone(
                        report,
                        disposition,
                        RetainedEntry {
                            path,
                            reason: "the directory holds entries this transaction did not create, \
                                     so removing it would take them with it"
                                .to_owned(),
                        },
                    );
                    left_alone = true;
                }
                Ok(RemoveOutcome::OwnershipMismatch(mismatch)) => {
                    record_left_alone(
                        report,
                        disposition,
                        RetainedEntry {
                            path,
                            reason: mismatch_reason(mismatch),
                        },
                    );
                    left_alone = true;
                }
                Err(error) => {
                    // A failed prune of best-effort scaffolding is an observation rather than a
                    // cleanup failure: the path is untouched, and once every cleanup-critical link
                    // is reconciled no selected Skill is reachable through it.
                    match disposition {
                        CleanupDisposition::BestEffort => {
                            report.preserved_scaffolding.push(RetainedEntry {
                                path,
                                reason: format!(
                                    "pruning this scaffolding directory failed ({error}), so it \
                                     was left exactly as it is"
                                ),
                            });
                        }
                        CleanupDisposition::Required | CleanupDisposition::None => {
                            report.errors.push(error.to_string());
                        }
                    }
                    left_alone = true;
                }
            }
        }
        // Nothing left alone and nothing at either path: the process may have stopped before the
        // entry was created, or a previous pass may already have removed it.
        outcome_for(disposition, left_alone)
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
        disposition: CleanupDisposition,
        report: &mut CleanupReport,
    ) -> Outcome {
        let mut left_alone = false;
        for path in action.candidate_paths() {
            match self.backend.inspect_no_follow(&path) {
                Ok(entry) if entry.kind == EntryKind::Missing => {}
                Ok(entry) => {
                    left_alone = true;
                    record_left_alone(
                        report,
                        disposition,
                        RetainedEntry {
                            path,
                            reason: format!(
                                "the entry exists as {} but its concrete kind and identity were \
                                 not durably recorded, so ownership cannot be proved",
                                entry.kind.label()
                            ),
                        },
                    );
                }
                Err(error) => {
                    left_alone = true;
                    report.errors.push(error.to_string());
                }
            }
        }
        outcome_for(disposition, left_alone)
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

/// What responsibility an action still carries after one pass.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Outcome {
    /// Nothing this transaction created remains at either candidate path.
    Reconciled,
    /// A best-effort helper directory was deliberately left in place. Its responsibility is
    /// discharged even though the directory is still there.
    Preserved,
    /// A cleanup-critical entry could not be proved gone, so the action stays pending.
    Unresolved,
}

/// Maps "this pass left something alone" to the action's remaining responsibility.
///
/// A [`CleanupDisposition::None`] action never becomes a cleanup candidate; treating it as
/// unresolved keeps the fail-closed direction if one ever did.
const fn outcome_for(disposition: CleanupDisposition, left_alone: bool) -> Outcome {
    if !left_alone {
        return Outcome::Reconciled;
    }
    match disposition {
        CleanupDisposition::BestEffort => Outcome::Preserved,
        CleanupDisposition::Required | CleanupDisposition::None => Outcome::Unresolved,
    }
}

/// Records one entry the pass left exactly as it is, in the channel its disposition belongs to.
fn record_left_alone(
    report: &mut CleanupReport,
    disposition: CleanupDisposition,
    entry: RetainedEntry,
) {
    match disposition {
        CleanupDisposition::BestEffort => report.preserved_scaffolding.push(entry),
        CleanupDisposition::Required | CleanupDisposition::None => report.retained.push(entry),
    }
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
