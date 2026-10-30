//! Shared evidence checks for session-owned regular configuration files.

use std::fs::File;
use std::io::{self, Read, Seek};

use super::{EntryKind, OwnedConfigFile, Ownership, OwnershipMismatch, PathEntry};

/// Checks entry kind and object identity before any content read or mutation.
pub(super) fn verify_config_ownership(live: &PathEntry, recorded: &OwnedConfigFile) -> Ownership {
    if live.kind == EntryKind::Missing {
        return Ownership::Absent;
    }
    if live.kind != EntryKind::File {
        return Ownership::Mismatch(OwnershipMismatch::NotAFile);
    }
    match (&live.identity, &recorded.identity) {
        (Some(actual), Some(expected)) if actual == expected => Ownership::Owned,
        (Some(_), Some(_)) => Ownership::Mismatch(OwnershipMismatch::IdentityChanged),
        _ => Ownership::Mismatch(OwnershipMismatch::IdentityUnavailable),
    }
}

/// Reads at most the expected byte count plus one, never an unbounded replaced file.
pub(super) fn config_file_matches(file: &mut File, contents: &str) -> io::Result<bool> {
    if contents.len() > super::MAX_CONFIG_BYTES {
        return Ok(false);
    }
    let metadata = file.metadata()?;
    if !metadata.is_file() || metadata.len() != contents.len() as u64 {
        return Ok(false);
    }
    file.rewind()?;
    let mut actual = Vec::with_capacity(contents.len());
    file.take(contents.len() as u64 + 1)
        .read_to_end(&mut actual)?;
    Ok(actual == contents.as_bytes())
}

#[cfg(test)]
mod tests {
    use crate::link::{
        MAX_CONFIG_BYTES, OwnershipMismatch, PlacementOutcome, RemoveOutcome, platform_backend,
    };
    use crate::test_support::TestDir;
    use std::fs;

    const CONTENTS: &str = "skills:\n  customDirectories:\n    - /external/skills\n";

    #[test]
    fn config_file_create_place_verify_and_remove() {
        let dir = TestDir::new("owned-config-lifecycle");
        let backend = platform_backend();
        let staged = backend
            .create_config_file(&dir.path().join("staged"), CONTENTS)
            .unwrap();
        assert!(backend.verify_config_file(&staged).unwrap());
        let final_path = dir.path().join("omp-config.yml");
        let PlacementOutcome::Placed(placed) = backend
            .place_config_file_no_replace(&staged, &final_path)
            .unwrap()
        else {
            panic!("file placed");
        };
        assert!(!staged.path.exists());
        assert_eq!(fs::read_to_string(&final_path).unwrap(), CONTENTS);
        assert!(backend.verify_config_file(&placed).unwrap());
        assert_eq!(
            backend.remove_config_file(&placed).unwrap(),
            RemoveOutcome::Removed
        );
        assert_eq!(
            backend.remove_config_file(&placed).unwrap(),
            RemoveOutcome::AlreadyAbsent
        );
    }

    #[test]
    fn config_file_never_overwrites_existing_entries() {
        let dir = TestDir::new("owned-config-existing");
        let backend = platform_backend();
        let destination = dir.path().join("existing");
        fs::write(&destination, "user data").unwrap();
        assert!(backend.create_config_file(&destination, CONTENTS).is_err());
        let staged = backend
            .create_config_file(&dir.path().join("staged"), CONTENTS)
            .unwrap();
        assert_eq!(
            backend
                .place_config_file_no_replace(&staged, &destination)
                .unwrap(),
            PlacementOutcome::DestinationExists
        );
        assert_eq!(fs::read_to_string(destination).unwrap(), "user data");
        assert_eq!(
            backend.remove_config_file(&staged).unwrap(),
            RemoveOutcome::Removed
        );
    }

    #[test]
    fn config_file_modified_contents_are_retained() {
        let dir = TestDir::new("owned-config-modified");
        let backend = platform_backend();
        let file = backend
            .create_config_file(&dir.path().join("config"), CONTENTS)
            .unwrap();
        fs::write(&file.path, "user changes").unwrap();
        assert!(!backend.verify_config_file(&file).unwrap());
        assert_eq!(
            backend.remove_config_file(&file).unwrap(),
            RemoveOutcome::OwnershipMismatch(OwnershipMismatch::ContentsChanged)
        );
        assert!(matches!(
            backend
                .place_config_file_no_replace(&file, &dir.path().join("final"))
                .unwrap(),
            PlacementOutcome::OwnershipMismatch(_)
        ));
        assert_eq!(fs::read_to_string(file.path).unwrap(), "user changes");
    }

    #[test]
    fn config_file_same_bytes_new_identity_are_retained() {
        let dir = TestDir::new("owned-config-replaced");
        let backend = platform_backend();
        let file = backend
            .create_config_file(&dir.path().join("config"), CONTENTS)
            .unwrap();
        let moved = dir.path().join("original");
        fs::rename(&file.path, &moved).unwrap();
        fs::write(&file.path, CONTENTS).unwrap();
        assert!(!backend.verify_config_file(&file).unwrap());
        assert_eq!(
            backend.remove_config_file(&file).unwrap(),
            RemoveOutcome::OwnershipMismatch(OwnershipMismatch::IdentityChanged)
        );
        assert_eq!(fs::read_to_string(&file.path).unwrap(), CONTENTS);
        assert_eq!(
            backend
                .remove_config_file(&file.relocated_to(&moved))
                .unwrap(),
            RemoveOutcome::Removed
        );
    }

    #[test]
    fn config_file_requires_identity_and_bounds_payload() {
        let dir = TestDir::new("owned-config-evidence");
        let backend = platform_backend();
        let mut file = backend
            .create_config_file(&dir.path().join("config"), CONTENTS)
            .unwrap();
        file.identity = None;
        assert_eq!(
            backend.remove_config_file(&file).unwrap(),
            RemoveOutcome::OwnershipMismatch(OwnershipMismatch::IdentityUnavailable)
        );
        let oversized = dir.path().join("oversized");
        assert!(
            backend
                .create_config_file(&oversized, &"x".repeat(MAX_CONFIG_BYTES + 1))
                .is_err()
        );
        assert!(!oversized.exists());
        assert_eq!(fs::read_to_string(file.path).unwrap(), CONTENTS);
    }

    #[test]
    fn config_file_symlink_replacement_never_reads_or_removes_target() {
        let dir = TestDir::new("owned-config-symlink");
        let backend = platform_backend();
        let file = backend
            .create_config_file(&dir.path().join("config"), CONTENTS)
            .unwrap();
        let target = dir.path().join("sentinel");
        fs::write(&target, "user data").unwrap();
        fs::remove_file(&file.path).unwrap();
        if !crate::test_support::symlink_file_or_skip(&target, &file.path) {
            return;
        }
        assert!(!backend.verify_config_file(&file).unwrap());
        assert_eq!(
            backend.remove_config_file(&file).unwrap(),
            RemoveOutcome::OwnershipMismatch(OwnershipMismatch::NotAFile)
        );
        assert_eq!(fs::read_to_string(target).unwrap(), "user data");
        assert!(
            fs::symlink_metadata(file.path)
                .unwrap()
                .file_type()
                .is_symlink()
        );
    }
}
