//! The durable write-ahead record of one mutating session.
//!
//! A session outlives no process guarantee: power can be lost between any filesystem call and the
//! next, and a child agent can be force-killed at any moment. Memory-only ownership would therefore
//! leave entries nobody can prove belong to `SkillMount`, and the safe response to an unprovable
//! entry is to never touch it — which over time turns a crash into permanent residue.
//!
//! The journal removes that failure mode by making every mutation discoverable *before* it
//! happens. Each action records what it is about to do, then what it created, then that the
//! creation reached its destination. Recovery reads those three states and can always tell an entry
//! this crate owns from one it does not.
//!
//! Nothing in this module mutates the filesystem; [`store`] does that, and only it.

pub mod store;

mod codec;
#[cfg(test)]
mod tests;

use std::fmt;
use std::path::PathBuf;

use crate::domain::AgentId;
use crate::link::PlatformIdentity;
use crate::lock::{LockResource, LockResourceIdentity, LockResourceKind};
use crate::mount::{CleanupDisposition, PathPrecondition};

use codec::Line;

/// Journal schema this build writes and is willing to read.
///
/// A journal with any other version is refused rather than guessed at. A future version may record
/// paths this build would not remove and statuses it would misread, and recovery acts by deleting
/// entries — the one operation where a wrong guess is unrecoverable.
pub const SCHEMA_VERSION: u32 = 1;

/// Filename extension every journal uses.
pub const JOURNAL_EXTENSION: &str = "journal";

/// The identity of one transaction, used for its journal name and its staged entries.
///
/// The value is unique per process and per transaction within a process, and it is safe as a path
/// component on both supported platforms: only lowercase hexadecimal digits and hyphens.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct TransactionId(String);

impl TransactionId {
    /// Mints an identifier for a transaction that is about to open.
    ///
    /// Uniqueness comes from three independent parts because no single one is sufficient: the
    /// process id is reused by the operating system, the clock can step backwards, and a counter
    /// only orders transactions inside one process.
    #[must_use]
    pub fn mint() -> Self {
        use std::sync::atomic::{AtomicU64, Ordering};
        use std::time::{SystemTime, UNIX_EPOCH};

        static COUNTER: AtomicU64 = AtomicU64::new(0);

        let nanos = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map_or(0, |elapsed| elapsed.as_nanos());
        let sequence = COUNTER.fetch_add(1, Ordering::Relaxed);
        Self(format!(
            "{nanos:032x}-{:08x}-{sequence:08x}",
            std::process::id()
        ))
    }

    /// Accepts an identifier read back from a journal name or a journal body.
    ///
    /// The grammar is enforced rather than assumed: the value becomes a path component of a staged
    /// entry, so a journal carrying `../` would otherwise direct a later removal outside the store.
    #[must_use]
    pub fn parse(value: &str) -> Option<Self> {
        let valid = !value.is_empty()
            && value.len() <= 128
            && value
                .bytes()
                .all(|byte| byte.is_ascii_hexdigit() || byte == b'-');
        valid.then(|| Self(value.to_owned()))
    }

    /// Returns the identifier.
    #[must_use]
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl fmt::Display for TransactionId {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(&self.0)
    }
}

/// How far a transaction has durably progressed.
///
/// Only [`TransactionStatus::Completed`] and [`TransactionStatus::Kept`] are terminal. Every other
/// state needs attention before a later invocation may mutate overlapping resources.
/// [`TransactionStatus::Supervising`] is quarantined because free wrapper locks do not prove child
/// death; the remaining incomplete states are eligible for ordinary locked recovery.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TransactionStatus {
    /// The complete plan is durable and no mutation has been attempted.
    Planned,
    /// At least one action has been attempted.
    Applying,
    /// Every planned action is durably applied.
    Active,
    /// A child may be using the mounted entries; only proven process-domain death may advance it.
    Supervising,
    /// Ordinary cleanup is in progress.
    Cleaning,
    /// Cleanup finished and nothing cleanup-critical remains to reconcile.
    Completed,
    /// The operator asked for the mounts to be retained.
    Kept,
    /// Apply or cleanup failed; the journal carries every error.
    Failed,
}

