//! Durable, atomic journal persistence.
//!
//! Every guarantee the transaction layer makes rests on one property of this module: after
//! [`persist`] returns, the journal on disk describes state that has not happened yet, and it
//! survives losing power on the next instruction. That is why the write is not a `write_all` to the
//! final path — a partially written journal is worse than none, because recovery would act on the
//! half of the plan it can read.
//!
//! The sequence is the standard durable-replace one: write a unique temporary file, flush its
//! contents, rename it over the final name, then flush the containing directory so the rename
//! itself is durable. Unix flushes the directory and propagates a failure. Windows performs the
//! replacement through `MoveFileExW(MOVEFILE_REPLACE_EXISTING | MOVEFILE_WRITE_THROUGH)`, whose
//! documented success boundary is the move actually reaching disk. A checksum detects corruption;
//! it is never treated as a substitute for durable directory-entry replacement.

use std::fs::{self, File};
use std::io::{Read, Write};
use std::path::{Path, PathBuf};

use crate::error::{AppError, JournalError};
use crate::state;

use super::codec::{self, DecodeError, MAX_JOURNAL_BYTES};
use super::{JOURNAL_EXTENSION, TransactionId, TransactionJournal};

/// Persistence boundaries exposed only to fault-injection unit tests.
#[cfg(test)]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum PersistFault {
    /// The temporary file contents are durable but replacement has not started.
    AfterFileSync,
    /// Replacement succeeded but Unix has not synced the directory yet.
    AfterReplacement,
    /// The directory sync itself fails.
    DirectorySync,
    /// The platform durability boundary completed before the caller continues.
    AfterDurability,
}

#[cfg(test)]
thread_local! {
    static PERSIST_FAULT: std::cell::Cell<Option<PersistFault>> = const { std::cell::Cell::new(None) };
}

/// Runs one unit of work with a persistence fault injected on the current test thread.
#[cfg(test)]
pub(crate) fn with_persist_fault<T>(fault: PersistFault, work: impl FnOnce() -> T) -> T {
    PERSIST_FAULT.with(|selected| {
        let previous = selected.replace(Some(fault));
        let result = work();
        selected.set(previous);
        result
    })
}

/// A journal file that exists but cannot be acted on.
///
/// The path is retained deliberately. Recovery refuses to guess what an unreadable journal owned,
/// so the only safe outcome is to stop and name the file an operator has to look at; deleting it
/// would discard the record of entries that are still on disk.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RejectedJournal {
    /// File that could not be read.
    pub path: PathBuf,
    /// Why it was refused, phrased for an operator.
    pub reason: String,
}

/// One journal together with the file it was read from.
///
/// The pair is kept rather than the journal alone, because the filename and the recorded
/// transaction id are two sources of truth for one fact. Re-deriving the path from the id would
/// make a journal whose name does not match — a manual copy, a partially restored backup — be
/// reconciled against a *different* file: the derived name would be written and removed on every
/// run while the original stayed incomplete forever.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ScannedJournal {
    /// File the journal was read from, which is the file reconciling it must write and remove.
    pub path: PathBuf,
    /// The decoded journal.
    pub journal: TransactionJournal,
}

/// Everything found in the journal directory during one scan.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct JournalScan {
    /// Journals that decoded and validated, each with the file it came from.
    pub journals: Vec<ScannedJournal>,
    /// Journals that exist but were refused.
    pub rejected: Vec<RejectedJournal>,
}

impl JournalScan {
    /// Returns the non-terminal journals a later invocation must recover, quarantine, or report.
    pub fn incomplete(&self) -> impl Iterator<Item = &ScannedJournal> {
        self.journals
            .iter()
            .filter(|scanned| scanned.journal.status.is_incomplete())
    }
}

