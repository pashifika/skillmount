# SkillMount architecture baseline

This document is the tracked current-state architecture baseline for SkillMount. It records the
product boundary, execution flow, module responsibilities, dependency and mutation boundaries,
cross-module safety rules, supported targets, and implementation status.

It is deliberately a repository baseline rather than a complete product specification. Detailed
state tables, failure scenarios, algorithms, and platform call contracts stay in tests, focused
architecture decision records (ADRs), or the code closest to the behavior. A behavior appears here
as current only after it exists in the tracked source and can be traced to repository evidence.

Status: catalog resolution, discovery inspection, read-only planning, cross-platform link
primitives, resource locking, durable transactions, cleanup, and stale recovery are implemented.
Agent child launch, operator commands, release automation, and the remaining transaction-lifetime
hardening named under [Reserved work](#reserved-work) are not implemented.

## Product definition

SkillMount is a Rust wrapper CLI that makes Agent Skills stored in external directories visible for
the intended lifetime of a Codex CLI or Claude Code CLI session. It resolves an ordered catalog,
inspects the discovery scopes implemented by the selected adapter, produces a deterministic mount
or preservation outcome for each selected Skill, and owns only the entries its transaction created.

One package installs two behaviorally identical binaries:

- `asm`, the primary command used by documentation;
- `skillmount`, a fallback name that delegates to the same `skillmount::run_from` entry point.

The initial release targets are:

- `i686-pc-windows-msvc`;
- `x86_64-pc-windows-msvc`;
- `aarch64-apple-darwin`.

Linux and WSL are not release targets. Linux-specific code exists only where the Ubuntu quality
job needs to exercise a portable invariant such as atomic no-replace placement. Windows ARM64 and
macOS Intel are also outside the version-one acceptance matrix.

### Non-goals

Version one does not download, update, sign, install permanently, or transform Skills. It does not
manage Codex or Claude authentication, weaken either agent's permission model, provide a GUI or
resident daemon, or claim that selected Skills are safe. SkillMount treats Skills as trusted,
user-selected code and instructions.

SkillMount is not an operating-system sandbox. It does not elevate through UAC or `sudo`, modify an
agent's configuration to grant access, or edit the user's Git ignore rules or index as product
behavior.

## Current command behavior

| Surface | Current behavior |
|---|---|
| `asm inspect` | Resolves catalogs and both agents' discovery layouts without mutation. |
| `asm codex --dry-run` | Produces the Codex plan without directories, links, locks, journals, recovery, or child launch. |
| `asm claude --dry-run` | Produces the isolated Claude staging plan under the same read-only contract. |
| `asm claude --mount-mode=project` | Uses the project's `.claude/skills` namespace instead of isolated staging; `--dry-run` keeps that plan read-only. |
| Session without `--dry-run` | Locks, recovers, replans, journals, applies, and cleans up, then exits at the reserved child-launch boundary. |
| Session with `--keep-mounts` | Runs the same mutation path but records terminal kept state instead of removing owned entries. |
| Session with `--no-recover` | Refuses when incomplete state requires reconciliation; otherwise continues through the normal mutating path. |
| `doctor`, `cleanup` | Parsed but rejected as reserved and unimplemented. |

A mutating invocation therefore does not yet run Codex or Claude and returns internal exit category
70 at that reserved boundary. Ordinary cleanup attempts to release every owned entry, while
`--keep-mounts` retains them intentionally; any entry cleanup cannot prove safe to remove remains
reported and journal-backed.

## Execution architecture

The read-only path is a strict composition of pure observation and planning:

```text
argv
  -> cli parsing
  -> invocation / launch / project path resolution
  -> catalog discovery -> rightmost-wins selection -> winner validation
  -> agent discovery inspection
  -> deterministic mount plan
  -> inspect / dry-run rendering
```

`inspect` and `--dry-run` stop at rendering. `tests/read_only.rs` snapshots the project, Skill
sources, and redirected user state around success and error paths, and fails if a child executable
is reached.

The mutating path deliberately does not build a complete plan before locking:

```text
read-only journal preflight
  -> ensure the staging-state base when staging is required
  -> mint transaction / staging identity
  -> discovery-only inspection
  -> acquire logical and physical resource locks
  -> recover eligible incomplete transactions
  -> catalog + discovery + full plan under the locks
  -> expand or reacquire the lock set until it stabilizes
  -> persist planned journal
  -> write-ahead apply
  -> [child launch reserved]
  -> reverse-order cleanup or terminal kept state
```

Discovery can run before the lock because it independently identifies the resources that may be
mutated. The conflict-producing plan cannot: crash residue may look like an ordinary destination
conflict until recovery removes it. This ordering is the accepted decision in
[ADR 0012](adr/0012-acquire-locks-before-building-the-plan.md).

## Module responsibilities

| Module | Responsibility |
|---|---|
| `src/cli.rs` | Shared `clap` contract and conversion into typed commands. |
| `src/paths.rs` | Invocation CWD, launch CWD, project root, source occurrence, and executable resolution. |
| `src/catalog/` | No-follow source discovery, ordered overlay selection, and selected-winner validation. |
| `src/agent/` | Codex and Claude discovery inspection and declarative plan construction. |
| `src/mount/` | Destination conflict policy and deterministic, read-only mount actions. |
| `src/render.rs` | Read-only reports and warnings. |
| `src/lock/` | Logical/physical resource identities and sorted operating-system advisory locks. |
| `src/journal/` | Versioned, checksummed write-ahead ownership records and durable storage. |
| `src/transaction/` | Apply, rollback, ordinary cleanup, kept state, and stale recovery. |
| `src/link/` | Sealed platform boundary for no-follow inspection, link creation, no-replace placement, and verified entry removal. |
| `src/state.rs` | Computes state locations and, only after the mutation boundary, creates their requested final directories with platform-specific access restrictions. |
| `src/native.rs` | Lossless platform-native path encoding for journals and lock keys. |
| `src/domain.rs` | Shared values crossing the catalog, adapter, plan, lock, and transaction boundaries. |
| `src/error.rs` | Typed failures and stable sysexits-style wrapper categories. |
| `src/diagnostic.rs` | Non-fatal structured observations returned by lower layers. |
| `src/app.rs` | The only top-level orchestration of read-only and mutating flows. |

### Dependency and mutation boundaries

The catalog, adapters, and mount planner describe state; they do not change it. In particular, an
`AgentAdapter` observes agent-specific discovery and returns `DiscoverySnapshot` and `MountPlan`
values. Discovery classification reaches the sealed link backend through `mount::resolve` for
no-follow inspection, but the adapter never invokes a mutating platform operation.

The shared application layer owns ordering and policy. The transaction layer owns multi-step
mutation and durable ownership. The sealed link backend provides both read-only inspection and
narrow mutation primitives; it does not decide when a plan should be applied, recovered, or cleaned
up. There is no recursive removal operation in the link contract.

`src/link/` therefore serves two callers without owning their policy:

```text
agent discovery -> mount::resolve -> sealed link inspection
catalog + agent snapshot -> mount plan -> transaction policy -> sealed link mutation
```

## Catalog and planning rules

Every `--skills-dir` occurrence is retained with its ordinal and provenance. The occurrences form
an ordered overlay: the rightmost candidate for a logical mount-name key wins. Selection happens
before validation. An invalid selected winner is a hard error; SkillMount never falls back to an
earlier shadowed candidate.

Source precedence chooses only among caller-supplied candidates. It never authorizes replacement
of a project-owned or otherwise pre-existing Skill in an inspected scope. Planning checks every
scope in the current adapter discovery model, not just the destination directory, because relying
on undocumented duplicate precedence would make the selected Skill ambiguous. Before child launch
is implemented, each session adapter must reconcile that model with the supported agent versions
and cover every scope the child will search.

SkillMount validates every name it creates against the portable `SkillName` grammar. Existing
entries are stored as their raw platform-native name plus a comparison key because users and other
tools are not required to follow that grammar. Dropping such an entry would make conflict detection
unsound; [ADR 0010](adr/0010-discovery-entry-identity.md) records this decision.

## Agent discovery models

| Concern | Codex | Claude Code |
|---|---|---|
| Current modeled discovery | `.agents/skills` from launch CWD through project root, plus the project `.codex/skills` compatibility candidate | Selected destination, project and user `.claude/skills`, plus user-supplied `--add-dir` scopes |
| Compatibility | Existing `.agents/skills -> .codex/skills` layouts may use `.codex/skills` as their backing store | No project compatibility store is created |
| Planned destination | Project discovery/backing store chosen by the Codex state table | Default: unique state-root staging tree at `<session>/root/.claude/skills`; project mode: `<project>/.claude/skills` |
| Project mutation | Transaction-owned entries may be added to the selected project store | Project mode may add transaction-owned entries; default staging does not modify the project namespace |
| Launch integration | Reserved | Reserved; the current default-staging plan includes `<session>/root` via `--add-dir`, while the project-mode plan adds no argument |

Scopes that resolve to one terminal directory are folded for conflict and lock purposes while their
visible aliases remain available for diagnostics. That prevents a conventional
`.agents/skills -> .codex/skills` layout from being mistaken for two competing namespaces.

This table describes the implemented read-only planning model, not completed launch integration.
The reserved session-adapter work must revalidate each agent's discovery behavior and argument
contract against the supported CLI versions before it may launch a child.

Claude `--add-dir` values are preserved for forwarding. Absolute values identify the same scope for
planning and a future child; relative values are currently inspected relative to SkillMount's own
process directory rather than the resolved launch CWD. Aligning those path semantics is part of the
reserved argument-contract work.

## Locks, journals, and recovery

Resource descriptions separate a logical identity from an optional physical identity. The logical
key uses an existing canonical anchor plus a normalized suffix, so the first process creating a
missing directory and a later process observing it contend on the same key. A physical identity
makes aliases and worktrees that reach one existing directory contend as well.

Advisory locks are acquired in sorted key order. A lock file is not liveness evidence because it
survives process death; only the operating-system lock is. Human-readable holder information lives
in a sidecar so it remains readable while Windows holds a mandatory byte-range lock.

A transaction persists its journal before each planned destination mutation can become externally
visible. The journal distinguishes intent, staged identity, final placement, active use, cleanup,
kept state, and failure. Its path codec round-trips arbitrary Unix bytes and Windows UTF-16,
including unpaired surrogates, rather than passing ownership evidence through UTF-8.

Apply rechecks every planned precondition and uses evidence-bearing, atomic same-filesystem
no-replace placement. Successful placement returns identity for the object established at the final
path before the journal advances to `applied`; a visible mismatch remains journal-backed residue.
Rollback and ordinary cleanup share the same reverse-order removal path. Windows derives
attributes, strongest identity, and reparse data from one no-follow handle and retains that handle
through rename or disposition. Unix performs final no-follow verification before pathname mutation
while the application holds all cooperating-session locks; those advisory state-file locks do not
exclude another program, so ADR 0014 records the residual final pathname race. A creation whose
ownership cannot be established is reported and retained rather than removed by an unchecked
pathname rollback. Recovery eligibility comes from the held lock set, never a recorded PID: PIDs
are reusable and cannot authorize cleanup. Unreadable or future-schema journals block new mutation
and are retained for operator inspection.

## Platform and unsafe boundary

The crate sets `unsafe_code = "deny"`. Exactly two modules may opt in:

- `src/link/unix_ffi.rs`;
- `src/link/windows_ffi.rs`.

They wrap filesystem operations that have no safe standard-library equivalent, including atomic
no-replace placement and Windows reparse-point observation, handle rename, and handle disposition.
Each unsafe block has a `SAFETY` justification, raw platform types do not cross the module boundary,
and reparse decoding stays in safe Rust. [ADR 0011](adr/0011-scoped-unsafe-for-platform-link-backends.md)
records why `deny` with this audited scope replaced crate-wide `forbid`.

Paths and forwarded arguments remain `PathBuf` and `OsString` through every public seam. They are
never joined into a shell command or converted lossily for policy, journal, lock, or ownership
decisions. Diagnostics may render a reversible representation only at the output boundary.

macOS uses directory symbolic links and `renameatx_np(RENAME_EXCL)`. Windows prefers a directory
symbolic link and falls back to a junction only for `ERROR_PRIVILEGE_NOT_HELD` in automatic mode;
placement uses `SetFileInformationByHandle(FileRenameInfo)` with replacement disabled, and removal
uses `FileDispositionInfo` on the verified handle. The variable-length rename layout is compiled and
tested on both Windows x86 and x64. The Linux test branch uses `renameat2(RENAME_NOREPLACE)` but does
not establish Linux product support.

## Cross-module invariants

The following are product rules rather than style preferences:

1. Platform-native paths and arguments remain lossless end to end.
2. Source occurrences are rightmost-wins, validate-after-select, with no fallback from an invalid
   winner.
3. Source precedence never overrides a pre-existing Skill in an inspected discovery scope.
4. `inspect` and `--dry-run` create no directories, links, locks, journals, recovery mutations, or
   child processes.
5. Discovery supplies the first lock set; recovery runs under locks before the first complete
   mutating plan is accepted.
6. No planned destination mutation occurs before its durable intent, and apply rechecks persisted
   preconditions.
7. Windows mutation verifies and mutates the same no-follow object handle. Unix pathname mutation
   performs the last available identity check under cooperating-session locks; visible uncertainty
   is retained and reported, and advisory locks are never described as excluding other programs.
8. Link removal never recursively descends into a target directory.
9. Shared application and transaction code own policy and sequencing; agent adapters inspect and
   plan, while the sealed platform backend executes only the link primitives it is asked for.
10. Product behavior never edits Git state, escalates privileges, or weakens agent permissions.

Tests enforce the observable parts of these rules. Local comments retain the narrower preconditions
needed to preserve them inside an implementation.

## Implementation status

### Implemented

- shared CLI parsing, path resolution, stable exit categories, and equivalent binary entry points;
- catalog discovery, overlay selection, selected-winner validation, and provenance;
- Codex and Claude discovery inspection and deterministic read-only planning;
- `inspect`, `--dry-run`, concise/verbose plan rendering, and read-only regression tests;
- Unix/macOS symbolic-link and Windows symbolic-link/junction backends;
- no-follow link-chain resolution, evidence-bearing atomic no-replace placement, Windows
  handle-bound mutation, and Unix ownership-checked pathname mutation under cooperative locks;
- logical and physical resource locks, versioned journals, write-ahead apply, rollback, cleanup,
  terminal kept state, and stale recovery;
- crash-boundary, concurrency, path-encoding, ownership, and native platform test coverage.

### Reserved work

- child-process supervision and the actual Codex/Claude launch boundary;
- complete Codex and Claude session adapters built on that supervisor, including discovery-model
  and argument-contract validation against the supported agent versions;
- `doctor`, explicit `cleanup`, lock-file reclamation, compatibility evidence, and user recovery
  documentation;
- binding a public transaction's lifetime to the lock guard validated when it is opened or adopted;
- rejecting pre-existing links in application-state directory paths before creation or permission changes;
- versioned release packaging and publication.

Until child launch is implemented, a normal session applies and then cleans up before returning
exit category 70. Until operator cleanup is implemented, lock files accumulate for distinct logical
and physical lock keys. Owner sidecars are removed on ordinary release but may remain after a crash
or failed removal; neither file's presence blocks or proves a live session.

## Documentation governance

Documentation is part of implementation:

- this file is the normative current architecture baseline;
- a change that replaces a normative decision adds or updates a focused ADR from
  [the template](adr/0000-template.md) and updates this file to the resulting current state;
- an ordinary implementation of an already recorded rule does not need another ADR;
- code comments own local reasoning, `SAFETY` obligations, and preconditions;
- tests and source provide evidence for current status, and any mismatch with this baseline is
  resolved explicitly rather than ignored;
- Rasen changes and pull-request bodies describe planned or reviewed deltas and remain history, not
  the only current-state documentation;
- root-anchored product ignore rules keep the separately managed `rasen/`, `.rasen/`, and
  `local_docs/` paths out of ordinary staging; their contents remain machine-local planning or
  historical inputs, never required checkout context.

[ADR 0013](adr/0013-track-current-architecture-and-agent-guidance.md) records this authority split
and the decision to share one tracked guidance source between coding agents.

Read [CONTRIBUTING.md](../CONTRIBUTING.md) before changing the implementation or this baseline.
