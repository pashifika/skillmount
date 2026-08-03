//! Journal codec, validation, and persistence tests.

use std::fmt::Write as _;
use std::path::{Path, PathBuf};

use super::codec::{self, Line};
use super::store::{self, JournalScan, PersistFault};
use super::{
    ActionOperation, ActionStatus, JournalAction, JournalLock, RecordedKind, SourceResolution,
    TransactionId, TransactionJournal, TransactionStatus,
};
use crate::domain::AgentId;
use crate::error::{AppError, ExitCategory, JournalError};
use crate::link::PlatformIdentity;
use crate::lock::LockResourceKind;
use crate::mount::PathPrecondition;
use crate::state::testing::StateRootGuard;
use crate::test_support::TestDir;
use sha2::Digest as _;

/// Builds a journal that exercises every record type, including optional fields.
fn sample(status: TransactionStatus) -> TransactionJournal {
    TransactionJournal {
        transaction_id: TransactionId::parse("00ff-0001-0002").expect("a legal identifier"),
        agent: AgentId::Codex,
        owner_pid: 4321,
        status,
        project_root: PathBuf::from("/projects/app"),
        launch_cwd: PathBuf::from("/projects/app/crates"),
        discovery_entry: PathBuf::from("/projects/app/.agents/skills"),
        backing_store: PathBuf::from("/projects/app/.codex/skills"),
        keep_mounts: false,
        sources: vec![SourceResolution {
            mount_name: "alpha".to_owned(),
            source_ordinal: 0,
            source_entry: PathBuf::from("/skills/alpha"),
            source_canonical: PathBuf::from("/skills/alpha"),
        }],
        locks: vec![JournalLock {
            kind: LockResourceKind::BackingStore,
            path: PathBuf::from("/projects/app/.codex/skills"),
            anchor: PathBuf::from("/projects/app"),
            suffix: PathBuf::from(".codex/skills"),
            physical: Some(PlatformIdentity::from_recorded("unix:1:00000000000000ff")),
        }],
        actions: vec![
            JournalAction {
                id: 1,
                operation: ActionOperation::CreateDirectory,
                expected_precondition: PathPrecondition::Missing,
                temporary_path: Some(PathBuf::from("/projects/app/.codex/.skillmount-tmp")),
                final_path: PathBuf::from("/projects/app/.codex/skills"),
                source_canonical: None,
                link_target: None,
                kind: RecordedKind::Directory,
                status: ActionStatus::Applied,
                identity: Some(PlatformIdentity::from_recorded("unix:1:0000000000000001")),
            },
            JournalAction {
                id: 2,
                operation: ActionOperation::CreateDirectoryLink,
                expected_precondition: PathPrecondition::Missing,
                temporary_path: Some(PathBuf::from(
                    "/projects/app/.codex/skills/.skillmount-2.tmp",
                )),
                final_path: PathBuf::from("/projects/app/.codex/skills/alpha"),
                source_canonical: Some(PathBuf::from("/skills/alpha")),
                link_target: Some(PathBuf::from("/skills/alpha")),
                kind: RecordedKind::Symlink,
                status: ActionStatus::Staged,
                identity: None,
            },
            JournalAction {
                id: 3,
                operation: ActionOperation::ReuseExistingLink,
                expected_precondition: PathPrecondition::ExistingLinkToSource,
                temporary_path: None,
                final_path: PathBuf::from("/projects/app/.codex/skills/beta"),
                source_canonical: Some(PathBuf::from("/skills/beta")),
                link_target: None,
                kind: RecordedKind::Undecided,
                status: ActionStatus::Reused,
                identity: None,
            },
        ],
        errors: vec!["the third action hit a new conflict".to_owned()],
    }
}

fn round_trip(journal: &TransactionJournal) -> Result<TransactionJournal, String> {
    let document = codec::render_document(&journal.to_lines());
    let lines = codec::parse_document(&document).map_err(|error| error.to_string())?;
    TransactionJournal::from_lines(&lines)
}

