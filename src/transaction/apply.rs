//! Write-ahead application of a planned transaction.
//!
//! The sequence for every created entry is fixed and every step of it is durable before the next
//! one runs:
//!
//! 1. `intent` — the journal says a temporary entry is about to appear at a named path.
//! 2. the temporary entry is created at that path.
//! 3. `staged` — its platform identity is durable, so it can now be proved to belong here.
//! 4. the entry is placed atomically at its final path, replacing nothing.
//! 5. `applied` — the journal says the final path holds this transaction's entry.
//!
//! Stopping anywhere in that sequence leaves state a later invocation can reconcile. Stopping
//! between 4 and 5 is the awkward one, because the journal says `staged` while the entry already
//! sits at its final path; recovery handles it by inspecting both paths and removing only the one
//! whose identity matches.
//!
//! The final destination is never mutated before step 1, which is what the specification means by
//! "the journal precedes the mutation".

use std::fmt::{self, Write as _};
use std::path::Path;

use crate::checkpoint::{Checkpoint, reached};
use crate::domain::LinkMode;
use crate::error::{AppError, LinkError, PlanError};
use crate::journal::{ActionOperation, ActionStatus, RecordedKind, TransactionStatus};
use crate::link::{
    CreatedLink, EntryKind, LinkRequest, OwnedDirectory, PlacementOutcome, PlacementResidue,
};
use crate::mount::PathPrecondition;

use super::Transaction;

/// Why applying a plan stopped, and what rolling it back left behind.
///
/// The original failure and every rollback failure are carried together rather than one replacing
/// the other. A rollback that also failed is the situation an operator most needs the full picture
/// of, and reporting only the last error would hide the reason any of it happened.
#[derive(Debug)]
pub struct ApplyFailure {
    /// The failure that stopped the apply sequence.
    pub cause: AppError,
    /// Paths rollback could not verify, and why each was left alone.
    pub retained: Vec<super::cleanup::RetainedEntry>,
    /// Failures encountered while rolling back.
    pub rollback_errors: Vec<String>,
}

impl ApplyFailure {
    /// Returns the error a caller should report, with rollback context folded in.
    #[must_use]
    pub fn into_error(self) -> AppError {
        if self.retained.is_empty() && self.rollback_errors.is_empty() {
            return self.cause;
        }
        let mut message = format!("{}\n{}", self.cause, self.describe_residue());
        message.truncate(message.trim_end().len());
        // The category of the original cause is preserved: a destination conflict that also left
        // residue is still a destination conflict, and a caller keying on the exit code must not
        // see it turn into something else because cleanup was imperfect.
        match self.cause.category() {
            crate::error::ExitCategory::Temporary => AppError::Temporary(message),
            _ => AppError::Filesystem(message),
        }
    }

    /// Renders what was left behind, one line per retained path.
    #[must_use]
    pub fn describe_residue(&self) -> String {
        let mut rendered = String::new();
        for entry in &self.retained {
            let _ = writeln!(rendered, "retained {entry}");
        }
        for error in &self.rollback_errors {
            let _ = writeln!(rendered, "rollback error: {error}");
        }
        rendered
    }
}

impl fmt::Display for ApplyFailure {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(formatter, "{}", self.cause)?;
        let residue = self.describe_residue();
        if residue.is_empty() {
            return Ok(());
        }
        write!(formatter, "\n{}", residue.trim_end())
    }
}

impl Transaction {
    /// Applies every planned action, rolling back in reverse order if any of them fails.
    ///
    /// On success the journal is durably `active` and every created action is `applied`. On failure
    /// the journal is durably `failed`, carries the original error and every rollback error, and
    /// the transaction owns nothing that could still be verified and removed.
    ///
    /// # Errors
    ///
    /// Returns [`ApplyFailure`] describing the cause and anything rollback had to retain. It is
    /// boxed because it carries the original error plus every retained path, which is far larger
    /// than the success value and would otherwise widen every frame on the happy path.
    pub fn apply(&mut self) -> Result<(), Box<ApplyFailure>> {
        if let Err(error) = self.advance(TransactionStatus::Applying) {
            return Err(Box::new(Self::fail_without_rollback(error)));
        }
        reached(Checkpoint::JournalApplying, 1);

        for index in 0..self.journal.actions.len() {
            if let Err(error) = self.apply_one(index) {
                return Err(Box::new(self.roll_back(error)));
            }
        }

        if let Err(error) = self.advance(TransactionStatus::Active) {
            return Err(Box::new(self.roll_back(error)));
        }
        reached(Checkpoint::JournalActive, 1);
        Ok(())
    }