impl TransactionStatus {
    /// Returns the stable label written to the journal.
    #[must_use]
    pub const fn label(self) -> &'static str {
        match self {
            Self::Planned => "planned",
            Self::Applying => "applying",
            Self::Active => "active",
            Self::Supervising => "supervising",
            Self::Cleaning => "cleaning",
            Self::Completed => "completed",
            Self::Kept => "kept",
            Self::Failed => "failed",
        }
    }

    /// Parses a label a journal recorded earlier.
    #[must_use]
    pub fn parse(label: &str) -> Option<Self> {
        match label {
            "planned" => Some(Self::Planned),
            "applying" => Some(Self::Applying),
            "active" => Some(Self::Active),
            "supervising" => Some(Self::Supervising),
            "cleaning" => Some(Self::Cleaning),
            "completed" => Some(Self::Completed),
            "kept" => Some(Self::Kept),
            "failed" => Some(Self::Failed),
            _ => None,
        }
    }

    /// Returns whether the transaction has reached a state nothing needs to reconcile.
    #[must_use]
    pub const fn is_terminal(self) -> bool {
        matches!(self, Self::Completed | Self::Kept)
    }

    /// Returns whether a later invocation must resolve this non-terminal transaction.
    #[must_use]
    pub const fn is_incomplete(self) -> bool {
        !self.is_terminal()
    }

    /// Returns whether free transaction locks are sufficient to authorize automatic recovery.
    ///
    /// A supervising wrapper can disappear while its child or descendant remains alive. Its locks
    /// then become free without proving process-domain death, so that state requires an explicit
    /// operator cleanup decision instead of stale-transaction recovery.
    #[must_use]
    pub const fn is_automatically_recoverable(self) -> bool {
        self.is_incomplete() && !matches!(self, Self::Supervising)
    }
}

/// How far one action has durably progressed.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ActionStatus {
    /// Recorded in the plan; nothing has been attempted.
    Planned,
    /// The transaction is about to create the temporary entry.
    Intent,
    /// The temporary entry exists and its identity is durable.
    Staged,
    /// The entry occupies its final path.
    Applied,
    /// The entry already existed and belongs to someone else; cleanup never touches it.
    Reused,
    /// Cleanup has no remaining responsibility for the entry this action created.
    ///
    /// The exact claim depends on the operation's [`CleanupDisposition`]. For a
    /// [`CleanupDisposition::Required`] link the entry was verified and removed, or was already
    /// absent, so it is physically gone. For a [`CleanupDisposition::BestEffort`] helper directory
    /// this records reconciliation only: the pass may have preserved a non-empty, replaced, or
    /// unremovable directory, because nothing a child could load depends on it once every required
    /// entry is reconciled. The label itself is unchanged, so an older build reads a newly written
    /// journal exactly as before.
    RolledBack,
}

impl ActionStatus {
    /// Returns the stable label written to the journal.
    #[must_use]
    pub const fn label(self) -> &'static str {
        match self {
            Self::Planned => "planned",
            Self::Intent => "intent",
            Self::Staged => "staged",
            Self::Applied => "applied",
            Self::Reused => "reused",
            Self::RolledBack => "rolled_back",
        }
    }

    /// Parses a label a journal recorded earlier.
    #[must_use]
    pub fn parse(label: &str) -> Option<Self> {
        match label {
            "planned" => Some(Self::Planned),
            "intent" => Some(Self::Intent),
            "staged" => Some(Self::Staged),
            "applied" => Some(Self::Applied),
            "reused" => Some(Self::Reused),
            "rolled_back" => Some(Self::RolledBack),
            _ => None,
        }
    }

    /// Returns whether an entry may exist because of this action.
    ///
    /// [`ActionStatus::Intent`] is included even though nothing is proven to exist: the process may
    /// have stopped between the record and the creation, or between the creation and the next
    /// record. Recovery therefore inspects the temporary path of an `intent` action, and removes it
    /// only when the live entry matches everything the journal does know.
    #[must_use]
    pub const fn may_have_created_something(self) -> bool {
        matches!(self, Self::Intent | Self::Staged | Self::Applied)
    }
}