#[test]
fn every_record_and_optional_field_survives_a_round_trip() {
    let journal = sample(TransactionStatus::Applying);

    let decoded = round_trip(&journal).expect("a journal this build wrote must decode");

    assert_eq!(decoded, journal);
    assert!(
        decoded.actions[1].identity.is_none(),
        "an omitted identity must not decode as an empty one, because a recorded-but-empty \
         identity would be compared and a missing one refuses removal"
    );
}

#[test]
fn a_truncated_journal_is_refused_rather_than_partially_read() {
    let journal = sample(TransactionStatus::Active);
    let document = codec::render_document(&journal.to_lines());

    for keep in [1, document.len() / 2, document.len() - 1] {
        let error = codec::parse_document(&document[..keep])
            .expect_err("a truncated journal must never decode");
        assert!(
            matches!(
                error,
                codec::DecodeError::ChecksumMismatch | codec::DecodeError::Malformed(_)
            ),
            "unexpected decode error for a {keep}-byte prefix: {error}"
        );
    }
}

#[test]
fn a_flipped_body_byte_fails_the_checksum() {
    let journal = sample(TransactionStatus::Active);
    let mut document = codec::render_document(&journal.to_lines());
    let last = document.len() - 2;
    document[last] ^= 0x20;

    assert_eq!(
        codec::parse_document(&document).expect_err("a corrupt body must be refused"),
        codec::DecodeError::ChecksumMismatch
    );
}

#[test]
fn an_unknown_schema_version_is_reported_before_anything_else() {
    let journal = sample(TransactionStatus::Planned);
    let document = codec::render_document(&journal.to_lines());
    let text = String::from_utf8(document).expect("the rendered journal is ASCII");
    let future = text.replacen(
        &format!("skillmount-journal {}", super::SCHEMA_VERSION),
        &format!("skillmount-journal {}", super::SCHEMA_VERSION + 1),
        1,
    );

    let error = codec::parse_document(future.as_bytes())
        .expect_err("a future schema must never be interpreted");

    assert_eq!(
        error,
        codec::DecodeError::UnsupportedVersion((super::SCHEMA_VERSION + 1).to_string()),
        "the version must be reported even though changing it also broke the checksum"
    );
}

#[test]
fn a_journal_from_the_other_platform_is_refused() {
    let journal = sample(TransactionStatus::Planned);
    let lines = journal.to_lines();
    let mut body = String::new();
    for line in &lines {
        writeln!(body, "{}", line.render()).expect("writing to a string cannot fail");
    }
    // The checksum is recomputed so the platform tag is what fails, not the digest.
    let mut checksum = String::new();
    for byte in sha2::Sha256::digest(body.as_bytes()) {
        write!(checksum, "{byte:02x}").expect("writing to a string cannot fail");
    }
    let foreign = if cfg!(windows) { "unix" } else { "windows" };
    let document = format!(
        "skillmount-journal {} {foreign} {checksum}\n{body}",
        super::SCHEMA_VERSION
    );

    assert_eq!(
        codec::parse_document(document.as_bytes()).expect_err("a foreign encoding is unreadable"),
        codec::DecodeError::ForeignPlatform(foreign.to_owned())
    );
}

#[test]
fn a_non_unicode_path_round_trips_byte_for_byte() {
    let mut journal = sample(TransactionStatus::Planned);
    let awkward = non_unicode_path();
    journal.project_root = awkward.clone();
    journal.actions[1].final_path = awkward.join("alpha");

    let decoded = round_trip(&journal).expect("a native path is always representable");

    assert_eq!(decoded.project_root, awkward);
    assert_eq!(decoded.actions[1].final_path, awkward.join("alpha"));
}

#[test]
fn a_path_holding_a_field_separator_round_trips() {
    let mut journal = sample(TransactionStatus::Planned);
    // A space and an equals sign are the two characters the line format itself uses.
    journal.project_root = PathBuf::from("/projects/my app=v2");

    let decoded = round_trip(&journal).expect("separators must be escaped, not rejected");

    assert_eq!(decoded.project_root, PathBuf::from("/projects/my app=v2"));
}

