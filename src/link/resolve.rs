//! Bounded directory-link chain resolution and platform-aware path comparison.
//!
//! Resolution is written against [`LinkBackend`] rather than `std::fs` for two reasons. The first
//! is that a Windows junction is not a symbolic link and only the backend can tell them apart. The
//! second is testability: an in-memory backend drives every branch of this walker on both
//! platforms, so a cycle, a broken hop, and an over-deep chain are all exercised on a host that
//! could not create such a layout for real.
//!
//! Comparison is deliberately a separate concept from display. The key is normalized and may
//! differ substantially from what the operator typed; the display value is never rewritten,
//! because a diagnostic that reports a path the operator cannot find in their own project is
//! worse than no diagnostic.

use std::collections::BTreeSet;
use std::path::{Path, PathBuf};

use crate::error::LinkError;
use crate::link::{EntryKind, LinkBackend, LinkTarget, PathEntry, PlatformIdentity};

/// Maximum number of directory-link hops this walker follows before rejecting an entry.
///
/// The bound matches the `SKILL.md` chain limit used by catalog validation so both layers reject
/// exactly the same pathological layouts and the crate has one traversal ceiling to reason about.
///
/// It counts the hops this walker *takes*, which are the links occupying the final component of
/// each path it inspects. A link sitting in an ancestor component — `a/b/c` where `b` is itself a
/// link — is resolved inside the operating system's own path lookup before the backend ever sees
/// the entry, so it is neither counted here nor recorded in [`ResolvedChain::hops`]. That is
/// deliberate: the kernel already bounds its own traversal, and a cycle hidden there surfaces as an
/// inspection error rather than as [`ChainState::Cyclic`]. This bound governs the chain `SkillMount`
/// walks, not the total number of indirections the filesystem contains.
pub const MAX_LINK_DEPTH: usize = 40;

/// Where a directory-link chain ended up.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ChainState {
    /// Nothing exists at the starting entry.
    Missing,
    /// The starting entry is a regular directory.
    Directory,
    /// The chain terminates in a directory.
    LinkToDirectory,
    /// The chain reaches a path that does not exist.
    Broken,
    /// The chain revisits an entry it already inspected.
    Cyclic,
    /// The chain is longer than [`MAX_LINK_DEPTH`].
    DepthExceeded,
    /// The chain reaches an entry that cannot be a directory.
    Unsupported,
}

impl ChainState {
    /// Returns whether the chain reached a usable directory.
    #[must_use]
    pub const fn reaches_directory(self) -> bool {
        matches!(self, Self::Directory | Self::LinkToDirectory)
    }

    /// Returns the stable label used in diagnostics.
    #[must_use]
    pub const fn label(self) -> &'static str {
        match self {
            Self::Missing => "missing entry",
            Self::Directory => "regular directory",
            Self::LinkToDirectory => "directory link",
            Self::Broken => "broken link",
            Self::Cyclic => "link cycle",
            Self::DepthExceeded => "link chain deeper than the supported maximum",
            Self::Unsupported => "non-directory entry",
        }
    }
}

/// One entry, the chain it travels, and the directory it ends at.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ResolvedChain {
    /// The visible entry, classified without following it.
    pub entry: PathEntry,
    /// How the chain ended.
    pub state: ChainState,
    /// Every link target in traversal order, each retaining its raw on-disk form.
    pub hops: Vec<LinkTarget>,
    /// Canonical terminal directory, present only when the chain reaches one.
    pub terminal: Option<PathBuf>,
    /// Platform identity of the terminal directory, when the host reports one.
    pub terminal_identity: Option<PlatformIdentity>,
}

impl ResolvedChain {
    /// Returns the canonical terminal directory, or an error describing why there is none.
    ///
    /// Callers that can act on an unusable entry read [`ResolvedChain::state`] instead. This
    /// accessor exists for the callers that cannot, such as junction eligibility, where an
    /// unresolvable source has to stop the operation rather than change its shape.
    ///
    /// # Errors
    ///
    /// Returns [`LinkError::UnresolvableChain`] when the chain is missing, broken, cyclic,
    /// over-deep, or terminates in something that is not a directory.
    pub fn require_directory(&self) -> Result<&Path, LinkError> {
        match self.terminal.as_deref() {
            Some(terminal) if self.state.reaches_directory() => Ok(terminal),
            _ => Err(LinkError::UnresolvableChain {
                entry: self.entry.path.clone(),
                state: self.state.label(),
            }),
        }
    }

    /// Returns whether this chain and `other` reach the same directory.
    ///
    /// Identity decides when both terminals report one, because two spellings of one directory on
    /// a case-insensitive volume are the same directory even though their paths differ. Two
    /// unresolvable entries are never equal: an unresolvable state carries no identity a later
    /// mutation could rely on.
    #[must_use]
    pub fn shares_terminal_with(&self, other: &Self) -> bool {
        match (
            self.terminal_identity.as_ref(),
            other.terminal_identity.as_ref(),
        ) {
            (Some(left), Some(right)) => left == right,
            _ => match (self.terminal.as_deref(), other.terminal.as_deref()) {
                (Some(left), Some(right)) => {
                    ComparablePath::new(left).names_same_path(&ComparablePath::new(right))
                }
                _ => false,
            },
        }
    }
}