/// What one journalled action does.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ActionOperation {
    /// Create a helper directory the plan needs.
    CreateDirectory,
    /// Create a directory link for a selected Skill.
    CreateDirectoryLink,
    /// Record an entry that already satisfies the mount and that this transaction did not create.
    ReuseExistingLink,
}

impl ActionOperation {
    /// Returns the stable label written to the journal.
    #[must_use]
    pub const fn label(self) -> &'static str {
        match self {
            Self::CreateDirectory => "mkdir",
            Self::CreateDirectoryLink => "link",
            Self::ReuseExistingLink => "reuse",
        }
    }

    /// Parses a label a journal recorded earlier.
    #[must_use]
    pub fn parse(label: &str) -> Option<Self> {
        match label {
            "mkdir" => Some(Self::CreateDirectory),
            "link" => Some(Self::CreateDirectoryLink),
            "reuse" => Some(Self::ReuseExistingLink),
            _ => None,
        }
    }

    /// Returns whether this operation creates an entry, and therefore needs a staged sibling and
    /// write-ahead progress.
    #[must_use]
    pub const fn creates_entry(self) -> bool {
        match self {
            Self::CreateDirectory | Self::CreateDirectoryLink => true,
            Self::ReuseExistingLink => false,
        }
    }

    /// Returns how much authority cleanup has over the entry this operation produced.
    ///
    /// The disposition is derived from the label already on disk rather than serialized beside it.
    /// A journal written before this rule existed therefore receives the same policy without a
    /// schema change, and no journal can record a kind and a disposition that disagree.
    #[must_use]
    pub const fn cleanup_disposition(self) -> CleanupDisposition {
        match self {
            Self::CreateDirectoryLink => CleanupDisposition::Required,
            Self::CreateDirectory => CleanupDisposition::BestEffort,
            Self::ReuseExistingLink => CleanupDisposition::None,
        }
    }
}

/// The kind of entry an action produces, as recorded for later verification.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RecordedKind {
    /// A regular directory recorded at this transaction's initial evidence boundary.
    Directory,
    /// A directory symbolic link.
    Symlink,
    /// A Windows junction.
    Junction,
    /// The implementation is not decided until the backend resolves `auto` at apply time.
    Undecided,
}

impl RecordedKind {
    /// Returns the stable label written to the journal.
    #[must_use]
    pub const fn label(self) -> &'static str {
        match self {
            Self::Directory => "directory",
            Self::Symlink => "symlink",
            Self::Junction => "junction",
            Self::Undecided => "undecided",
        }
    }

    /// Parses a label a journal recorded earlier.
    #[must_use]
    pub fn parse(label: &str) -> Option<Self> {
        match label {
            "directory" => Some(Self::Directory),
            "symlink" => Some(Self::Symlink),
            "junction" => Some(Self::Junction),
            "undecided" => Some(Self::Undecided),
            _ => None,
        }
    }
}

impl From<crate::link::CreatedLinkKind> for RecordedKind {
    fn from(kind: crate::link::CreatedLinkKind) -> Self {
        match kind {
            crate::link::CreatedLinkKind::Symlink => Self::Symlink,
            crate::link::CreatedLinkKind::Junction => Self::Junction,
        }
    }
}

/// Where one selected Skill came from, retained so recovery can explain a mount it did not plan.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SourceResolution {
    /// Destination component the Skill was mounted under.
    pub mount_name: String,
    /// Zero-based `--skills-dir` occurrence that won the overlay.
    pub source_ordinal: usize,
    /// Candidate directory as discovered through that occurrence.
    pub source_entry: PathBuf,
    /// Canonical directory the mount refers to.
    pub source_canonical: PathBuf,
}

