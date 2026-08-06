//! The mutating half of a session: apply, roll back, clean up, and recover.
//!
//! This module owns planned destination mutation and recovery. The application prepares private
//! control state and acquires locks before entering it, and every rule here exists because a session
//! can stop at any instruction.
//!
//! Three invariants hold across the whole module:
//!
//! - **No planned destination is created before its intent is durable.** A journal describing the
//!   mutation reaches the disk first, so a crash always leaves a record of what may exist.
//! - **Nothing is removed without proof.** Every removal compares the live entry against recorded
//!   evidence, and an entry that cannot be proved to belong to this transaction is retained and
//!   reported instead. Residue is recoverable; deleting someone's Skills is not.
//! - **The application keeps every planned destination under its locks.** Apply and ordinary cleanup
//!   use the discovery-derived keys checked when the transaction opens; recovery also holds every
//!   key recorded by the journal it adopts. A public caller must currently keep that validated
//!   `HeldLocks` guard alive because `Transaction` does not retain it structurally.

pub mod apply;
pub mod cleanup;
pub mod recover;

#[cfg(test)]
mod tests;

use std::collections::BTreeMap;
use std::fmt;
use std::path::{Path, PathBuf};

use crate::agent::DiscoverySnapshot;
use crate::domain::{LinkMode, RunContext, SkillCatalog};
use crate::error::AppError;
use crate::journal::{
    ActionOperation, ActionStatus, JournalAction, JournalLock, RecordedKind, SourceResolution,
    TransactionId, TransactionJournal, TransactionStatus, store,
};
use crate::link::{LinkBackend, platform_backend};
use crate::lock::acquire::HeldLocks;
use crate::mount::{MountAction, MountPlan};

/// Filename prefix every staged entry uses.
///
/// The leading dot keeps a staged entry out of the way of an agent that lists the store while a
/// transaction is in flight, and the fixed prefix means a leftover from a crashed run is
/// identifiable on sight even before its journal is read.
const STAGING_PREFIX: &str = ".skillmount-";

/// One open transaction and the journal that describes it.
///
/// The journal in memory and the journal on disk are kept in step by construction: every mutating
/// method persists before it acts, so the value here never describes less than the disk does.
pub struct Transaction {
    journal: TransactionJournal,
    path: PathBuf,
    backend: &'static dyn LinkBackend,
    placement_residue: BTreeMap<u32, cleanup::RetainedEntry>,
}

/// Renders the transaction without its backend, which is a stateless singleton with no
/// representation worth printing.
impl fmt::Debug for Transaction {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("Transaction")
            .field("journal", &self.journal)
            .field("path", &self.path)
            .field("placement_residue", &self.placement_residue)
            .finish_non_exhaustive()
    }
}

impl Transaction {
    /// Builds the complete `planned` journal for `plan` and persists it before anything is created.
    ///
    /// `locks` must already hold every key the snapshot's resources need. Nothing on the filesystem
    /// is touched by this call except the journal itself, which lives outside the project.
    ///
    /// # Errors
    ///
    /// Returns [`AppError::Internal`] when the caller does not hold the plan's locks, and
    /// [`AppError::Journal`] when the journal cannot be made durable. In both cases no mutation has
    /// been attempted.
    pub fn open(
        context: &RunContext,
        catalog: &SkillCatalog,
        plan: &MountPlan,
        snapshot: &DiscoverySnapshot,
        locks: &HeldLocks,
    ) -> Result<Self, AppError> {
        Self::open_with(
            context,
            catalog,
            plan,
            snapshot,
            locks,
            TransactionId::mint(),
        )
    }

