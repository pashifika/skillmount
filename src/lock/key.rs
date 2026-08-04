//! Hashed lock keys derived from resource identity.
//!
//! A lock is a file, and a file needs a name that is legal on both platforms, bounded in length,
//! and identical for two processes that describe one resource differently. A path cannot be that
//! name: it may exceed the host's component limit, it may contain separators, and on Windows it may
//! contain characters a filename cannot. Hashing the identity solves all three at once.
//!
//! Two rules make the derivation safe. Every component is length-prefixed before it is hashed, so
//! `("ab", "c")` and `("a", "bc")` cannot produce the same digest — without that, a resource named
//! `.codex` under `skills/` and one named `.codexskills` under nothing would collide. And a domain
//! string leads every digest, so a key from this crate can never coincide with a digest some other
//! tool wrote into the same directory.

use std::fmt::{self, Write as _};

use sha2::{Digest, Sha256};

use crate::link::PlatformIdentity;
use crate::native::os_bytes;

use super::{LockResourceIdentity, LockResourceKind};

/// Prefix that scopes every digest to this crate and this key derivation.
///
/// The version suffix is load-bearing. Changing what goes into a key must change every key at
/// once, because a build that hashed the old inputs and one that hashed the new ones would
/// otherwise take different locks for the same resource and run concurrently.
const DOMAIN: &[u8] = b"skillmount-lock-v1";

/// A resource key rendered as the lock filename that protects it.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct LockKey(String);

impl LockKey {
    /// Returns the filename, which is the digest plus the lock extension.
    #[must_use]
    pub fn file_name(&self) -> String {
        format!("{}.lock", self.0)
    }

    /// Returns the digest.
    #[must_use]
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl fmt::Display for LockKey {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(&self.0)
    }
}

/// Derives the key a resource has whether or not it exists yet.
///
/// The anchor and suffix are hashed separately rather than joined, for the same reason they are
/// stored separately: joining would let a plan that creates intermediate directories change the
/// split and therefore the key.
#[must_use]
pub fn logical(kind: LockResourceKind, identity: &LockResourceIdentity) -> LockKey {
    digest(&[
        b"logical",
        kind.label().as_bytes(),
        &os_bytes(identity.anchor.as_os_str()),
        &os_bytes(identity.suffix.as_os_str()),
    ])
}

/// Derives the key an existing resource has, regardless of the path used to reach it.
///
/// The resource kind is deliberately *not* part of this key. Two kinds that reach one directory —
/// a Codex discovery entry linking to its own backing store — are genuinely the same resource, and
/// serializing them together is the conservative answer.
#[must_use]
pub fn physical(identity: &PlatformIdentity) -> LockKey {
    digest(&[b"physical", identity.as_str().as_bytes()])
}

fn digest(parts: &[&[u8]]) -> LockKey {
    let mut hasher = Sha256::new();
    hasher.update(DOMAIN);
    for part in parts {
        // Length-prefixing keeps concatenation unambiguous, so no two different component lists
        // can hash to one key.
        hasher.update((part.len() as u64).to_be_bytes());
        hasher.update(part);
    }
    let mut rendered = String::with_capacity(64);
    for byte in hasher.finalize() {
        let _ = write!(rendered, "{byte:02x}");
    }
    LockKey(rendered)
}
