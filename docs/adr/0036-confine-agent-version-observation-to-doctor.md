# ADR 0036: Confine Agent Version Observation to Doctor

- **Status:** Proposed
- **Date:** 2026-08-07
- **Supersedes:** ADR 0033's mandatory normal-session `--version` observation and its
  banner-mismatch session warning; ADR 0034's decision 6 session version-evidence rule; and ADR
  0021's residual exact `codex-cli 0.146.0` acceptance clause, which ADR 0033 replaced in
  implementation without naming

## Context

Every mutating Codex, Claude, and OMP session spawns one bounded `--version` child before SkillMount
state access and compares the banner with a single embedded string by exact equality
(`src/agent/version.rs:500-506`). Any other value emits an advisory warning
(`src/app.rs:206-214`). The operator's installed Claude Code reports `2.1.224 (Claude Code)` against
a last-tested `2.1.220 (Claude Code)`, and `docs/compatibility.md:20` already records `2.1.222` as
an `observed` banner, so the warning fires on every session for a release this project has itself
recorded as observed.

ADR 0033 established that a version is evidence, not authorization, and removed the banner as a
launch allowlist. It kept the observation in the session path so an operator would retain local
evidence. A review of what that evidence protects shows it protects nothing on that path.

Three accepted ADRs still carry a session version rule. ADR 0033 named only ADR 0023's Codex
probe rule and ADR 0024's Claude acceptance rule, so ADR 0021's independent clause — "a mutating
launch SHALL run a shell-free `--version` probe and accept exactly `codex-cli 0.146.0` before
SkillMount state inspection, locking, or mounting" (lines 102-103) — remained accepted while the
implementation stopped honoring it. ADR 0034's decision 6 then restated the advisory session
observation for OMP. Both are superseded here, so the recorded decisions and the implementation
agree again rather than diverging by one unnamed clause.

Mount-lifetime correctness does not consult the banner. `src/process/mod.rs:779-798` reaps the root
child and then requires `domain_is_empty` before cleanup runs; a non-empty domain yields
`ChildOutcome::Uncertain`, defers cleanup, and retains the journal
(`src/process/mod.rs:720-733`). An Agent that detaches therefore cannot have its links removed from
underneath it, with or without a version check. Ownership-verified removal and the write-ahead
journal supply the remaining two guarantees. All three are argument- and version-independent.

Auditing the adapters' passthrough guards against that boundary gives one result: every bypass lands
on a mount that is useless, not one that is unsafe. A disabled Skill surface, an undone visibility
override, a non-session subcommand, and an unclassified new option all end in the same place — the
Agent runs, exits, and cleanup proceeds correctly. `src/agent/claude.rs:265-274` already forwards
unknown options unchanged, which is the correct behavior for a wrapper whose contract is link
lifetime rather than Agent semantics.

A banner difference is uncorrelated with any of this. The operator cannot act on the warning, and
its absence certifies nothing. It is inert output on the one path where SkillMount otherwise spawns
no Agent process before the session child.

## Decision

A mutating session SHALL NOT observe an Agent version banner and SHALL NOT emit a version
compatibility warning. `doctor` remains the single surface that performs the bounded, shell-free,
contained `--version` observation and classifies the result `pass` or `unverified`.

`VersionSpec`, the last-tested banner constant in each adapter, the shared bounded observer,
`VersionEvidenceKind`, and `doctor_detail` are retained. The constant continues to be rendered
without a process by `inspect` (`src/render.rs:122-123`) and by the automatic-junction compatibility
warning (`src/app.rs:386-392`). `VersionObservation::session_warning` is removed.

ADR 0033's governing principle is preserved and applied more strictly: a value that authorizes
nothing is removed from the authorization path rather than retained as advice the operator cannot
act on.

## Alternatives

- **Shorten the warning or emit it once per banner.** Rejected because it reduces the volume of a
  signal that carries no information. Persisting "already warned" state adds a state-directory
  write to a path that currently makes none, to preserve output that cannot be acted on.
- **Enumerate observed banners in a ledger the binary consults.** Rejected because it makes the
  compiled binary the arbiter of a fact that changes on the operator's machine, requires a
  SkillMount release for every recorded Agent patch, and still silences nothing that matters. It
  also reintroduces the maintenance loop this decision removes.
- **Invocation-scoped Agent contract observation.** Rejected. The proposal would build the used CLI
  contract for each launch, probe the relevant Agent surface, and block on required-capability loss
  or parser ambiguity. It is well-formed but solves a problem that does not exist: the failures it
  detects produce a useless mount, never an unsafe one, so it purchases no safety while adding a
  second Agent process, a 64x increase in retained probe output, and a new environmental path to a
  refused launch. Retained as rejected input at
  `local_docs/skillmount-agent-contract-observation-design.md`.
- **Semantic-version ranges.** Already rejected by ADR 0033 for certifying unexecuted behavior; that
  rejection is unaffected.
- **Remove version observation entirely, including from `doctor`.** Rejected because `doctor` exists
  to report environment state, an operator contributing a compatibility record needs the observed
  banner, and the check is explicit and infrequent there rather than implicit on every session.

## Consequences

- Operators see no compatibility warning during a normal session, and session start no longer waits
  on a bounded three-second probe. No flag, exit code, mount behavior, or cleanup behavior changes.
  `asm doctor` output is unchanged.
- A normal session spawns exactly one Agent process instead of two.
- SkillMount gains no per-session signal that the installed Agent moved. This is accepted: the
  removed signal was uncorrelated with the integration. Drift remains the responsibility of
  `docs/compatibility.md` and the opt-in live smoke, which observe the scenario rather than the
  banner.
- A session in which the Agent starts, exits successfully, and never sees the mounted Skills stays
  undetected. This is not a regression; the banner check never detected it either. Detecting it
  requires an authenticated live run, which remains opt-in evidence.
- Removing the session call site must not remove ADR 0033's process-containment coverage. The
  oversized-stream, invalid-UTF-8, spawn-failure, timeout, descendant-handle, and domain-death cases
  are properties of the shared bounded observer and move to its unit tests and to `doctor` coverage.
- Restoring a session-time Agent probe later would need a new ADR and would have to state which
  safety property it establishes that the process domain, journal, and ownership-verified removal do
  not.
- `docs/architecture.md` and `docs/compatibility.md` are updated in the same product change so the
  recorded mutating-session flow matches the implementation.

## Verification

- The Codex, Claude, and OMP session acceptance suites assert that a mutating session spawns no
  `--version` child and emits no compatibility warning for any banner, including a banner differing
  from the adapter's last-tested constant and an executable that cannot produce one.
- `tests/read_only.rs` is unchanged and continues to prove that `inspect` and session `--dry-run`
  create no child process; the property becomes trivially true on the session path as well.
- `src/operator/doctor.rs` coverage retains `pass` for an exact banner and `unverified` for a
  different or unavailable one, and retains exit `0` for version uncertainty alone.
- The shared observer's unit tests retain every bounded-capture case ADR 0033 introduced:
  spawn failure, nonzero exit, timeout, oversized standard output and standard error, invalid UTF-8,
  control-character escaping, descendants inheriting captured handles, and domain-death proof.
- `tests/transaction.rs` and the process-supervision suite are unchanged, which is the evidence that
  cleanup correctness never depended on the removed observation.