#[test]
fn validation_rejects_states_no_apply_sequence_can_produce() {
    let mut unlocked = sample(TransactionStatus::Planned);
    unlocked.locks.clear();
    assert!(
        round_trip(&unlocked)
            .expect_err("a mutating transaction always records its complete lock set")
            .contains("at least one resource lock")
    );

    let mut duplicate_ids = sample(TransactionStatus::Planned);
    duplicate_ids.actions[1].id = 1;
    assert!(
        round_trip(&duplicate_ids)
            .expect_err("duplicate ids are ambiguous")
            .contains("ascending")
    );

    let mut owned_reuse = sample(TransactionStatus::Planned);
    owned_reuse.actions[2].status = ActionStatus::Applied;
    assert!(
        round_trip(&owned_reuse)
            .expect_err("a reuse action must never look transaction-owned")
            .contains("reuse action")
    );

    let mut sourceless_link = sample(TransactionStatus::Planned);
    sourceless_link.actions[1].source_canonical = None;
    assert!(
        round_trip(&sourceless_link)
            .expect_err("a link with no source can never be verified")
            .contains("canonical source")
    );
}

#[test]
fn an_unknown_record_name_is_refused_rather_than_skipped() {
    let mut lines = sample(TransactionStatus::Planned).to_lines();
    let mut unknown = Line::new("future");
    unknown.push("data", codec::encode_text("value"));
    lines.push(unknown);

    let error = TransactionJournal::from_lines(&lines)
        .expect_err("an unrecognised record may carry ownership evidence this build would ignore");

    assert!(error.contains("unknown journal record"));
}

#[test]
fn persistence_is_atomic_and_leaves_no_temporary_file() {
    let fixture = TestDir::new("journal-persist");
    let _guard = StateRootGuard::set(fixture.path());
    let journal = sample(TransactionStatus::Planned);

    let path = store::journal_path(&journal.transaction_id).unwrap();
    store::persist(&journal, &path).expect("the state directory is writable");

    assert_eq!(store::load(&path).expect("the journal reloads"), journal);
    let leftovers = std::fs::read_dir(path.parent().unwrap())
        .unwrap()
        .filter_map(Result::ok)
        .filter(|entry| entry.file_name().to_string_lossy().contains(".tmp-"))
        .count();
    assert_eq!(leftovers, 0, "a durable write must not leave its temporary");
}

#[test]
fn every_persistence_boundary_fails_closed_under_injection() {
    for (label, fault, destination_exists) in [
        ("after-file-sync", PersistFault::AfterFileSync, false),
        ("after-replacement", PersistFault::AfterReplacement, true),
        ("directory-sync", PersistFault::DirectorySync, true),
        ("after-durability", PersistFault::AfterDurability, true),
    ] {
        let fixture = TestDir::new(&format!("journal-fault-{label}"));
        let _guard = StateRootGuard::set(fixture.path());
        let journal = sample(TransactionStatus::Planned);
        let path = store::journal_path(&journal.transaction_id).unwrap();

        let error = store::with_persist_fault(fault, || store::persist(&journal, &path))
            .expect_err("an injected boundary must never return success");

        assert_eq!(error.category(), ExitCategory::Filesystem);
        assert_eq!(path.exists(), destination_exists, "fault at {label}");
        if destination_exists {
            assert_eq!(
                store::load(&path).expect("a completed replacement stays readable"),
                journal,
                "fault at {label} must never expose a partial journal"
            );
        }
        let leftovers = std::fs::read_dir(path.parent().unwrap())
            .unwrap()
            .filter_map(Result::ok)
            .filter(|entry| entry.file_name().to_string_lossy().contains(".tmp-"))
            .count();
        assert_eq!(leftovers, 0, "fault at {label} leaked a temporary file");
    }
}

#[test]
fn a_status_advance_replaces_the_same_file() {
    let fixture = TestDir::new("journal-advance");
    let _guard = StateRootGuard::set(fixture.path());
    let mut journal = sample(TransactionStatus::Planned);

    let path = store::journal_path(&journal.transaction_id).unwrap();
    store::persist(&journal, &path).expect("the state directory is writable");
    journal.status = TransactionStatus::Active;
    store::persist(&journal, &path).expect("advancing a status rewrites the journal");

    assert_eq!(
        store::load(&path).unwrap().status,
        TransactionStatus::Active
    );
    assert_eq!(store::scan().unwrap().journals.len(), 1);
}

