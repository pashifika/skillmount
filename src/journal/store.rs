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
//! itself is durable. The last step is only available on Unix; Windows has no portable directory
//! flush, which the checksum in the header compensates for by making a torn journal detectable
//! rather than silently readable.

use std::fs::{self, File};
use std::io::{Read, Write};
use std::path::{Path, PathBuf};

use crate::error::{AppError, JournalError};
use crate::state;

use super::codec::{self, DecodeError, MAX_JOURNAL_BYTES};
use super::{JOURNAL_EXTENSION, TransactionId, TransactionJournal};

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

/// Everything found in the journal directory during one scan.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct JournalScan {
    /// Journals that decoded and validated.
    pub journals: Vec<TransactionJournal>,
    /// Journals that exist but were refused.
    pub rejected: Vec<RejectedJournal>,
}

impl JournalScan {
    /// Returns the journals a later invocation must reconcile.
    pub fn incomplete(&self) -> impl Iterator<Item = &TransactionJournal> {
        self.journals
            .iter()
            .filter(|journal| journal.status.is_incomplete())
    }
}

/// Writes `journal` durably, replacing any earlier version of the same transaction.
///
/// # Errors
///
/// Returns [`AppError::Journal`] when the state directory, the temporary file, the flush, or the
/// rename fails. A failure here is reported before the mutation it was about to describe, so the
/// caller has not yet changed anything.
pub fn persist(journal: &TransactionJournal) -> Result<PathBuf, AppError> {
    let directory = state::transaction_base()?;
    state::ensure_private_directory(&directory)?;
    let final_path = directory.join(journal.file_name());
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
    state::restrict_to_owner(&temporary)?;

    // A journal is replaced in place as its status advances, so this rename must replace. That is
    // the opposite of link placement, which must never replace: there the destination belongs to
    // the project, while here it belongs to this transaction and no other writer exists.
    fs::rename(&temporary, &final_path).map_err(|error| {
        let _ = fs::remove_file(&temporary);
        AppError::Journal(JournalError::Write {
            path: final_path.clone(),
            reason: error.to_string(),
        })
    })?;
    flush_directory(&directory);
    Ok(final_path)
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
        flush_directory(parent);
    }
    Ok(())
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
            Ok(journal) => scan.journals.push(journal),
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

/// Flushes a directory entry so a rename survives power loss.
///
/// Best effort by design. A host that refuses to open the directory has not made the rename any
/// less correct, only less durable, and failing the transaction over it would turn a weaker
/// guarantee into an outage.
#[cfg(unix)]
fn flush_directory(directory: &Path) {
    if let Ok(handle) = File::open(directory) {
        let _ = handle.sync_all();
    }
}

/// Windows has no portable directory flush.
///
/// Opening a directory handle needs `FILE_FLAG_BACKUP_SEMANTICS`, which the standard library does
/// not expose and which would push a fourth `unsafe` module past the ADR 0011 boundary for a
/// best-effort call. The header checksum covers the gap: a rename that did not reach the disk
/// leaves either the previous journal or a torn one, and a torn one is refused rather than acted
/// on.
#[cfg(windows)]
fn flush_directory(_directory: &Path) {}