/// One durable action record.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct JournalAction {
    /// Plan position, starting at 1.
    pub id: u32,
    /// What the action does.
    pub operation: ActionOperation,
    /// Destination state the action requires.
    pub expected_precondition: PathPrecondition,
    /// Transaction-unique sibling the entry is created at before placement.
    pub temporary_path: Option<PathBuf>,
    /// Path the entry ultimately occupies.
    pub final_path: PathBuf,
    /// Canonical directory a link refers to.
    pub source_canonical: Option<PathBuf>,
    /// Target exactly as written into the link entry.
    pub link_target: Option<PathBuf>,
    /// Implementation the entry uses.
    pub kind: RecordedKind,
    /// How far the action has durably progressed.
    pub status: ActionStatus,
    /// Platform identity captured when the entry was created.
    pub identity: Option<PlatformIdentity>,
}

impl JournalAction {
    /// Returns the path the entry currently occupies, given the action's durable status.
    ///
    /// This is a *best* guess, not a proof. The process can stop between placement and the
    /// `applied` record, so a `staged` action may already sit at its final path. Recovery inspects
    /// both, which is what [`JournalAction::candidate_paths`] returns.
    #[must_use]
    pub fn current_path(&self) -> &PathBuf {
        match self.status {
            ActionStatus::Intent | ActionStatus::Staged => {
                self.temporary_path.as_ref().unwrap_or(&self.final_path)
            }
            _ => &self.final_path,
        }
    }

    /// Returns every path an entry from this action could occupy, temporary before final.
    ///
    /// Order matters. A staged entry that was never placed lives at the temporary path, and a
    /// staged entry that *was* placed lives at the final path; verifying the temporary path first
    /// means the common recovery case removes the entry that could never have been reachable by the
    /// agent, before touching anything the agent may have already used.
    #[must_use]
    pub fn candidate_paths(&self) -> Vec<PathBuf> {
        let mut paths = Vec::with_capacity(2);
        if let Some(temporary) = &self.temporary_path {
            paths.push(temporary.clone());
        }
        if !paths.contains(&self.final_path) {
            paths.push(self.final_path.clone());
        }
        paths
    }
}

/// One resource the transaction holds a lock on, recorded so recovery can reconstruct the set.
///
/// Recovery must take every lock the original session held before it may touch anything. It cannot
/// recompute them: the plan that produced them is gone, and the filesystem it was computed against
/// has changed. Persisting the identity components is what makes the set reconstructible.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct JournalLock {
    /// What the resource protects.
    pub kind: LockResourceKind,
    /// Resource path as the transaction addressed it.
    pub path: PathBuf,
    /// Canonical directory the resource hangs beneath.
    pub anchor: PathBuf,
    /// Normalized path from the anchor to the resource.
    pub suffix: PathBuf,
    /// Physical identity, present only when the resource existed at planning time.
    pub physical: Option<PlatformIdentity>,
}

impl JournalLock {
    /// Rebuilds the lock resource this record describes.
    #[must_use]
    pub fn to_resource(&self) -> LockResource {
        LockResource {
            kind: self.kind,
            path: self.path.clone(),
            identity: LockResourceIdentity {
                anchor: self.anchor.clone(),
                suffix: self.suffix.clone(),
                physical: self.physical.clone(),
            },
        }
    }
}

impl From<&LockResource> for JournalLock {
    fn from(resource: &LockResource) -> Self {
        Self {
            kind: resource.kind,
            path: resource.path.clone(),
            anchor: resource.identity.anchor.clone(),
            suffix: resource.identity.suffix.clone(),
            physical: resource.identity.physical.clone(),
        }
    }
}

