# ADR 0039: Distinguish Observation from Mutation Lock Access

- **Status:** Accepted
- **Date:** 2026-08-08
- **Supersedes:** the single-access lock-set assumption in ADR 0012 and only the lock-access classification portions of ADRs 0021 and 0034

## Context

A mutating session currently acquires every resource reported by discovery with the same exclusive operating-system lock and retains that complete set through child supervision and cleanup. Codex and OMP report user, administrator, compatibility, settings, plugin, custom, and recursively traversed roots even when SkillMount only reads them. Two sibling projects consequently serialize on common evidence despite mutating distinct project destinations.

Destination-only locking is insufficient. A project destination can be nested beneath a root another session observes, two logical paths can resolve to one physical directory, and one Agent's destination can be another Agent's discovery entry. Missing paths also need a shared logical identity before a physical identity exists.

The existing `fs4` 1.1.0 dependency already supplies a safe `FileExt::try_lock_shared` interface. Its tagged source implements the Unix operation with `FlockOperation::NonBlockingLockShared` and the Windows operation with `LOCKFILE_FAIL_IMMEDIATELY` without `LOCKFILE_EXCLUSIVE_LOCK`. The crate declares Rust 1.75, below SkillMount's Rust 1.85 minimum. The source record is captured in the change evidence and requires neither a dependency update nor a new SkillMount `unsafe` boundary.

## Decision

Every `LockResource` SHALL carry explicit `LockAccess::Observe` or `LockAccess::Mutate` intent. `Observe` covers namespace evidence SkillMount only reads; `Mutate` covers every logical and physical identity that a transaction may create, place into, exclude competing mutation from, prune, or remove. Adapters describe intent, while shared application and transaction code remains the sole authority for acquisition, recovery, journaling, apply, supervision, and cleanup.

Access SHALL NOT participate in the existing `skillmount-lock-v1` logical or physical key derivation. Requests that collapse to one key retain all diagnostic paths and fold to the strongest access, with `Mutate` stronger than `Observe`. Observation uses a shared operating-system lock; mutation uses an exclusive lock. Both remain held through the child lifetime and exactly one cleanup callback.

A locked rebuild that adds an earlier key or strengthens a held key SHALL drop the complete set, reacquire the strongest accumulated union in global order, and rerun recovery and discovery across the unlocked gap. SkillMount SHALL NOT upgrade a lock in place or apply a plan observed before reacquisition.

New journals SHALL use schema version 2 and record access on every lock. Schema version 1 remains readable by mapping every legacy record to `Mutate`. Transaction opening and recovery SHALL require held mutation access for every owned mutation identity; compatible observation access is never cleanup authority.

Operating-system lock state remains the only liveness evidence. Holder text remains owner-restricted, advisory, and multi-reader safe through transaction-specific ownership or compare-before-remove behavior. Read-only doctor probes use a non-creating exclusive attempt so either shared or exclusive holders are detected.

This ADR preserves ADR 0012's discovery-before-lock, recovery-before-plan, sorted acquisition, complete-lock-set, and unlocked-gap reinspection rules. It preserves ADR 0021's Codex inventory and destination model and ADR 0034's OMP namespace, launch, and mutation boundaries; it replaces only their implicit treatment of every reported root as exclusive access.

## Alternatives

- Lock only transaction destinations. Rejected because nested scopes, cross-Agent layouts, physical aliases, and missing logical paths would permit a writer to cross an active reader before spawn.
- Infer access from `LockResourceKind`. Rejected because one path can be both discovery evidence and a destination or backing store, while a missing path has no physical identity to bridge kinds.
- Upgrade observation locks in place. Rejected because competing upgrades can deadlock and an unlocked or partially upgraded interval cannot authorize the old plan.
- Release observation locks after spawn. Rejected for this change because it creates a second lock lifetime in journals and recovery and permits a later SkillMount writer to change a scope an Agent may reload. Shared child-lifetime locks already enable the required sibling-project concurrency.

## Consequences

- Independent project sessions may overlap when they share only observations; observation/mutation and mutation/mutation overlap still waits or returns temporary status 75 before apply.
- Each invocation computes conflicts from its own stabilized current snapshot. An already-running Agent receives no live-update guarantee, and external tools remain outside SkillMount's advisory-lock protocol.
- Access labels become durable recovery evidence. An older binary fails closed on a version-2 journal; rollback requires completing or explicitly cleaning that state with the new binary.
- Adapter reviews must classify every observed and mutation-capable identity, including helper chains and cross-Agent logical spellings. Transaction assertions fail closed if classification misses mutation authority.
- No CLI, permission, filesystem write scope, package, release, dependency, license, MSRV, or `unsafe` allowance changes.
- Shared-lock behavior becomes a native acceptance obligation on Windows x64/x86 and Apple Silicon macOS; cross-compilation is not runtime evidence.

## Verification

The decision is enforced by access/key aggregation and lock-acquisition tests in `src/lock/`, schema and access-drift tests in `src/journal/` and `src/transaction/`, adapter resource fixtures, and real-process Codex, Claude, OMP, recovery, read-only, and supervision suites. Acceptance requires native shared/shared overlap, shared/exclusive contention, crash release, doctor observation, sibling-project child overlap, same-destination and physical-alias serialization, and independent ownership-verified cleanup.

Primary-source evidence for the pinned lock implementation and final verification commands are recorded under `rasen/changes/allow-concurrent-agent-sessions-across-projects/evidence/`. Native acceptance run `31258758913` exercised implementation revision `3fa538fd` on Windows x64, Windows x86, and Apple Silicon macOS; every required runtime, quality, policy, and shell-completion job passed.