/// Writes `journal` durably to `path`, replacing any earlier version of the same transaction.
///
/// The destination is supplied rather than derived from the journal's own id. A transaction that
/// was read from disk must keep writing to the file it was read from, so that one journal is never
/// described by two files.
///
/// # Errors
///
/// Returns [`AppError::Journal`] when the state directory, the temporary file, the flush, or the
/// rename fails. A failure here is reported before the mutation it was about to describe, so the
/// caller has not yet changed anything.
pub fn persist(journal: &TransactionJournal, path: &Path) -> Result<(), AppError> {
    let directory = path.parent().unwrap_or_else(|| Path::new("."));
    state::ensure_private_directory(directory)?;
    let document = codec::render_document(&journal.to_lines());

    // The temporary name carries the transaction id, so two concurrent transactions never collide
    // and a crashed one leaves a file whose owner is identifiable.
    let temporary = directory.join(format!(
        "{}.{JOURNAL_EXTENSION}.tmp-{:08x}",
        journal.transaction_id,
        std::process::id()
    ));

    write_durably(&temporary, &document).inspect_err(|_| {
        let _ = fs::remove_file(&temporary);
    })?;
    #[cfg(test)]
    if let Err(error) = injected_failure(PersistFault::AfterFileSync, path) {
        let _ = fs::remove_file(&temporary);
        return Err(error);
    }
    state::restrict_to_owner(&temporary)?;

    // A journal is replaced in place as its status advances, so this rename must replace. That is
    // the opposite of link placement, which must never replace: there the destination belongs to
    // the project, while here it belongs to this transaction and no other writer exists.
    replace_journal(&temporary, path).map_err(|error| {
        let _ = fs::remove_file(&temporary);
        AppError::Journal(JournalError::Write {
            path: path.to_path_buf(),
            reason: error.to_string(),
        })
    })?;
    #[cfg(test)]
    injected_failure(PersistFault::AfterReplacement, path)?;
    flush_directory(directory)?;
    #[cfg(test)]
    injected_failure(PersistFault::AfterDurability, path)?;
    Ok(())
}

/// Reads and validates one journal file.
///
/// # Errors
///
/// Returns [`AppError::Journal`] when the file cannot be read, is not a journal, carries a schema
/// version this build does not write, or fails validation.
pub fn load(path: &Path) -> Result<TransactionJournal, AppError> {
    let document = read_bounded(path)?;
    decode(path, &document)
}

/// Removes a journal whose transaction has reached a terminal state.
///
/// # Errors
///
/// Returns [`AppError::Journal`] when the file exists and cannot be removed. A journal that is
/// already gone is success: a concurrent recovery that finished the same transaction is not a
/// failure of this one.
pub fn remove(path: &Path) -> Result<(), AppError> {
    remove_durably(path)
}

#[cfg(unix)]
fn remove_durably(path: &Path) -> Result<(), AppError> {
    match fs::remove_file(path) {
        Ok(()) => {}
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(()),
        Err(error) => {
            return Err(AppError::Journal(JournalError::Write {
                path: path.to_path_buf(),
                reason: error.to_string(),
            }));
        }
    }
    if let Some(parent) = path.parent() {
        flush_directory(parent)?;
    }
    Ok(())
}

/// Removes a Windows journal from the scanner's namespace with a write-through rename.
///
/// Win32 exposes no non-privileged directory fsync and a plain `DeleteFileW` success does not carry
/// the `MOVEFILE_WRITE_THROUGH` contract. Renaming the terminal journal to a non-`.journal`
/// tombstone establishes the logical removal durably; deleting that tombstone is best effort. A
/// crash can therefore leave inert evidence, never resurrect an incomplete journal as current.
#[cfg(windows)]
fn remove_durably(path: &Path) -> Result<(), AppError> {
    let parent = path.parent().unwrap_or_else(|| Path::new("."));
    let file_name = path
        .file_name()
        .unwrap_or_else(|| std::ffi::OsStr::new("transaction.journal"));
    let mut retired_name = file_name.to_os_string();
    retired_name.push(format!(".removed-{:08x}", std::process::id()));
    let retired = parent.join(retired_name);
    match crate::link::replace_file_write_through(path, &retired) {
        Ok(()) => {
            let _ = fs::remove_file(retired);
            Ok(())
        }
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(error) => Err(AppError::Journal(JournalError::Write {
            path: path.to_path_buf(),
            reason: error.to_string(),
        })),
    }
}

/// Returns the path a transaction's journal occupies.
///
/// # Errors
///
/// Returns [`AppError::MissingInput`] when the platform state location cannot be resolved.
pub fn journal_path(transaction_id: &TransactionId) -> Result<PathBuf, AppError> {
    Ok(state::transaction_base()?.join(format!("{transaction_id}.{JOURNAL_EXTENSION}")))
}

/// Reads every journal in the state directory.
///
/// A missing directory scans as empty, because a host that has never run a mutating session has
/// nothing to recover. Unreadable and undecodable files are collected rather than returned as an
/// error, so one corrupt journal cannot hide the healthy ones beside it.
///
/// # Errors
///
/// Returns [`AppError::Journal`] when the directory itself exists but cannot be enumerated.
pub fn scan() -> Result<JournalScan, AppError> {
    let directory = state::transaction_base()?;
    let entries = match fs::read_dir(&directory) {
        Ok(entries) => entries,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            return Ok(JournalScan::default());
        }
        Err(error) => {
            return Err(AppError::Journal(JournalError::Unreadable {
                path: directory,
                reason: error.to_string(),
            }));
        }
    };

    let mut scan = JournalScan::default();
    let mut paths = Vec::new();
    for entry in entries {
        let entry = entry.map_err(|error| {
            AppError::Journal(JournalError::Unreadable {
                path: directory.clone(),
                reason: error.to_string(),
            })
        })?;
        let path = entry.path();
        if path
            .extension()
            .is_some_and(|value| value == JOURNAL_EXTENSION)
        {
            paths.push(path);
        }
    }
    // Enumeration order is host-defined, so recovery would otherwise reconcile transactions in a
    // different order on two machines with the same state.
    paths.sort();

    for path in paths {
        match load(&path) {
            Ok(journal) => scan.journals.push(ScannedJournal { path, journal }),
            Err(AppError::Journal(error)) => scan.rejected.push(RejectedJournal {
                path,
                reason: error.reason().clone(),
            }),
            Err(other) => return Err(other),
        }
    }
    Ok(scan)
}

