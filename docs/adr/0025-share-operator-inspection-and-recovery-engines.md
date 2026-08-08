# ADR 0025: Share Operator Inspection and Recovery Engines

- **Status:** Accepted; vector-only recovery presentation superseded by ADR 0038
- **Date:** 2026-08-03
- **Supersedes:** _none_

## Context

The implemented session adapters now have enough platform and discovery behavior that an operator
cannot safely diagnose residue from filenames or process IDs. ADR 0022 deliberately quarantines a
`supervising` journal because free wrapper locks do not prove that a descendant agent exited, while
the transaction layer removes an entry only after live kind, target, identity, and directory
contents match durable evidence. A separate cleanup implementation would weaken those proofs.

The public `doctor` and `cleanup` names were parsed but rejected. The hardening change needs stable
exit behavior, a way to inspect both explicit and `PATH` agent binaries, and an explicit operator
decision that can release a quarantined or intentionally kept transaction. The executable-seam
tests under `tests/operator_commands.rs` and `tests/transaction.rs` reproduce these needs.

## Decision

`asm doctor` SHALL reuse the agent discovery and no-follow link resolvers, observe existing
advisory locks without creating lock files, and run any mutating link check only in a unique
owner-restricted temporary directory. It reports typed `pass`, `warning`, `failure`, and
`unverified` findings on stdout; any failure returns data status 65, while warnings and unverified
compatibility alone return success. `--codex-bin` and `--claude-bin` select explicit shell-free
executables, otherwise each executable is resolved through `PATH`. Capturing a version necessarily
executes that selected external program with the single literal `--version` argument. SkillMount
does not mutate the project while diagnosing it, but an operator-selected executable remains
trusted code whose own side effects SkillMount cannot sandbox or characterize.

`asm cleanup` SHALL enumerate only validated journal files from SkillMount's transaction state,
claim every recorded resource lock through immediate, internally sorted attempts, and adopt the
journal into the ordinary transaction cleanup path. Immediate attempts never wait while an
accumulated key is held. One invocation keeps successfully claimed keys in a shared set so
overlapping journals never contend with locks that invocation already owns. It derives batch
cleanup order from recorded ownership: a transaction owning a descendant entry is reconciled
before the transaction owning its shared helper directory. A scoped invocation selects the
canonical recorded project; `--all` is an explicit mutually exclusive global scope. Invoking
cleanup is the operator's explicit assertion that selected `supervising` process domains should be
dead and that selected `kept` mounts should be released; free locks are still required and PID text
is never authority. Because every product transaction has at least one mutable discovery resource,
a current-schema journal with no recorded locks is an impossible state and is rejected as corrupt
rather than treating the empty set as proof that all locks are held.

Unreadable journals remain a global fail-closed condition and prevent every cleanup mutation.
Active or unreadable state returns temporary status 75, ownership-retained or filesystem cleanup
failure returns 73, and a complete/no-op pass returns zero. When conditions overlap, 73 takes
precedence. Retry operations remain executable-plus-argument native values. ADR 0038 supersedes
only this record's requirement to display them exclusively as indexed native values.

## Alternatives

- Add a cleanup-specific recursive or prefix-based remover. Rejected because a name such as
  `.skillmount-*`, an empty directory, or a reused PID proves neither ownership nor liveness.
- Automatically recover `supervising` whenever wrapper locks are free. Rejected by ADR 0022's
  orphan-descendant reproduction; only a deliberate operator command adds the missing assertion.
- Continue past a corrupt journal and clean readable neighbors. Rejected because the unknown record
  may own the same resources; scope cannot be proved from a journal that cannot be decoded.
- Probe link capability inside `.agents`, `.codex`, or `.claude`. Rejected because diagnosis would
  then mutate the project namespace it is meant to inspect.
- Print one portable or unconditional copy-paste shell command. Rejected because platform-native
  and non-Unicode arguments cannot share one portable representation. ADR 0038 later permits a
  narrower detected-shell convenience line only after an exact encoder succeeds, with this native
  representation retained as the fallback.

## Consequences

- Operators can diagnose unsupported versions and layouts without starting an agent, and can
  explicitly release kept or quarantined state without bypassing ownership checks.
- A false-positive `supervising` quarantine needs an explicit cleanup invocation; SkillMount still
  cannot independently prove descendant death after wrapper loss.
- A single corrupt journal blocks cleanup of unrelated valid state until it is accounted for. This
  is deliberately less available than partial cleanup and preserves the existing unknown-ownership
  invariant.
- Multiple valid kept journals that share discovery helpers are claimed as one lock batch and
  converge in one cleanup pass without misclassifying this invocation's own lock as active.
- Doctor can create temporary probe entries outside the project. It must report any residue it
  cannot ownership-verify and never remove recursively.
- Doctor's own observation and probes are non-destructive to the project, but version capture runs
  the selected trusted agent executable. This is not an operating-system sandbox boundary.
- The CLI and exit mapping are now public compatibility contracts. Future changes require an ADR,
  architecture update, and executable-seam regression coverage.

## Verification

- `tests/operator_commands.rs` covers explicit and `PATH` executables, versions, healthy/failing
  findings, exact broken/cyclic chains, isolated probe failure, reversible non-Unicode rendering,
  and project/state no-mutation.
- `src/lock/tests.rs::advisory_observation_is_read_only_and_uses_the_kernel_lock_as_liveness`
  proves doctor lock observation creates no files and ignores stale lock-file existence.
- `tests/transaction.rs` exercises scoped/all cleanup, active locks, kept and supervising journals,
  overlapping kept journals and helper ownership, corrupt-state fail-closed behavior, ownership
  mismatches, and project boundaries through real `asm` processes.
- `rasen/changes/add-operator-diagnostics-and-hardening/evidence/test-matrix-audit.md` maps the
  remaining source-design and platform evidence, including native tests that cannot be inferred
  from a portable run.