    /// Applies the action at `index`, leaving the journal durable at every boundary.
    fn apply_one(&mut self, index: usize) -> Result<(), AppError> {
        let sequence = u32::try_from(index + 1).unwrap_or(u32::MAX);
        match self.journal.actions[index].operation {
            ActionOperation::ReuseExistingLink => self.confirm_reuse(index),
            ActionOperation::CreateDirectory | ActionOperation::CreateDirectoryLink => {
                self.create_and_place(index, sequence)
            }
        }
    }

    /// Verifies that a reused entry still satisfies the mount, and records that nobody owns it.
    ///
    /// A reuse action never advances past [`ActionStatus::Reused`] and never becomes a rollback
    /// candidate. The entry belonged to the project or to another tool before this session started,
    /// and it must still be there when the session ends.
    fn confirm_reuse(&mut self, index: usize) -> Result<(), AppError> {
        let action = &self.journal.actions[index];
        let expected_source = action.source_canonical.clone().ok_or_else(|| {
            AppError::Internal("a reuse action must record its source".to_owned())
        })?;
        let path = action.final_path.clone();

        let live = self.backend.inspect_no_follow(&path)?;
        let reaches_source = matches!(
            live.kind,
            EntryKind::Directory | EntryKind::Symlink | EntryKind::Junction
        ) && self
            .backend
            .canonical_directory(&path)
            .is_ok_and(|canonical| canonical == expected_source);
        if !reaches_source {
            return Err(Self::drift(
                &path,
                &format!(
                    "the entry planned for reuse is now a {} rather than a directory resolving to {}",
                    live.kind.label(),
                    expected_source.display()
                ),
            ));
        }
        Ok(())
    }

    /// Runs the full intent/staged/applied sequence for one created entry.
    fn create_and_place(&mut self, index: usize, sequence: u32) -> Result<(), AppError> {
        let final_path = self.journal.actions[index].final_path.clone();
        let temporary = self.journal.actions[index]
            .temporary_path
            .clone()
            .ok_or_else(|| {
                AppError::Internal("a created action must record its staged sibling".to_owned())
            })?;

        self.check_precondition(index, &final_path)?;

        self.journal.actions[index].status = ActionStatus::Intent;
        self.persist()?;
        reached(Checkpoint::ActionIntent, sequence);

        let staged = self.create_staged(index, &temporary)?;
        reached(Checkpoint::TemporaryCreated, sequence);

        self.journal.actions[index].status = ActionStatus::Staged;
        self.persist()?;
        reached(Checkpoint::ActionStaged, sequence);

        // Placement is the point of no return for the destination, and it is the one operation the
        // backend guarantees is atomic and never replaces. A destination that appeared since the
        // precondition check therefore loses the race safely: nothing is overwritten and the staged
        // pathname remains available for an ownership-verified rollback attempt.
        let placed = match staged {
            Staged::Link(link) => match self.backend.place_no_replace(&link, &final_path)? {
                PlacementOutcome::Placed(placed) => Placed::Link(placed),
                PlacementOutcome::DestinationExists => {
                    return Err(Self::drift(
                        &final_path,
                        "another process created the destination after this plan was built",
                    ));
                }
                PlacementOutcome::OwnershipMismatch(residue) => {
                    return Err(self.placement_failure(index, &residue));
                }
            },
            Staged::Directory(directory) => {
                match self
                    .backend
                    .place_directory_no_replace(&directory, &final_path)?
                {
                    PlacementOutcome::Placed(placed) => Placed::Directory(placed),
                    PlacementOutcome::DestinationExists => {
                        return Err(Self::drift(
                            &final_path,
                            "another process created the destination after this plan was built",
                        ));
                    }
                    PlacementOutcome::OwnershipMismatch(residue) => {
                        return Err(self.placement_failure(index, &residue));
                    }
                }
            }
        };
        reached(Checkpoint::FinalPlaced, sequence);

        match placed {
            Placed::Link(link) => {
                self.journal.actions[index]
                    .identity
                    .clone_from(&link.identity);
                self.journal.actions[index].kind = link.kind.into();
                self.journal.actions[index].link_target = Some(link.target);
            }
            Placed::Directory(directory) => {
                self.journal.actions[index]
                    .identity
                    .clone_from(&directory.identity);
            }
        }
        self.journal.actions[index].status = ActionStatus::Applied;
        self.persist()?;
        reached(Checkpoint::ActionApplied, sequence);
        Ok(())
    }