/// The complete durable record of one mutating session.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TransactionJournal {
    /// Identity of this transaction.
    pub transaction_id: TransactionId,
    /// Adapter the plan belongs to.
    pub agent: AgentId,
    /// Process that opened the transaction, retained for diagnostics only.
    ///
    /// Recovery eligibility never consults it. A process id is reused by the operating system, so
    /// "that pid is gone" and "that pid belongs to something else now" are indistinguishable, and
    /// acting on either would authorize deleting a live session's mounts.
    pub owner_pid: u32,
    /// How far the transaction has durably progressed.
    pub status: TransactionStatus,
    /// Project root the session planned against.
    pub project_root: PathBuf,
    /// Directory the agent would have run in.
    pub launch_cwd: PathBuf,
    /// Logical discovery entry the child reads.
    pub discovery_entry: PathBuf,
    /// Store selected Skills are mounted into.
    pub backing_store: PathBuf,
    /// Whether ordinary cleanup is suppressed for this transaction.
    pub keep_mounts: bool,
    /// Where every mounted Skill came from.
    pub sources: Vec<SourceResolution>,
    /// Every resource the transaction locked.
    pub locks: Vec<JournalLock>,
    /// Actions in dependency order.
    pub actions: Vec<JournalAction>,
    /// Original and rollback errors, oldest first.
    pub errors: Vec<String>,
}

impl TransactionJournal {
    /// Returns the file name this journal is stored under.
    #[must_use]
    pub fn file_name(&self) -> String {
        format!("{}.{JOURNAL_EXTENSION}", self.transaction_id)
    }

    /// Returns the recorded lock-resource descriptions in deterministic order.
    #[must_use]
    pub fn lock_resources(&self) -> Vec<LockResource> {
        let mut resources = self
            .locks
            .iter()
            .map(JournalLock::to_resource)
            .collect::<Vec<_>>();
        resources.sort_by_key(LockResource::ordering_key);
        resources.dedup();
        resources
    }

    /// Returns the actions that may have created something, newest first.
    ///
    /// Reverse plan order is the only safe order: a helper directory is created before the links
    /// inside it, so visiting the directory first would always find it non-empty. A best-effort
    /// directory whose action is already reconciled is excluded exactly like a removed link — the
    /// action, not the filesystem, records that cleanup has no remaining responsibility for it.
    pub fn cleanup_candidates(&self) -> impl Iterator<Item = &JournalAction> {
        self.actions.iter().rev().filter(|action| {
            action.operation.creates_entry() && action.status.may_have_created_something()
        })
    }

    /// Renders the journal into the lines the codec writes.
    fn to_lines(&self) -> Vec<Line> {
        let mut lines = Vec::with_capacity(4 + self.sources.len() + self.actions.len());

        let mut header = Line::new("transaction");
        header
            .push("id", codec::encode_text(self.transaction_id.as_str()))
            .push("agent", codec::encode_text(self.agent.label()))
            .push("status", codec::encode_text(self.status.label()))
            .push("pid", codec::encode_text(&self.owner_pid.to_string()))
            .push(
                "keep_mounts",
                codec::encode_text(if self.keep_mounts { "true" } else { "false" }),
            );
        lines.push(header);

        let mut paths = Line::new("paths");
        paths
            .push("project_root", codec::encode_path(&self.project_root))
            .push("launch_cwd", codec::encode_path(&self.launch_cwd))
            .push("discovery_entry", codec::encode_path(&self.discovery_entry))
            .push("backing_store", codec::encode_path(&self.backing_store));
        lines.push(paths);

        for source in &self.sources {
            let mut line = Line::new("source");
            line.push("name", codec::encode_text(&source.mount_name))
                .push(
                    "ordinal",
                    codec::encode_text(&source.source_ordinal.to_string()),
                )
                .push("entry", codec::encode_path(&source.source_entry))
                .push("canonical", codec::encode_path(&source.source_canonical));
            lines.push(line);
        }

        for lock in &self.locks {
            let mut line = Line::new("lock");
            line.push("kind", codec::encode_text(lock.kind.label()))
                .push("path", codec::encode_path(&lock.path))
                .push("anchor", codec::encode_path(&lock.anchor))
                .push("suffix", codec::encode_path(&lock.suffix))
                .push_optional(
                    "physical",
                    lock.physical
                        .as_ref()
                        .map(|identity| codec::encode_text(identity.as_str())),
                );
            lines.push(line);
        }

        for action in &self.actions {
            let mut line = Line::new("action");
            line.push("id", codec::encode_text(&action.id.to_string()))
                .push("op", codec::encode_text(action.operation.label()))
                .push(
                    "precondition",
                    codec::encode_text(action.expected_precondition.label()),
                )
                .push("kind", codec::encode_text(action.kind.label()))
                .push("status", codec::encode_text(action.status.label()))
                .push("final", codec::encode_path(&action.final_path))
                .push_optional(
                    "temp",
                    action.temporary_path.as_deref().map(codec::encode_path),
                )
                .push_optional(
                    "source",
                    action.source_canonical.as_deref().map(codec::encode_path),
                )
                .push_optional(
                    "target",
                    action.link_target.as_deref().map(codec::encode_path),
                )
                .push_optional(
                    "identity",
                    action
                        .identity
                        .as_ref()
                        .map(|identity| codec::encode_text(identity.as_str())),
                );
            lines.push(line);
        }

        for error in &self.errors {
            let mut line = Line::new("error");
            line.push("text", codec::encode_text(error));
            lines.push(line);
        }

        lines
    }

