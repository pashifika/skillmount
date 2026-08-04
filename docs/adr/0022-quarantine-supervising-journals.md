# ADR 0022: Quarantine Supervising Journals

- **Status:** Accepted
- **Date:** 2026-08-03
- **Supersedes:** _none_

## Context

ADR 0019 correctly defers cleanup when the supervisor cannot prove its managed process domain is
empty. Stale transaction recovery still assumed that free advisory locks proved every incomplete
transaction was safe to reconcile. That implication ends at child launch: the wrapper can die and
release its locks while a child or descendant continues using the mounted paths.

The failure was reproduced with the native fake agent. Its group leader exited while a descendant
ignored termination; the supervisor returned uncertainty and retained the mount, but a later
SkillMount invocation saw free locks and removed that live mount through ordinary stale recovery.
Neither a recorded PID nor the absence of the wrapper can prove process-domain death because IDs
are reusable and descendants can outlive their original parent.

## Decision

Immediately before every Codex spawn attempt, the transaction SHALL durably advance from `active`
to `supervising`. If the wrapper remains alive and launch fails or the managed process domain is
proven dead, ordinary cleanup SHALL advance that journal to `cleaning` and reconcile it normally.

A later invocation SHALL NOT automatically recover a `supervising` journal merely because all of
its advisory locks are free. Recovery SHALL classify it as quarantined, retain every recorded path,
and stop the mutating run with temporary exit category 75 before reconciling any automatically
recoverable neighbor. A held lock still classifies the journal as actively driven. Explicit
operator cleanup remains reserved and will require a separate process-domain assertion contract.

## Alternatives

- Treat free locks as sufficient after launch. Rejected by the live orphan reproduction.
- Record the child PID and recover when it disappears. Rejected because PID reuse and surviving
  descendants make that observation neither necessary nor sufficient.
- Persist platform process handles for later recovery. Rejected because handles do not survive
  wrapper process death as portable durable capabilities, and Unix process groups retain numeric
  reuse hazards after their leader is reaped.
- Quarantine every `active` journal. Rejected because crashes before child exposure are safely and
  usefully recoverable under the existing lock-and-ownership proof. The new status isolates the
  exact point where that proof becomes insufficient.

## Consequences

- A crash after `supervising` is persisted but before a child is actually spawned can produce a
  conservative false quarantine. Safety takes precedence over unattended cleanup.
- Failure to persist `supervising` prevents spawn, so the older `active` state remains safe for
  automatic recovery.
- Until `asm cleanup` is implemented, the diagnostic directs the operator to verify that related
  processes exited and account for the retained journal and paths manually.
- Older builds that do not know the additive status reject the journal as uninterpretable and also
  fail closed.

## Verification

- `tests/codex_session.rs::a_supervising_journal_is_quarantined_while_an_orphan_descendant_remains_alive`
  proves a live orphan survives both supervisor uncertainty and the next invocation.
- `tests/transaction.rs::a_session_stopped_after_supervision_intent_is_quarantined_not_recovered`
  proves the durable checkpoint is retained with its mount and produces exit 75.
- `src/journal/tests.rs::incomplete_and_terminal_statuses_are_partitioned_exhaustively` keeps
  `supervising` non-terminal but excludes it from automatic recovery.
- Recovery scans quarantined journals before healthy incomplete neighbors, so no cleanup can occur
  earlier merely because journal filenames sort differently.
