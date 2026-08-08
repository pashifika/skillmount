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
    TransactionId, TransactionJournal, TransactionStatus, staged_sibling, store,
};
use crate::link::{LinkBackend, platform_backend};
use crate::lock::acquire::HeldLocks;
use crate::lock::{LockAccess, LockResource, LockResourceKind};
use crate::mount::{MountAction, MountPlan};

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
    /// Opens a transaction whose identifier was minted before lock-set stabilization.
    ///
    /// The caller must use the same identifier to derive every staged-sibling resource before
    /// acquisition. Minting here would make those exact logical keys unknowable to the caller.
    ///
    /// # Errors
    ///
    /// Returns [`AppError::Internal`] when the caller does not hold the complete plan and mutation
    /// lock set, and [`AppError::Journal`] when the journal cannot be made durable. In both cases no
    /// destination mutation has been attempted.
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
        // locks, not of the code that removes. Observation locks still matter for snapshot
        // stability, but they never authorize a journal that may create or remove an entry.
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
        let mutation_resources = required_mutation_resources(&actions, plan, snapshot)?;
        if !locks.holds_all(&mutation_resources) {
            return Err(AppError::Internal(
                "a transaction may only open while mutation access is held for every owned \
                 destination"
                    .to_owned(),
            ));
        }

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

    /// Returns every still-pending created Skill link path for a cleanup failure diagnostic.
    ///
    /// This is evidence, not removal authority. The caller uses it only when journal persistence
    /// fails before cleanup can return a structured report, so the operator still learns which
    /// mounted entries remain.
    pub(crate) fn cleanup_critical_paths(&self) -> Vec<PathBuf> {
        let mut paths = Vec::new();
        for action in self
            .journal
            .cleanup_candidates()
            .filter(|action| action.operation == ActionOperation::CreateDirectoryLink)
        {
            let path = action.current_path();
            if !paths.contains(path) {
                paths.push(path.clone());
            }
        }
        paths
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

/// Returns the exact mutation resources for transaction-unique staged siblings.
///
/// These are added only after a mutating session has minted its transaction id. Dry-run planning
/// never calls this helper, so its deterministic placeholder plan remains lock- and id-free.
pub(crate) fn staged_mutation_resources(
    transaction_id: &TransactionId,
    plan: &MountPlan,
) -> Result<Vec<LockResource>, AppError> {
    let mut resources = Vec::new();
    for planned in &plan.actions {
        let final_path = match &planned.operation {
            MountAction::CreateDirectory { path } => path,
            MountAction::CreateDirectoryLink { destination, .. } => destination,
            MountAction::ReuseExistingLink { .. } => continue,
        };
        let temporary = staged_sibling(transaction_id, planned.id, final_path);
        resources.push(LockResource::describe_shared(
            LockResourceKind::DiscoveryEntry,
            LockAccess::Mutate,
            &temporary,
        )?);
    }
    resources.sort_by_key(LockResource::ordering_key);
    resources.dedup();
    Ok(resources)
}

/// Returns the mutation resources that authorize every path this transaction may own.
///
/// A resource protects its own coherent logical path and descendants. Reused links are deliberately
/// absent: the transaction neither creates nor removes them. Returning the recorded resource itself
/// also retains its physical key when the destination container already exists.
fn required_mutation_resources(
    actions: &[JournalAction],
    plan: &MountPlan,
    snapshot: &DiscoverySnapshot,
) -> Result<Vec<LockResource>, AppError> {
    let mut required_paths = vec![
        (
            plan.discovery.entry.clone(),
            Some(LockResourceKind::DiscoveryEntry),
            false,
        ),
        (
            plan.discovery.backing_store.clone(),
            Some(LockResourceKind::BackingStore),
            false,
        ),
    ];
    for action in actions
        .iter()
        .filter(|action| action.operation.creates_entry())
    {
        if let Some(temporary) = &action.temporary_path {
            required_paths.push((
                temporary.clone(),
                Some(LockResourceKind::DiscoveryEntry),
                true,
            ));
        }
        required_paths.push((action.final_path.clone(), None, false));
    }
    let mut required = Vec::new();

    for (path, kind, exact) in required_paths {
        let resource = snapshot
            .lock_resources
            .iter()
            .filter(|resource| kind.is_none_or(|required| resource.kind == required))
            .filter(|resource| {
                if exact {
                    resource.authorizes_exact_mutation_of(&path)
                } else {
                    resource.authorizes_mutation_of(&path)
                }
            })
            .max_by_key(|resource| resource.identity.logical_path().components().count())
            .ok_or_else(|| {
                AppError::Internal(format!(
                    "the plan has no mutation lock resource covering owned destination {}",
                    path.display()
                ))
            })?;
        if !required.contains(resource) {
            required.push(resource.clone());
        }
    }

    Ok(required)
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
        // Both create operations receive a transaction-unique staged sibling. Cleanup authority
        // differs between a Skill link and a helper directory, but the write-ahead sequence that
        // brings either into existence does not.
        temporary_path: operation
            .creates_entry()
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
