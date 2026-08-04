# ADR 0027: Reload Recovery Journals Under Lock

- **Status:** Accepted
- **Date:** 2026-08-04
- **Supersedes:** _none_

## Context

Automatic recovery and `asm cleanup` must scan journals before they know which resource locks to
take. They previously adopted that pre-lock journal object after waiting for its locks. A real
two-process checkpoint reproduction showed that another session can advance the same journal from
`planned` to `active` or `supervising` during that wait. Adopting the stale object can then discard
ownership evidence for a placed mount or remove mounts while a child process may use them.

## Decision

A recovery scan is candidate discovery only. After acquiring a candidate's complete recorded lock
set, recovery must reload the same journal path, verify that its transaction identity, project
root, and complete lock set still match, and classify the refreshed status before any recovery or
explicit-cleanup mutation.

## Alternatives

- Trust the scan because journal writes are atomic. Rejected because atomic replacement prevents a
  torn read but does not keep a previously decoded value current while lock acquisition waits.
- Hold a global journal-store lock during scan and cleanup. Rejected because it serializes
  independent projects and does not replace the per-resource locks that protect mount mutations.
- Retry only when file metadata changes. Rejected because timestamps and file identities are not
  the durable transaction contract; the decoded immutable fields and current status are.

## Consequences

- Recovery performs one additional bounded journal read after lock acquisition.
- Disappearance, unreadability, or immutable-field drift after the scan fails closed before
  mutation. Absence cannot prove that another reconciler removed every recorded entry: an external
  actor can remove only the journal while leaving its mounts behind.
- Automatic recovery refreshes all free candidates before mutating any of them.
- A refreshed terminal journal is left alone, and a refreshed `supervising` journal is quarantined
  even if its stale scan state appeared automatically recoverable.
- Explicit cleanup reports a journal-specific lock I/O failure without erasing reports for earlier
  cleanup mutations.

## Verification

- `cleanup_reloads_a_journal_after_waiting_for_its_locks` reproduces the stale planned snapshot and
  proves explicit cleanup uses the refreshed active actions.
- `automatic_recovery_reloads_a_journal_that_advanced_to_supervising` proves automatic recovery
  quarantines the refreshed child-use state without removing its mount.
- `cleanup_reports_completed_mutations_before_a_later_lock_io_failure` protects partial reporting
  after an earlier journal was already changed.
- `cleanup_fails_closed_when_a_journal_disappears_after_its_candidate_scan` proves explicit cleanup
  reports unknown ownership and retains a mount after journal-only removal.
- `automatic_recovery_fails_closed_when_a_scanned_journal_disappears` proves a new session stops
  before planning or mutation under the same race.
- `cleanup_reconciles_overlapping_kept_journals_and_their_shared_helpers_in_one_pass` proves a
  batch claims shared locks only once and derives cleanup order from recorded ownership.
