//! Named boundaries a test can stop the process at.
//!
//! Crash recovery is the one behaviour that cannot be proved by unit-testing serialized states. A
//! test that hand-writes a `staged` journal and then calls recovery proves that recovery reads
//! that journal — not that the apply sequence ever produces it, and not that the filesystem looks
//! the way the journal claims. The only convincing evidence is a real process that really stops
//! between two real operations, followed by a real second invocation against whatever it left
//! behind.
//!
//! Each mutation boundary therefore calls [`reached`] with a stable name. When the process was
//! started with [`STOP_AT`] naming that boundary, the process aborts immediately — no unwinding, no
//! destructors, no journal flush that a panic might still perform. That is what makes it a
//! faithful stand-in for a power loss or a `SIGKILL`.
//!
//! # Why this compiles out of a release build
//!
//! The whole mechanism is behind `debug_assertions`, so a release binary contains no environment
//! lookup, no abort call, and no reachable path that stops the process early. `cargo test` builds
//! in debug and the integration tests run the debug binary, so the coverage is real; a shipped
//! binary simply has nothing to trigger.

/// Environment variable naming the boundary at which the process must abort.
///
/// Read on every call rather than cached: integration tests set it per child process, and a cached
/// value would make the first checkpoint of the run decide the behaviour of all the others.
pub const STOP_AT: &str = "SKILLMOUNT_STOP_AT";

/// Environment variable naming a boundary the process should pause at instead of stopping.
///
/// Serialization is only observable while a session is *holding* its locks, and the locks are held
/// for as long as the process lives. A second invocation therefore has to overlap the first, which
/// needs the first to still be running — something an aborting checkpoint cannot arrange.
pub const HOLD_AT: &str = "SKILLMOUNT_HOLD_AT";

/// Environment variable giving the pause length in milliseconds.
pub const HOLD_MS: &str = "SKILLMOUNT_HOLD_MS";

/// A durable boundary the transaction layer passes through.
///
/// The names are part of the test contract, not internal detail: a test names the boundary it
/// wants to stop at, and renaming one here without renaming it there silently turns a crash test
/// into a no-crash test.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Checkpoint {
    /// Preliminary discovery is complete and no resource lock has been acquired yet.
    DiscoveryInspected,
    /// The complete plan is durable and nothing has been mutated.
    JournalPlanned,
    /// The transaction is durably marked as mutating.
    JournalApplying,
    /// One action's intent is durable; its temporary entry does not exist yet.
    ActionIntent,
    /// The temporary entry exists; its identity is not durable yet.
    TemporaryCreated,
    /// The temporary entry's identity is durable; it has not been placed.
    ActionStaged,
    /// The entry occupies its final path; the journal still says staged.
    FinalPlaced,
    /// The action is durably applied.
    ActionApplied,
    /// Every action is applied and the transaction is durably active.
    JournalActive,
    /// Child supervision intent is durable; process-domain death has not been proved.
    JournalSupervising,
    /// Cleanup has durably begun and nothing has been removed yet.
    JournalCleaning,
    /// One entry has been removed and the journal has not recorded it yet.
    EntryRemoved,
    /// A helper directory has been removed and the journal has not recorded it yet.
    DirectoryRemoved,
}

impl Checkpoint {
    /// Returns the name a test uses to select this boundary.
    #[must_use]
    pub const fn name(self) -> &'static str {
        match self {
            Self::DiscoveryInspected => "discovery-inspected",
            Self::JournalPlanned => "journal-planned",
            Self::JournalApplying => "journal-applying",
            Self::ActionIntent => "action-intent",
            Self::TemporaryCreated => "temporary-created",
            Self::ActionStaged => "action-staged",
            Self::FinalPlaced => "final-placed",
            Self::ActionApplied => "action-applied",
            Self::JournalActive => "journal-active",
            Self::JournalSupervising => "journal-supervising",
            Self::JournalCleaning => "journal-cleaning",
            Self::EntryRemoved => "entry-removed",
            Self::DirectoryRemoved => "directory-removed",
        }
    }

    /// Every boundary, so a test suite can assert it covers all of them.
    pub const ALL: [Self; 13] = [
        Self::DiscoveryInspected,
        Self::JournalPlanned,
        Self::JournalApplying,
        Self::ActionIntent,
        Self::TemporaryCreated,
        Self::ActionStaged,
        Self::FinalPlaced,
        Self::ActionApplied,
        Self::JournalActive,
        Self::JournalSupervising,
        Self::JournalCleaning,
        Self::EntryRemoved,
        Self::DirectoryRemoved,
    ];
}

/// Aborts the process when this boundary was selected for failure injection.
///
/// `sequence` counts occurrences of the same boundary within one run, starting at 1, so a test can
/// stop at the second link rather than the first. A selector without an occurrence stops at the
/// first.
#[cfg(debug_assertions)]
pub fn reached(checkpoint: Checkpoint, sequence: u32) {
    if selected(HOLD_AT, checkpoint, sequence) {
        let millis = std::env::var_os(HOLD_MS)
            .and_then(|value| value.to_str().and_then(|text| text.parse::<u64>().ok()))
            .unwrap_or(500);
        eprintln!(
            "skillmount: failure injection holding at {} occurrence {sequence} for {millis}ms",
            checkpoint.name()
        );
        std::thread::sleep(std::time::Duration::from_millis(millis));
    }
    if !selected(STOP_AT, checkpoint, sequence) {
        return;
    }

    // Written to the inherited standard error before aborting so a test can distinguish an injected
    // stop from a genuine crash.
    eprintln!(
        "skillmount: failure injection stopping at {} occurrence {sequence}",
        checkpoint.name()
    );
    // `abort` rather than `exit` or `panic`: both of the others run cleanup that a power loss would
    // not, and the point of the boundary is that nothing after it happens.
    std::process::abort();
}

/// Returns whether `variable` selects this boundary occurrence.
///
/// The selector is `<name>` or `<name>@<occurrence>`; without an occurrence the first one matches.
#[cfg(debug_assertions)]
fn selected(variable: &str, checkpoint: Checkpoint, sequence: u32) -> bool {
    let Some(selector) = std::env::var_os(variable) else {
        return false;
    };
    let Some(selector) = selector.to_str() else {
        return false;
    };
    let (name, wanted) = match selector.split_once('@') {
        Some((name, occurrence)) => (name, occurrence.parse::<u32>().ok()),
        None => (selector, None),
    };
    name == checkpoint.name() && wanted.is_none_or(|wanted| wanted == sequence)
}

/// Failure injection is compiled out of a release build.
#[cfg(not(debug_assertions))]
#[inline]
pub fn reached(_checkpoint: Checkpoint, _sequence: u32) {}
