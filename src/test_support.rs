//! Shared fixtures for unit tests.

use std::collections::BTreeMap;
use std::fs;
use std::io;
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

/// A temporary directory that removes itself when the test ends.
pub(crate) struct TestDir(pub(crate) PathBuf);

impl TestDir {
    pub(crate) fn new(label: &str) -> Self {
        let nonce = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .expect("clock should be after epoch")
            .as_nanos();
        let path =
            std::env::temp_dir().join(format!("skillmount-{label}-{}-{nonce}", std::process::id()));
        fs::create_dir_all(&path).expect("fixture should be created");
        Self(path)
    }

    pub(crate) fn path(&self) -> &Path {
        &self.0
    }

    /// Creates `relative` and every missing parent, returning the created path.
    pub(crate) fn dir(&self, relative: &str) -> PathBuf {
        let path = self.0.join(relative);
        fs::create_dir_all(&path).expect("fixture directory should be created");
        path
    }

    /// Creates a regular file with every missing parent.
    pub(crate) fn file(&self, relative: &str, contents: &str) -> PathBuf {
        let path = self.0.join(relative);
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent).expect("fixture parent should be created");
        }
        fs::write(&path, contents).expect("fixture file should be written");
        path
    }
}

impl Drop for TestDir {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.0);
    }
}

/// Creates a directory link, or returns the operating-system failure.
///
/// Windows requires either Developer Mode or an elevated process to create a symbolic link.
/// Callers use [`skip_unprivileged`] so a restricted host reports a skip rather than a spurious
/// failure.
pub(crate) fn try_symlink_dir(target: &Path, link: &Path) -> io::Result<()> {
    #[cfg(unix)]
    {
        std::os::unix::fs::symlink(target, link)
    }
    #[cfg(windows)]
    {
        std::os::windows::fs::symlink_dir(target, link)
    }
}

/// Environment variable that turns a skipped link fixture into a failure.
///
/// CI sets it so link coverage cannot silently disappear. A skipped test still reports success,
/// and its output is captured unless it fails, so without this guard a run where every link
/// fixture was skipped is indistinguishable from a run where all of them worked.
const REQUIRE_LINKS: &str = "SKILLMOUNT_REQUIRE_LINKS";

/// Creates a directory link and panics on any failure other than missing privilege.
///
/// Returns `false` when the host cannot create links, which lets a test return early. Windows
/// needs Developer Mode or an elevated process, so a contributor without either still gets a
/// usable suite locally while CI proves the link paths actually ran.
#[must_use]
pub(crate) fn symlink_dir_or_skip(target: &Path, link: &Path) -> bool {
    match try_symlink_dir(target, link) {
        Ok(()) => true,
        Err(error) if skip_unprivileged(&error) => {
            assert!(
                std::env::var_os(REQUIRE_LINKS).is_none(),
                "{REQUIRE_LINKS} is set, so a link fixture may not be skipped: creating {} failed: {error}",
                link.display()
            );
            eprintln!("skipping link fixture at {}: {error}", link.display());
            false
        }
        Err(error) => panic!("link fixture at {} failed: {error}", link.display()),
    }
}

/// Removes a directory link.
///
/// The call differs by platform and the wrong one fails rather than doing nothing: Windows treats
/// a directory symbolic link as a directory entry and rejects `remove_file` with "Access is
/// denied", while Unix treats every symbolic link as a file and rejects `remove_dir`.
pub(crate) fn remove_directory_link(path: &Path) {
    let result = if cfg!(windows) {
        fs::remove_dir(path)
    } else {
        fs::remove_file(path)
    };
    result.unwrap_or_else(|error| panic!("removing link {} failed: {error}", path.display()));
}

/// Returns whether the error means the host forbids link creation rather than the test being wrong.
fn skip_unprivileged(error: &io::Error) -> bool {
    matches!(
        error.kind(),
        io::ErrorKind::PermissionDenied | io::ErrorKind::Unsupported
    )
}

/// A recursive record of a directory tree, used to prove a read-only path changed nothing.
///
/// Entries are keyed by path relative to the root and recorded without following links, so a link
/// that is replaced by a directory with identical contents still shows up as a difference.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct TreeSnapshot(BTreeMap<PathBuf, String>);

impl TreeSnapshot {
    /// Records `root` and everything beneath it. A missing root records as empty.
    pub(crate) fn capture(root: &Path) -> Self {
        let mut entries = BTreeMap::new();
        collect(root, root, &mut entries);
        Self(entries)
    }
}

/// Runs `operation` and fails the test if it changed anything under `roots`.
///
/// This is the guard that makes "read-only" an asserted property rather than a review claim. Any
/// created directory, created or retargeted link, or changed file size shows up as a difference.
pub(crate) fn assert_no_side_effects<T>(roots: &[&Path], operation: impl FnOnce() -> T) -> T {
    let before = roots
        .iter()
        .map(|root| (root.to_path_buf(), TreeSnapshot::capture(root)))
        .collect::<Vec<_>>();

    let outcome = operation();

    for (root, snapshot) in before {
        assert_eq!(
            TreeSnapshot::capture(&root),
            snapshot,
            "a read-only operation modified {}",
            root.display()
        );
    }
    outcome
}

fn collect(root: &Path, current: &Path, entries: &mut BTreeMap<PathBuf, String>) {
    let Ok(metadata) = fs::symlink_metadata(current) else {
        return;
    };
    let file_type = metadata.file_type();
    let descriptor = if file_type.is_symlink() {
        let target = fs::read_link(current).map_or_else(
            |_| "unreadable".to_owned(),
            |target| target.display().to_string(),
        );
        format!("link -> {target}")
    } else if file_type.is_dir() {
        "dir".to_owned()
    } else {
        format!("file {}", metadata.len())
    };

    if let Ok(relative) = current.strip_prefix(root) {
        entries.insert(relative.to_path_buf(), descriptor);
    }

    if file_type.is_dir() && !file_type.is_symlink() {
        let Ok(children) = fs::read_dir(current) else {
            return;
        };
        for child in children.flatten() {
            collect(root, &child.path(), entries);
        }
    }
}
