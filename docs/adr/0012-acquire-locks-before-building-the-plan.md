# ADR 0012: Acquire Locks After Discovery and Build the Plan Only After Recovery

- **Status:** Accepted
- **Date:** 2026-08-02
- **Supersedes:** the application-flow ordering in the `implement-transaction-recovery-and-locking`
  design decision "Re-inspect and rebuild under locks"

## Context

The transaction change had to choose where lock acquisition and stale recovery sit relative to
planning. Its design decision specifies:

> Application flow is preliminary plan, stable lock-set acquisition, incomplete-transaction
> recovery, discovery re-inspection, full plan rebuild, journal creation, apply, active child,
> cleanup.

That ordering is unimplementable as written, and the failure is not subtle. A *plan* is the output
of the conflict table, and the conflict table's job is to refuse a destination it cannot safely use.
A session that was force-killed between apply and cleanup leaves exactly such a destination: a mount
pointing at a source the new run did not select. Building the preliminary plan first therefore makes
the run exit 73 before it ever acquires a lock — and recovery, which exists to remove that entry, is
never reached. The state is permanent: every subsequent run fails the same way at the same point.

This was found by test during implementation rather than by review.
`tests/transaction.rs::recovery_never_removes_an_entry_a_user_replaced_after_the_crash` kills a real
`asm` at the `journal-active` boundary, replaces the mount, and runs a real second invocation. With
the design's ordering the second invocation reported only the conflict and no recovery ever ran.

The lock set does not actually depend on the plan. `AgentAdapter::inspect_discovery` produces
`DiscoverySnapshot::lock_resources` on its own; only the *actions* need the catalog and the conflict
table. The design's "preliminary plan" step asks for strictly more than locking requires.

## Decision

`app::run_session` runs, in order: mint the transaction identifier, inspect discovery, acquire the
snapshot's lock set, reconcile incomplete transactions, build the full plan, acquire any lock the
rebuilt plan added, open the journal, apply, clean up.

Discovery, not a plan, is what precedes lock acquisition. The plan is built exactly once, under the
lock and after recovery, and it is the plan that gets applied. There is no "preliminary plan" and no
"rebuild".

The identifier is minted before discovery rather than at journal creation, because a Claude staging
layout is addressed by it. A preliminary layout uses the `crate::state::PENDING_SESSION` placeholder,
which is one shared path; locking that would serialize two sessions that never touch the same
directory.

## Alternatives

Three orderings were considered against the failing test.

- Keep the design's ordering and make the preliminary plan non-fatal, discarding its errors until
  after recovery. Rejected: the conflict table is the only thing that decides whether a destination
  is usable, so "plan, ignore the answer, plan again" runs it twice and gives the first run's result
  no meaning. It also doubles every filesystem inspection for no benefit.
- Keep the design's ordering and make the conflict table recovery-aware, so it tolerates a
  destination that some incomplete journal claims. Rejected: it puts knowledge of transaction state
  into the planning layer, which is required to stay side-effect-free and journal-unaware, and it
  would make the read-only `--dry-run` output depend on whether a journal happens to exist.
- Acquire locks with no prior observation at all, from a fixed set derived from the project root.
  Rejected: the resource set genuinely depends on the observed layout — a Codex `.agents/skills`
  that links to its own backing store collapses to one physical resource, and a Claude staging root
  is not under the project root at all — so a fixed set would either over-lock unrelated sessions or
  miss the store actually being mutated.

## Consequences

What this commits the project to:

- Discovery runs before any lock is held. A destination observed there can change before the plan is
  built, which is why every action re-checks its persisted precondition at apply time and placement
  is atomic and no-replace. Those two mechanisms are load-bearing rather than defensive, and neither
  may be removed as redundant.
- Catalog validation failures now surface after locks are taken rather than before. A caller sees
  the same exit code; the only difference is that a lock file may have been created first.
- `Transaction::open` and `Transaction::adopt` take `&HeldLocks` and refuse when the caller does not
  hold the plan's locks. The ordering above is therefore enforced at the type-and-runtime boundary,
  not only by the call site in `run_session`.
- The design's flow sentence stays as written. It is a machine-local input and the historical record
  of what was intended; this ADR is the repository's record of what is true.

Child launch keeps its place in the design's flow, between `active` and cleanup. This ADR reorders
the steps before apply and does not move or remove it; it is deferred, and
`rasen/changes/implement-transaction-recovery-and-locking/evidence/capability-audit.md` records what
that leaves unexercised.

What becomes harder: reordering these steps again means re-deriving which of them may fail before a
lock is held. The migration path is to change `run_session` and re-run the crash suite, which is what
catches the regression.

## Verification

The decision is enforced by tests rather than by review:

- `tests/transaction.rs::recovery_never_removes_an_entry_a_user_replaced_after_the_crash` fails if
  planning is moved back in front of recovery, because recovery stops being reached.
- `tests/transaction.rs::a_second_invocation_recovers_every_boundary_and_leaves_the_project_clean`
  kills a real process at each of the eleven durable boundaries and requires a real second
  invocation to reconcile it.
- `src/transaction/tests.rs::a_transaction_refuses_to_open_without_the_locks_its_plan_needs` and
  `::a_partially_locked_session_cannot_open_a_transaction` fail if the lock guard is weakened.
- `tests/transaction.rs::two_isolated_claude_sessions_do_not_serialize` fails if the identifier is
  minted after discovery, because both sessions then lock the placeholder path.

Evidence: `rasen/changes/implement-transaction-recovery-and-locking/evidence/verification-report.md`.