    /// Creates the temporary entry and records everything a later removal will compare against.
    ///
    /// The identity is captured before the journal is written, not after placement: an entry whose
    /// identity never reached the disk can never be removed by recovery, so capturing it late would
    /// convert an ordinary crash into permanent residue.
    fn create_staged(&mut self, index: usize, temporary: &Path) -> Result<Staged, AppError> {
        match self.journal.actions[index].operation {
            ActionOperation::CreateDirectory => {
                let created = self.backend.create_directory(temporary)?;
                self.journal.actions[index]
                    .identity
                    .clone_from(&created.identity);
                self.journal.actions[index].kind = RecordedKind::Directory;
                Ok(Staged::Directory(created))
            }
            ActionOperation::CreateDirectoryLink => {
                let source = self.journal.actions[index]
                    .source_canonical
                    .clone()
                    .ok_or_else(|| {
                        AppError::Internal("a link action must record its source".to_owned())
                    })?;
                let created = self.backend.create_directory_link(&LinkRequest {
                    source,
                    staged_path: temporary.to_path_buf(),
                    mode: requested_mode(self.journal.actions[index].kind),
                })?;
                self.journal.actions[index]
                    .identity
                    .clone_from(&created.identity);
                self.journal.actions[index].kind = created.kind.into();
                self.journal.actions[index].link_target = Some(created.target.clone());
                Ok(Staged::Link(created))
            }
            ActionOperation::ReuseExistingLink => Err(AppError::Internal(
                "a reuse action never creates anything".to_owned(),
            )),
        }
    }

    /// Rejects a destination that no longer matches what the plan was built against.
    ///
    /// Re-checking here rather than trusting the plan is the whole reason the precondition is
    /// persisted. The plan was built under the lock, but a process outside `SkillMount` holds no
    /// lock at all, and a destination it created between planning and applying must stop the
    /// transaction rather than be overwritten.
    fn check_precondition(&mut self, index: usize, path: &Path) -> Result<(), AppError> {
        if self.journal.actions[index].expected_precondition != PathPrecondition::Missing {
            return Ok(());
        }
        let live = self.backend.inspect_no_follow(path)?;
        if live.kind == EntryKind::Missing {
            return Ok(());
        }
        Err(Self::drift(
            path,
            &format!(
                "a {} now occupies a destination the plan expected to be missing",
                live.kind.label()
            ),
        ))
    }

    /// Builds the error for a destination whose observed state contradicts the plan.
    fn drift(path: &Path, reason: &str) -> AppError {
        AppError::Plan(Box::new(PlanError::UnsupportedLayout {
            path: path.to_path_buf(),
            reason: reason.to_owned(),
        }))
    }

    /// Records a placement result that must be reported but not repaired by this apply attempt.
    fn placement_failure(&mut self, index: usize, residue: &PlacementResidue) -> AppError {
        let reason = format!(
            "placement could not prove ownership: {}; the entry was retained",
            residue.mismatch.label()
        );
        let action_id = self.journal.actions[index].id;
        self.placement_residue.insert(
            action_id,
            super::cleanup::RetainedEntry {
                path: residue.path.clone(),
                reason: reason.clone(),
            },
        );
        Self::drift(&residue.path, &reason)
    }

    /// Records a failure that happened before anything could have been created.
    fn fail_without_rollback(cause: AppError) -> ApplyFailure {
        ApplyFailure {
            cause,
            retained: Vec::new(),
            rollback_errors: Vec::new(),
        }
    }
}

/// What was created at the staged path, carried between staging and placement.
enum Staged {
    Link(CreatedLink),
    Directory(OwnedDirectory),
}

/// Verified evidence relocated to an action's final path.
enum Placed {
    Link(CreatedLink),
    Directory(OwnedDirectory),
}

/// Maps a recorded kind back to the link mode the backend should honour.
///
/// [`RecordedKind::Undecided`] means the plan asked for `auto` and the backend has not chosen yet.
const fn requested_mode(kind: RecordedKind) -> LinkMode {
    match kind {
        RecordedKind::Symlink => LinkMode::Symlink,
        RecordedKind::Junction => LinkMode::Junction,
        RecordedKind::Directory | RecordedKind::Undecided => LinkMode::Auto,
    }
}

/// Converts a backend failure into the application error the caller reports.
impl From<LinkError> for ApplyFailure {
    fn from(error: LinkError) -> Self {
        Self {
            cause: AppError::Link(error),
            retained: Vec::new(),
            rollback_errors: Vec::new(),
        }
    }
}

impl std::error::Error for ApplyFailure {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        Some(&self.cause)
    }
}