fn decode(path: &Path, document: &[u8]) -> Result<TransactionJournal, AppError> {
    let lines = codec::parse_document(document).map_err(|error| {
        AppError::Journal(match error {
            DecodeError::UnsupportedVersion(found) => JournalError::UnsupportedVersion {
                path: path.to_path_buf(),
                found,
                supported: super::SCHEMA_VERSION,
            },
            other => JournalError::Corrupt {
                path: path.to_path_buf(),
                reason: other.to_string(),
            },
        })
    })?;
    TransactionJournal::from_lines(&lines).map_err(|reason| {
        AppError::Journal(JournalError::Corrupt {
            path: path.to_path_buf(),
            reason,
        })
    })
}

fn read_bounded(path: &Path) -> Result<Vec<u8>, AppError> {
    let unreadable = |error: &std::io::Error| {
        AppError::Journal(JournalError::Unreadable {
            path: path.to_path_buf(),
            reason: error.to_string(),
        })
    };
    let mut file = File::open(path).map_err(|error| unreadable(&error))?;
    let length = file.metadata().map_err(|error| unreadable(&error))?.len();
    if length > MAX_JOURNAL_BYTES {
        return Err(AppError::Journal(JournalError::Corrupt {
            path: path.to_path_buf(),
            reason: format!("the file is larger than the {MAX_JOURNAL_BYTES}-byte journal limit"),
        }));
    }
    let mut document = Vec::with_capacity(usize::try_from(length).unwrap_or(0));
    file.read_to_end(&mut document)
        .map_err(|error| unreadable(&error))?;
    Ok(document)
}

fn write_durably(path: &Path, document: &[u8]) -> Result<(), AppError> {
    let write_error = |error: &std::io::Error| {
        AppError::Journal(JournalError::Write {
            path: path.to_path_buf(),
            reason: error.to_string(),
        })
    };
    let mut file = File::create(path).map_err(|error| write_error(&error))?;
    file.write_all(document)
        .map_err(|error| write_error(&error))?;
    // `sync_all` rather than `flush`: flushing only moves bytes out of the process buffer, which
    // does not survive losing power, and the whole point of this file is that it does.
    file.sync_all().map_err(|error| write_error(&error))
}

/// Replaces the journal on Unix; the directory sync below is the durability boundary.
#[cfg(unix)]
fn replace_journal(from: &Path, to: &Path) -> std::io::Result<()> {
    fs::rename(from, to)
}

/// Replaces the journal and waits for the namespace update to reach disk on Windows.
#[cfg(windows)]
fn replace_journal(from: &Path, to: &Path) -> std::io::Result<()> {
    crate::link::replace_file_write_through(from, to)
}

/// Flushes a directory entry so a rename or removal survives power loss.
#[cfg(unix)]
fn flush_directory(directory: &Path) -> Result<(), AppError> {
    #[cfg(test)]
    injected_failure(PersistFault::DirectorySync, directory)?;
    let handle = File::open(directory).map_err(|error| {
        AppError::Journal(JournalError::Write {
            path: directory.to_path_buf(),
            reason: format!("cannot open journal directory for durability sync: {error}"),
        })
    })?;
    handle.sync_all().map_err(|error| {
        AppError::Journal(JournalError::Write {
            path: directory.to_path_buf(),
            reason: format!("cannot make the journal directory entry durable: {error}"),
        })
    })
}

/// The Windows replacement call itself is write-through, so no second directory operation exists.
#[cfg(windows)]
fn flush_directory(_directory: &Path) -> Result<(), AppError> {
    #[cfg(test)]
    injected_failure(PersistFault::DirectorySync, _directory)?;
    Ok(())
}

#[cfg(test)]
fn injected_failure(phase: PersistFault, path: &Path) -> Result<(), AppError> {
    let selected = PERSIST_FAULT.with(std::cell::Cell::get);
    if selected != Some(phase) {
        return Ok(());
    }
    Err(AppError::Journal(JournalError::Write {
        path: path.to_path_buf(),
        reason: format!("injected persistence failure at {phase:?}"),
    }))
}