    /// Rebuilds a journal from parsed lines, rejecting anything it cannot fully understand.
    ///
    /// Every field is required unless the type marks it optional, and an unknown record name is a
    /// hard failure rather than something to skip. Skipping would let a journal written by a build
    /// that records more state be read as if it recorded less, and the missing state is exactly the
    /// evidence that decides whether an entry may be removed.
    fn from_lines(lines: &[Line]) -> Result<Self, String> {
        let header = require_one(lines, "transaction")?;
        let paths = require_one(lines, "paths")?;

        let mut journal = Self {
            transaction_id: TransactionId::parse(&text(header, "id")?)
                .ok_or_else(|| "the transaction id is not a legal identifier".to_owned())?,
            agent: AgentId::parse(&text(header, "agent")?)
                .ok_or_else(|| "unknown agent".to_owned())?,
            owner_pid: text(header, "pid")?
                .parse()
                .map_err(|_| "the owner pid is not a number".to_owned())?,
            status: TransactionStatus::parse(&text(header, "status")?)
                .ok_or_else(|| "unknown transaction status".to_owned())?,
            keep_mounts: boolean(&text(header, "keep_mounts")?)?,
            project_root: path(paths, "project_root")?,
            launch_cwd: path(paths, "launch_cwd")?,
            discovery_entry: path(paths, "discovery_entry")?,
            backing_store: path(paths, "backing_store")?,
            sources: Vec::new(),
            locks: Vec::new(),
            actions: Vec::new(),
            errors: Vec::new(),
        };

        for line in lines {
            match line.record.as_str() {
                "transaction" | "paths" => {}
                "source" => journal.sources.push(SourceResolution {
                    mount_name: text(line, "name")?,
                    source_ordinal: text(line, "ordinal")?
                        .parse()
                        .map_err(|_| "a source ordinal is not a number".to_owned())?,
                    source_entry: path(line, "entry")?,
                    source_canonical: path(line, "canonical")?,
                }),
                "lock" => journal.locks.push(JournalLock {
                    kind: LockResourceKind::parse(&text(line, "kind")?)
                        .ok_or_else(|| "unknown lock resource kind".to_owned())?,
                    path: path(line, "path")?,
                    anchor: path(line, "anchor")?,
                    suffix: path(line, "suffix")?,
                    physical: optional_text(line, "physical")?
                        .map(|value| PlatformIdentity::from_recorded(&value)),
                }),
                "action" => journal.actions.push(JournalAction {
                    id: text(line, "id")?
                        .parse()
                        .map_err(|_| "an action id is not a number".to_owned())?,
                    operation: ActionOperation::parse(&text(line, "op")?)
                        .ok_or_else(|| "unknown action operation".to_owned())?,
                    expected_precondition: PathPrecondition::parse(&text(line, "precondition")?)
                        .ok_or_else(|| "unknown action precondition".to_owned())?,
                    kind: RecordedKind::parse(&text(line, "kind")?)
                        .ok_or_else(|| "unknown action entry kind".to_owned())?,
                    status: ActionStatus::parse(&text(line, "status")?)
                        .ok_or_else(|| "unknown action status".to_owned())?,
                    final_path: path(line, "final")?,
                    temporary_path: optional_path(line, "temp")?,
                    source_canonical: optional_path(line, "source")?,
                    link_target: optional_path(line, "target")?,
                    identity: optional_text(line, "identity")?
                        .map(|value| PlatformIdentity::from_recorded(&value)),
                }),
                "error" => journal.errors.push(text(line, "text")?),
                other => return Err(format!("unknown journal record {other:?}")),
            }
        }

        journal.validate()?;
        Ok(journal)
    }

