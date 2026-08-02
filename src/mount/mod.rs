//! Deterministic mount plans built entirely from read-only observation.

pub mod plan;
pub mod resolve;

use std::ffi::OsString;
use std::path::PathBuf;

use crate::agent::ScopeKind;
use crate::domain::{AgentId, LinkMode, SkillNameKey};
use crate::mount::resolve::PathKind;

/// Which discovery entry the child reads and which store backs it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DiscoveryPlan {
    /// Authoritative discovery entry the agent searches.
    pub entry: PathBuf,
    /// Store that selected Skills are mounted into.
    pub backing_store: PathBuf,
}

/// The destination state an action expects when it is applied.
///
/// The transaction change persists this in the journal and re-checks it under lock, so a plan
/// built against stale state fails instead of overwriting something that appeared in between.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PathPrecondition {
    /// Nothing exists at the destination.
    Missing,
    /// A directory link already points at the recorded source.
    ExistingLinkToSource,
}

impl PathPrecondition {
    /// Returns the stable label used in read-only output and in the journal.
    #[must_use]
    pub const fn label(self) -> &'static str {
        match self {
            Self::Missing => "missing",
            Self::ExistingLinkToSource => "existing_link_to_source",
        }
    }

    /// Parses a label a journal recorded earlier.
    #[must_use]
    pub fn parse(label: &str) -> Option<Self> {
        match label {
            "missing" => Some(Self::Missing),
            "existing_link_to_source" => Some(Self::ExistingLinkToSource),
            _ => None,
        }
    }
}

/// One filesystem operation a later transaction would perform.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum MountAction {
    /// Create a directory that does not exist yet.
    CreateDirectory {
        /// Directory to create.
        path: PathBuf,
    },
    /// Create a directory link at `destination` pointing at `source`.
    CreateDirectoryLink {
        /// Canonical directory the link refers to.
        source: PathBuf,
        /// Path the link is created at.
        destination: PathBuf,
        /// Requested link implementation; `Auto` is resolved by the platform backend at apply
        /// time, because the Windows symlink-to-junction fallback depends on runtime privilege.
        mode: LinkMode,
    },
    /// An entry already points at the intended source, so nothing is created.
    ///
    /// A reused entry is never transaction-owned and must never be removed at cleanup.
    ReuseExistingLink {
        /// Canonical directory the existing entry refers to.
        source: PathBuf,
        /// Existing entry that satisfies the mount.
        destination: PathBuf,
    },
}

impl MountAction {
    /// Returns the stable verb used in read-only output.
    #[must_use]
    pub const fn verb(&self) -> &'static str {
        match self {
            Self::CreateDirectory { .. } => "MKDIR",
            Self::CreateDirectoryLink { .. } => "LINK",
            Self::ReuseExistingLink { .. } => "REUSE",
        }
    }

    /// Returns whether a later cleanup owns the entry this action produces.
    #[must_use]
    pub const fn is_transaction_owned(&self) -> bool {
        matches!(
            self,
            Self::CreateDirectory { .. } | Self::CreateDirectoryLink { .. }
        )
    }
}

/// One ordered, journalable step of a mount plan.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PlannedMountAction {
    /// Position in the plan, starting at 1. Journal records refer to actions by this id.
    pub id: u32,
    /// What the action does.
    pub operation: MountAction,
    /// Destination state the action requires.
    pub expected_precondition: PathPrecondition,
    /// Transaction-unique staging sibling used by the write-ahead apply sequence.
    ///
    /// Always `None` in a preliminary plan: the name embeds a session identifier that does not
    /// exist until a transaction opens, and inventing one here would make `--dry-run` output
    /// non-deterministic for identical input.
    pub temporary_path: Option<PathBuf>,
}

/// A selected Skill that was omitted because an existing entry is preserved.
///
/// Preserved entries are reported but generate no action: the V2 design forbids journaling a
/// mutation for something that is deliberately left alone.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PreservedSkill {
    /// Comparison key of the omitted Skill.
    pub comparison_key: SkillNameKey,
    /// Existing entry that is kept.
    pub existing: PathBuf,
    /// Classification of the kept entry.
    pub existing_kind: PathKind,
    /// Scope the kept entry lives in.
    pub scope: ScopeKind,
    /// Canonical source that was not mounted.
    pub omitted_source: PathBuf,
}

/// How the child process would be started.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LaunchPlan {
    /// Explicit executable path, or the bare name resolved through `PATH`.
    pub executable: PathBuf,
    /// Working directory the child runs in.
    pub cwd: PathBuf,
    /// Arguments the adapter adds.
    pub injected_args: Vec<OsString>,
    /// Arguments forwarded verbatim after `--`.
    pub passthrough_args: Vec<OsString>,
}

impl LaunchPlan {
    /// Returns the full argument vector the child would receive.
    ///
    /// Values are returned as separate items and never joined: a joined string would carry
    /// quoting that a reader could reinterpret.
    #[must_use]
    pub fn effective_argv(&self) -> Vec<OsString> {
        let mut argv =
            Vec::with_capacity(1 + self.injected_args.len() + self.passthrough_args.len());
        argv.push(self.executable.clone().into_os_string());
        argv.extend(self.injected_args.iter().cloned());
        argv.extend(self.passthrough_args.iter().cloned());
        argv
    }
}

/// A complete plan for one session, produced before any mutation.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MountPlan {
    /// Adapter the plan belongs to.
    pub agent: AgentId,
    /// Discovery entry and backing store the plan targets.
    pub discovery: DiscoveryPlan,
    /// Actions in dependency order, ids assigned in that order.
    pub actions: Vec<PlannedMountAction>,
    /// Selected Skills omitted because an existing entry is preserved.
    pub preserved: Vec<PreservedSkill>,
    /// How the child would be launched.
    pub launch: LaunchPlan,
}

impl MountPlan {
    /// Returns the actions a later cleanup would own.
    pub fn owned_actions(&self) -> impl Iterator<Item = &PlannedMountAction> {
        self.actions
            .iter()
            .filter(|action| action.operation.is_transaction_owned())
    }
}

/// Assigns action ids in the order actions are appended.
///
/// Actions are built in dependency order rather than sorted afterwards, so the id sequence and
/// the apply order are the same thing and cannot drift apart.
#[derive(Debug, Default)]
pub(crate) struct ActionSequence {
    actions: Vec<PlannedMountAction>,
}

impl ActionSequence {
    pub(crate) fn push(&mut self, operation: MountAction, precondition: PathPrecondition) {
        let id = u32::try_from(self.actions.len() + 1).unwrap_or(u32::MAX);
        self.actions.push(PlannedMountAction {
            id,
            operation,
            expected_precondition: precondition,
            temporary_path: None,
        });
    }

    pub(crate) fn into_actions(self) -> Vec<PlannedMountAction> {
        self.actions
    }
}