/// Classifies `entry` and walks any directory-link chain it starts.
///
/// Broken, cyclic, over-deep, and non-directory layouts are returned as states rather than errors.
/// An unusable ancestor scope and an unusable authoritative entry have different consequences, so
/// the decision belongs to each caller instead of being forced into the walker.
///
/// # Errors
///
/// Returns [`LinkError::Inspect`] when the host reports a failure other than a missing path.
pub fn resolve_chain(backend: &dyn LinkBackend, entry: &Path) -> Result<ResolvedChain, LinkError> {
    let observed = backend.inspect_no_follow(entry)?;
    let mut chain = ResolvedChain {
        entry: observed.clone(),
        state: ChainState::Missing,
        hops: Vec::new(),
        terminal: None,
        terminal_identity: None,
    };

    let mut visited = BTreeSet::new();
    let mut current = observed;
    let mut current_path = entry.to_path_buf();

    // One iteration per entry inspected, which is one per hop plus the terminal it lands on. The
    // range is inclusive so that a chain of exactly `MAX_LINK_DEPTH` hops resolves and the next one
    // does not, rather than the bound silently admitting one fewer than it advertises.
    for _ in 0..=MAX_LINK_DEPTH {
        if !visited.insert(visit_key(&current_path, current.identity.as_ref())) {
            chain.state = ChainState::Cyclic;
            return Ok(chain);
        }

        match current.kind {
            EntryKind::Missing => {
                chain.state = if chain.hops.is_empty() {
                    ChainState::Missing
                } else {
                    ChainState::Broken
                };
                return Ok(chain);
            }
            EntryKind::Directory => {
                chain.state = if chain.hops.is_empty() {
                    ChainState::Directory
                } else {
                    ChainState::LinkToDirectory
                };
                chain.terminal = Some(backend.canonical_directory(&current_path)?);
                chain.terminal_identity = current.identity;
                return Ok(chain);
            }
            EntryKind::File | EntryKind::Other => {
                chain.state = ChainState::Unsupported;
                return Ok(chain);
            }
            EntryKind::Symlink | EntryKind::Junction => {
                let Some(target) = current.target else {
                    // A link the backend could not read a target for is not a chain this layer
                    // may guess about.
                    chain.state = ChainState::Unsupported;
                    return Ok(chain);
                };
                current_path.clone_from(&target.resolved);
                chain.hops.push(target);
                current = backend.inspect_no_follow(&current_path)?;
            }
        }
    }

    chain.state = ChainState::DepthExceeded;
    Ok(chain)
}

/// What one already-inspected hop is recorded as, for revisit detection.
///
/// A real platform identity is preferred because it detects a cycle that two different spellings
/// of one directory would otherwise hide. The normalized path is the fallback for an entry the
/// host reports no identity for, and [`MAX_LINK_DEPTH`] backstops both.
#[derive(Debug, PartialEq, Eq, PartialOrd, Ord)]
enum VisitKey {
    /// The entry's own platform identity.
    Identity(PlatformIdentity),
    /// The entry's normalized path.
    Path(PathBuf),
}

fn visit_key(path: &Path, identity: Option<&PlatformIdentity>) -> VisitKey {
    identity.map_or_else(
        || VisitKey::Path(ComparablePath::new(path).key),
        |identity| VisitKey::Identity(identity.clone()),
    )
}

/// A path retained in its original form alongside the form used to compare it.
///
/// Windows needs this split: `\\?\C:\Skills`, `\??\C:\Skills`, and `c:\Skills\` all name one
/// directory, but only the value the operator supplied belongs in a message. Unix needs it too,
/// though only for lexical `.` and `..` removal.
///
/// Equality is deliberately not derived. Two values with different display paths and one key are
/// the same path, and a derived comparison would silently say otherwise; callers use
/// [`ComparablePath::names_same_path`].
#[derive(Debug, Clone)]
pub struct ComparablePath {
    display: PathBuf,
    key: PathBuf,
}

impl ComparablePath {
    /// Normalizes `path` for comparison while retaining it for display.
    #[must_use]
    pub fn new(path: &Path) -> Self {
        Self {
            display: path.to_path_buf(),
            key: comparison_key(path),
        }
    }

    /// Returns the path exactly as it was supplied.
    #[must_use]
    pub fn display_path(&self) -> &Path {
        &self.display
    }

    /// Returns the normalized comparison form.
    ///
    /// Only ever compared against another key. It is not a path to hand to an operator or to a
    /// child process.
    #[must_use]
    pub fn key(&self) -> &Path {
        &self.key
    }

    /// Returns whether both values name the same path.
    #[must_use]
    pub fn names_same_path(&self, other: &Self) -> bool {
        self.key == other.key
    }

    /// Returns whether `other` is this path or lies beneath it.
    ///
    /// Comparison is by component, so `/skills/ab` is not contained by `/skills/a`.
    #[must_use]
    pub fn contains(&self, other: &Self) -> bool {
        other.key.starts_with(&self.key)
    }
}

/// Returns whether two raw link targets name the same path.
///
/// Used by verified removal, which compares what is stored in the live entry against what was
/// recorded at creation without following either.
#[must_use]
pub fn targets_match(recorded: &Path, live: &Path) -> bool {
    comparison_key(recorded) == comparison_key(live)
}

#[cfg(unix)]
fn comparison_key(path: &Path) -> PathBuf {
    crate::paths::lexical_normalize(path)
}

#[cfg(windows)]
fn comparison_key(path: &Path) -> PathBuf {
    use std::ffi::OsString;
    use std::os::windows::ffi::{OsStrExt, OsStringExt};

    let wide = path.as_os_str().encode_wide().collect::<Vec<_>>();
    PathBuf::from(OsString::from_wide(&super::winpath::comparison_key(&wide)))
}