    /// Opens a transaction with a caller-supplied identifier, so the staging root and the journal
    /// name can share one value.
    ///
    /// # Errors
    ///
    /// Returns the same errors as [`Transaction::open`].
    pub fn open_with(
        context: &RunContext,
        catalog: &SkillCatalog,
        plan: &MountPlan,
        snapshot: &DiscoverySnapshot,
        locks: &HeldLocks,
        transaction_id: TransactionId,
    ) -> Result<Self, AppError> {
        // Checked rather than documented. Every removal this transaction will later perform is safe
        // only because no other session can reach the same entry, and that is a property of the
        // locks, not of the code that removes. A caller that skipped them must fail here, before the
        // journal exists, rather than produce entries whose safety rests on an assumption.
        if !locks.holds_all(&snapshot.lock_resources) {
            return Err(AppError::Internal(
                "a transaction may only open while its plan's resource locks are held".to_owned(),
            ));
        }

        let actions = plan
            .actions
            .iter()
            .map(|planned| journal_action(&transaction_id, planned))
            .collect::<Vec<_>>();

        let journal = TransactionJournal {
            transaction_id,
            agent: context.agent_id(),
            owner_pid: std::process::id(),
            status: TransactionStatus::Planned,
            project_root: context.project_root.clone(),
            launch_cwd: context.launch_cwd.clone(),
            discovery_entry: plan.discovery.entry.clone(),
            backing_store: plan.discovery.backing_store.clone(),
            keep_mounts: context.options.keep_mounts,
            sources: catalog
                .resolutions
                .iter()
                .map(|resolution| SourceResolution {
                    mount_name: resolution.selected.mount_name.to_string(),
                    source_ordinal: resolution.selected.origin.source_ordinal,
                    source_entry: resolution.selected.origin.source_entry.clone(),
                    source_canonical: resolution.selected.origin.source_canonical.clone(),
                })
                .collect(),
            locks: snapshot
                .lock_resources
                .iter()
                .map(JournalLock::from)
                .collect(),
            actions,
            errors: Vec::new(),
        };

        let mut transaction = Self {
            path: store::journal_path(&journal.transaction_id)?,
            journal,
            backend: platform_backend(),
            placement_residue: BTreeMap::new(),
        };
        transaction.persist()?;
        crate::checkpoint::reached(crate::checkpoint::Checkpoint::JournalPlanned, 1);
        Ok(transaction)
    }

    /// Adopts a journal read from disk, so recovery reuses the same removal code.
    ///
    /// # Errors
    ///
    /// Returns [`AppError::Internal`] when the caller does not hold the locks the journal records.
    /// Recovery decides eligibility by taking exactly those locks, so failing here means the
    /// eligibility test and the reconciliation disagree — which must never be resolved by removing
    /// something anyway.
    pub(crate) fn adopt(
        journal: TransactionJournal,
        path: PathBuf,
        locks: &HeldLocks,
    ) -> Result<Self, AppError> {
        if !locks.holds_all(&journal.lock_resources()) {
            return Err(AppError::Internal(format!(
                "transaction {} cannot be reconciled without holding every lock it recorded",
                journal.transaction_id
            )));
        }
        Ok(Self {
            journal,
            path,
            backend: platform_backend(),
            placement_residue: BTreeMap::new(),
        })
    }

    /// Returns the durable record.
    #[must_use]
    pub fn journal(&self) -> &TransactionJournal {
        &self.journal
    }

    /// Returns where the journal lives.
    #[must_use]
    pub fn journal_path(&self) -> &Path {
        &self.path
    }

    /// Durably records that a child may begin using the mounted entries.
    ///
    /// This transition precedes every spawn attempt. If the wrapper disappears afterwards, free
    /// advisory locks alone cannot prove that the child process domain is empty, so recovery
    /// quarantines this journal until an explicit cleanup decision.
    pub(crate) fn begin_supervision(&mut self) -> Result<(), AppError> {
        #[cfg(debug_assertions)]
        if std::env::var_os("SKILLMOUNT_TEST_FAIL_BEGIN_SUPERVISION").is_some() {
            return Err(crate::error::JournalError::Write {
                path: self.path.clone(),
                reason: "injected begin-supervision persistence failure".to_owned(),
            }
            .into());
        }
        self.advance(TransactionStatus::Supervising)?;
        crate::checkpoint::reached(crate::checkpoint::Checkpoint::JournalSupervising, 1);
        Ok(())
    }