#[test]
fn a_scanned_journal_carries_the_file_it_came_from() {
    let fixture = TestDir::new("journal-scan-path");
    let _guard = StateRootGuard::set(fixture.path());
    let journal = sample(TransactionStatus::Active);
    // A journal whose filename does not match its recorded id, which a manual copy or a partially
    // restored backup produces. Reconciling it against a path re-derived from the id would write
    // and remove a second file on every run while this one stayed incomplete forever.
    let directory = crate::state::transaction_base().unwrap();
    crate::state::ensure_private_directory(&directory).unwrap();
    let misnamed = directory.join("renamed-by-hand.journal");
    store::persist(&journal, &misnamed).expect("the state directory is writable");

    let scan = store::scan().expect("the directory is readable");

    assert_eq!(scan.journals.len(), 1);
    assert_eq!(
        scan.journals[0].path, misnamed,
        "reconciling must target the file that was read, not one derived from the recorded id"
    );
    assert_ne!(
        scan.journals[0].path,
        store::journal_path(&journal.transaction_id).unwrap()
    );
}

#[test]
fn a_scan_separates_healthy_journals_from_refused_ones() {
    let fixture = TestDir::new("journal-scan");
    let _guard = StateRootGuard::set(fixture.path());
    let healthy = sample(TransactionStatus::Active);
    let healthy_path = store::journal_path(&healthy.transaction_id).unwrap();
    store::persist(&healthy, &healthy_path).expect("the state directory is writable");

    let directory = crate::state::transaction_base().unwrap();
    std::fs::write(directory.join("garbage.journal"), b"not a journal at all\n")
        .expect("fixture write");
    std::fs::write(directory.join("ignored.txt"), b"not a journal file name")
        .expect("fixture write");

    let scan: JournalScan = store::scan().expect("one bad journal must not hide the good ones");

    assert_eq!(scan.journals.len(), 1);
    assert_eq!(scan.journals[0].journal, healthy);
    assert_eq!(scan.journals[0].path, healthy_path);
    assert_eq!(scan.rejected.len(), 1);
    assert_eq!(
        scan.rejected[0].path,
        directory.join("garbage.journal"),
        "a refused journal is named so an operator can act on it"
    );
    assert!(
        directory.join("garbage.journal").exists(),
        "a journal that cannot be read is never deleted"
    );
}

#[test]
fn an_unreadable_journal_is_a_temporary_failure_that_keeps_the_file() {
    let fixture = TestDir::new("journal-category");
    let _guard = StateRootGuard::set(fixture.path());
    let directory = crate::state::transaction_base().unwrap();
    crate::state::ensure_private_directory(&directory).unwrap();
    let path = directory.join("broken.journal");
    std::fs::write(&path, b"skillmount-journal 1 unix deadbeef\n").expect("fixture write");

    let error = store::load(&path).expect_err("a bad checksum must never decode");

    let AppError::Journal(journal_error) = &error else {
        panic!("expected a journal error, got {error:?}");
    };
    assert_eq!(journal_error.path(), &path);
    assert!(journal_error.blocks_recovery());
    assert_eq!(
        error.category(),
        ExitCategory::Temporary,
        "an unusable journal is a stale-transaction condition, not a destination failure"
    );
    assert!(path.exists());
}

#[test]
fn a_write_failure_reports_the_filesystem_category() {
    let error = JournalError::Write {
        path: PathBuf::from("/state/one.journal"),
        reason: "disk full".to_owned(),
    };

    assert!(!error.blocks_recovery());
    assert_eq!(
        AppError::Journal(error).category(),
        ExitCategory::Filesystem
    );
}

