# ADR 0014: Scope Unix Link Ownership Binding to Cooperating Sessions

- **Status:** Accepted
- **Date:** 2026-08-02
- **Supersedes:** _none_

## Context

The architecture baseline previously reserved a follow-up that would bind ownership verification
and removal to one object across the verify-act window. That statement is achievable on Windows,
where an opened reparse-point handle can be retained through disposition or rename, but it is not
achievable with the supported macOS filesystem API.

The lock source trace explains the narrower Unix boundary. `src/lock/acquire.rs::take` opens a
hashed lock file under SkillMount's application-state directory and calls
`fs4::FileExt::try_lock`. `Cargo.lock` pins `fs4` 1.1.0; its Unix implementation maps `try_lock` to
`rustix::fs::flock` with `NonBlockingLockExclusive`. The lock is therefore on a SkillMount state
file, not on the discovery directory or its link entry. Apple's [`flock(2)` documentation][flock]
calls the lock advisory and says it enables cooperating processes to act consistently while other
processes may still access the file without using the lock.

The macOS 26.5 SDK installed for implementation supplied the API evidence. Its public headers
declare `unlink(const char *)`, `unlinkat(int, const char *, int)`, and
`renameatx_np(int, const char *, int, const char *, unsigned int)`. `unlinkat` anchors lookup at a
directory descriptor but still selects the entry by pathname; `renameatx_np` likewise selects both
entries by pathname. An exhaustive public-header search found no unlink-by-open-entry or
unlink-if-identity operation. Apple's [`unlink(2)` documentation][unlink] also defines removal in
terms of the link named by `path`. Thus a non-cooperating process can replace the name after the
last identity check and before pathname removal. Treating the advisory lock as directory exclusion
would promise a property neither the code nor the platform supplies.

[flock]: https://developer.apple.com/library/archive/documentation/System/Conceptual/ManPages_iPhoneOS/man2/flock.2.html
[unlink]: https://developer.apple.com/library/archive/documentation/System/Conceptual/ManPages_iPhoneOS/man2/unlink.2.html

## Decision

Windows link removal and staged placement bind verification and mutation to one no-follow handle.
Unix link removal and placement remain pathname operations: the application MUST hold all logical
and physical resource locks across them to exclude cooperating SkillMount sessions, and the backend
MUST verify ownership at the last available boundary without claiming exclusion over an arbitrary
process.

Unix placement verifies the staged entry before atomic no-replace rename and the destination after
rename. A visible pre-placement mismatch is refused without mutation. A post-placement mismatch is
reported as retained residue, is never deleted or moved in an attempted repair, and is never
recorded as applied; its write-ahead journal remains as operator evidence. Unix removal similarly
retains any mismatch visible at its final check, while documenting the non-cooperating replacement
window between that check and `unlink`.

## Alternatives

**Treat the advisory lock as exclusion over the destination directory.** Rejected because the lock
is held on a separate application-state file and Apple's contract requires every participant to
cooperate. A process that ignores the lock can still mutate the directory.

**Never remove macOS links.** Rejected because it would avoid the race by abandoning the normal
session lifecycle and leaving every mount behind. It does not preserve the product contract.

**Rename or swap the entry into quarantine before checking it.** Rejected because the rename still
selects its source by pathname. If another process crossed the window, SkillMount would already
have moved an entry it did not own, and restoration would introduce another pathname race.

**Change permissions on the containing directory.** Rejected because SkillMount does not own every
project or user discovery store, permission changes would be externally visible policy mutations,
and another process running with the same user authority is not reliably excluded.

## Consequences

- The supported macOS guarantee is cooperative serialization plus last-boundary evidence, not an
  object-bound unlink guarantee. A malicious or merely non-cooperating same-user process remains a
  residual risk during the final pathname window.
- Windows takes the stronger object-bound path because the platform exposes it. Cross-platform API
  outcomes therefore describe proof and residue explicitly instead of flattening both backends to
  the weaker guarantee.
- A Unix post-placement mismatch can leave a final entry and an incomplete journal. Reporting and
  operator evidence take priority over automatic cleanup of an entry whose ownership is unclear.
- No CLI, dependency, journal schema, agent-discovery, packaging, release-target, or lock-identity
  migration follows from this decision. Integrators see stricter typed placement outcomes only.
- `docs/architecture.md`, local safety comments, the directory-link contract, and native tests must
  distinguish object-bound Windows mutation from lock-scoped Unix pathname mutation.

## Verification

- Platform-neutral placement contract tests cover matching evidence, staged replacement,
  destination contention, and a post-placement mismatch that is retained rather than claimed.
- Unix backend tests force replacement before placement, replacement across the final pathname
  window, failed post-create inspection, and ownership-checked removal while source sentinels
  survive.
- Real-process transaction tests hold and contend the same resource locks across apply and cleanup,
  then prove operator-created mismatches stay journal-backed through rollback and recovery.
- Native Apple Silicon CI runs the guarded link and transaction suites. Native Windows x64 and x86
  CI separately proves the stronger handle-bound removal and placement contract.
- Review audits the public macOS SDK surface, all placement and deletion call sites, and the final
  architecture wording. The implementation evidence is recorded under
  `rasen/changes/harden-link-ownership-binding/evidence/`.