    /// Lets a test damage the durable record the way an interrupted run would.
    ///
    /// Not exposed outside tests: every ordinary path reaches the journal through a method that
    /// persists, and a caller that could edit it in place could leave memory and disk disagreeing.
    #[cfg(test)]
    pub(crate) fn journal_mut(&mut self) -> &mut TransactionJournal {
        &mut self.journal
    }

    /// Writes the current journal state to the file this transaction owns.
    ///
    /// The path is fixed when the transaction is created or adopted and never recomputed, so a
    /// journal read from disk keeps being written back to the file it came from.
    fn persist(&mut self) -> Result<(), AppError> {
        store::persist(&self.journal, &self.path)
    }

    /// Advances the transaction status and makes it durable.
    fn advance(&mut self, status: TransactionStatus) -> Result<(), AppError> {
        self.journal.status = status;
        self.persist()
    }

    /// Appends an error to the durable record without changing the status.
    fn record_error(&mut self, message: String) {
        self.journal.errors.push(message);
    }
}

/// Builds the durable record for one planned action, assigning its staged sibling.
///
/// The staged name is derived here rather than in the planner because it embeds the transaction
/// identifier, which does not exist until a transaction opens. That is also why a preliminary plan
/// leaves `temporary_path` empty: inventing one would make two identical `--dry-run` invocations
/// print different output.
fn journal_action(
    transaction_id: &TransactionId,
    planned: &crate::mount::PlannedMountAction,
) -> JournalAction {
    let (operation, final_path, source_canonical, kind) = match &planned.operation {
        MountAction::CreateDirectory { path } => (
            ActionOperation::CreateDirectory,
            path.clone(),
            None,
            RecordedKind::Directory,
        ),
        MountAction::CreateDirectoryLink {
            source,
            destination,
            mode,
        } => (
            ActionOperation::CreateDirectoryLink,
            destination.clone(),
            Some(source.clone()),
            // `auto` is resolved by the backend at apply time, because the Windows fallback from a
            // symbolic link to a junction depends on privilege only observable then.
            match mode {
                LinkMode::Symlink => RecordedKind::Symlink,
                LinkMode::Junction => RecordedKind::Junction,
                LinkMode::Auto => RecordedKind::Undecided,
            },
        ),
        MountAction::ReuseExistingLink {
            source,
            destination,
        } => (
            ActionOperation::ReuseExistingLink,
            destination.clone(),
            Some(source.clone()),
            RecordedKind::Undecided,
        ),
    };

    JournalAction {
        id: planned.id,
        operation,
        expected_precondition: planned.expected_precondition,
        temporary_path: operation
            .is_transaction_owned()
            .then(|| staged_sibling(transaction_id, planned.id, &final_path)),
        final_path,
        source_canonical,
        link_target: None,
        kind,
        // A reuse action is born in its terminal state: nothing is created for it, so there is no
        // sequence for it to progress through and nothing for cleanup to own.
        status: if operation == ActionOperation::ReuseExistingLink {
            ActionStatus::Reused
        } else {
            ActionStatus::Planned
        },
        identity: None,
    }
}

/// Returns the transaction-unique sibling an entry is staged at.
///
/// A sibling, not a temporary directory elsewhere: placement is an atomic rename, and a rename is
/// only atomic within one filesystem. Staging in the destination's own directory is the only way to
/// guarantee that on a host where the store sits on a different volume from anything else.
fn staged_sibling(transaction_id: &TransactionId, action_id: u32, final_path: &Path) -> PathBuf {
    let parent = final_path.parent().unwrap_or_else(|| Path::new(""));
    parent.join(format!("{STAGING_PREFIX}{transaction_id}-{action_id}.tmp"))
}