#[test]
fn incomplete_and_terminal_statuses_are_partitioned_exhaustively() {
    for status in [
        TransactionStatus::Planned,
        TransactionStatus::Applying,
        TransactionStatus::Active,
        TransactionStatus::Supervising,
        TransactionStatus::Cleaning,
        TransactionStatus::Failed,
    ] {
        assert!(
            status.is_incomplete(),
            "{} must remain non-terminal",
            status.label()
        );
    }
    assert!(TransactionStatus::Active.is_automatically_recoverable());
    assert!(!TransactionStatus::Supervising.is_automatically_recoverable());
    for status in [TransactionStatus::Completed, TransactionStatus::Kept] {
        assert!(
            status.is_terminal(),
            "{} must be left alone",
            status.label()
        );
        assert!(!status.is_incomplete());
    }
}

#[test]
fn recovery_walks_owned_actions_newest_first_and_skips_reuse() {
    let journal = sample(TransactionStatus::Applying);

    let ids = journal
        .reversible_actions()
        .map(|action| action.id)
        .collect::<Vec<_>>();

    assert_eq!(
        ids,
        [2, 1],
        "a helper directory must be undone after the links inside it, and a reused entry never"
    );
}

#[test]
fn a_staged_action_offers_both_paths_with_the_temporary_first() {
    let journal = sample(TransactionStatus::Applying);
    let staged = &journal.actions[1];

    assert_eq!(
        staged.candidate_paths(),
        [
            staged.temporary_path.clone().unwrap(),
            staged.final_path.clone()
        ],
        "placement may have happened without the applied record reaching disk"
    );
    assert_eq!(
        staged.current_path(),
        &staged.temporary_path.clone().unwrap()
    );
}

#[test]
fn a_transaction_id_never_becomes_a_path_traversal() {
    assert!(TransactionId::parse("../escape").is_none());
    assert!(TransactionId::parse("a/b").is_none());
    assert!(TransactionId::parse("").is_none());
    assert!(TransactionId::parse(&"a".repeat(129)).is_none());
    assert!(TransactionId::parse("00ab-12cd").is_some());

    let minted = TransactionId::mint();
    assert!(
        TransactionId::parse(minted.as_str()).is_some(),
        "a minted id must satisfy the grammar its own parser enforces"
    );
    assert_ne!(minted, TransactionId::mint());
}

#[test]
fn encoded_tokens_never_contain_a_field_separator() {
    for raw in [
        b"plain".to_vec(),
        b"has space".to_vec(),
        b"has=equals".to_vec(),
        b"has%percent".to_vec(),
        vec![0x00, 0x0a, 0xff],
        Vec::new(),
    ] {
        let token = codec::encode_bytes(&raw);
        assert!(!token.contains(' ') && !token.contains('=') && !token.contains('\n'));
        assert_eq!(codec::decode_bytes(&token).as_deref(), Some(raw.as_slice()));
    }
}

/// Returns a path the platform accepts but UTF-8 cannot represent.
#[cfg(unix)]
fn non_unicode_path() -> PathBuf {
    use std::ffi::OsString;
    use std::os::unix::ffi::OsStringExt;

    PathBuf::from(OsString::from_vec(vec![b'/', b's', 0xff, 0xfe, b'k']))
}

/// Returns a path the platform accepts but UTF-8 cannot represent.
#[cfg(windows)]
fn non_unicode_path() -> PathBuf {
    use std::ffi::OsString;
    use std::os::windows::ffi::OsStringExt;

    // An unpaired high surrogate is legal in a Windows filename and has no UTF-8 encoding.
    PathBuf::from(OsString::from_wide(&[
        u16::from(b'C'),
        u16::from(b':'),
        u16::from(b'\\'),
        0xd800,
        u16::from(b'k'),
    ]))
}

#[test]
fn a_journal_path_is_derived_from_the_transaction_id_alone() {
    let fixture = TestDir::new("journal-name");
    let _guard = StateRootGuard::set(fixture.path());
    let journal = sample(TransactionStatus::Planned);

    assert_eq!(journal.file_name(), "00ff-0001-0002.journal");
    assert_eq!(
        store::journal_path(&journal.transaction_id).unwrap(),
        Path::new(fixture.path())
            .join("transactions")
            .join("00ff-0001-0002.journal")
    );
}