    /// Rejects a structurally decodable journal whose contents cannot be acted on safely.
    fn validate(&self) -> Result<(), String> {
        if self.locks.is_empty() {
            return Err("a mutating transaction must record at least one resource lock".to_owned());
        }

        let mut previous = 0;
        for action in &self.actions {
            if action.id <= previous {
                return Err("action ids must be unique and ascending".to_owned());
            }
            previous = action.id;

            let valid_kind = match action.operation {
                ActionOperation::CreateDirectory => action.kind == RecordedKind::Directory,
                ActionOperation::CreateDirectoryLink => matches!(
                    action.kind,
                    RecordedKind::Undecided | RecordedKind::Symlink | RecordedKind::Junction
                ),
                ActionOperation::ReuseExistingLink => action.kind == RecordedKind::Undecided,
            };
            if !valid_kind {
                return Err(format!(
                    "a {} action cannot record the {} entry kind",
                    action.operation.label(),
                    action.kind.label()
                ));
            }

            if action.operation == ActionOperation::ReuseExistingLink
                && action.status != ActionStatus::Reused
            {
                return Err("a reuse action may only carry the reused status".to_owned());
            }
            if action.operation != ActionOperation::ReuseExistingLink
                && action.status == ActionStatus::Reused
            {
                return Err("only a reuse action may carry the reused status".to_owned());
            }
            if action.operation == ActionOperation::CreateDirectoryLink
                && action.source_canonical.is_none()
            {
                return Err("a link action must record its canonical source".to_owned());
            }
        }
        Ok(())
    }
}

fn require_one<'a>(lines: &'a [Line], record: &str) -> Result<&'a Line, String> {
    let mut found = lines.iter().filter(|line| line.record == record);
    let first = found
        .next()
        .ok_or_else(|| format!("the journal has no {record} record"))?;
    if found.next().is_some() {
        return Err(format!("the journal has more than one {record} record"));
    }
    Ok(first)
}

fn text(line: &Line, key: &str) -> Result<String, String> {
    optional_text(line, key)?.ok_or_else(|| format!("{} record has no {key}", line.record))
}

fn optional_text(line: &Line, key: &str) -> Result<Option<String>, String> {
    line.field(key).map_or(Ok(None), |token| {
        codec::decode_text(token)
            .map(Some)
            .ok_or_else(|| format!("{} record has an undecodable {key}", line.record))
    })
}

fn path(line: &Line, key: &str) -> Result<PathBuf, String> {
    optional_path(line, key)?.ok_or_else(|| format!("{} record has no {key}", line.record))
}

fn optional_path(line: &Line, key: &str) -> Result<Option<PathBuf>, String> {
    line.field(key).map_or(Ok(None), |token| {
        codec::decode_path(token)
            .map(Some)
            .ok_or_else(|| format!("{} record has an undecodable {key}", line.record))
    })
}

fn boolean(value: &str) -> Result<bool, String> {
    match value {
        "true" => Ok(true),
        "false" => Ok(false),
        other => Err(format!("{other:?} is not a boolean")),
    }
}
