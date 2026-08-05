# ADR 0033: Treat Agent Versions as Advisory Evidence

- **Status:** Accepted
- **Date:** 2026-08-05
- **Supersedes:** ADR 0023's exact Codex version-probe rule and ADR 0024's exact Claude Code version acceptance/probe rule only

## Context

ADRs 0023 and 0024 use one exact banner for two different purposes: dated evidence for the Agent release whose discovery and lifecycle behavior was investigated, and authorization to launch. SkillMount consequently probes the same banner three times and rejects every other or unavailable banner before the Agent can run, although no transaction identity, mutation precondition, cleanup decision, or process-liveness proof depends on that text.

The 2026-08-05 implementation review observed `codex-cli 0.146.0` and `2.1.222 (Claude Code)` on macOS/Apple Silicon. Current official Codex Skills and configuration documentation still describes the modeled repository, user, administrator, bundled, symlink, and configuration layers. Claude Code's official 2.1.221 and 2.1.222 changelog entries, current Skills/settings documentation, and local `2.1.222` help show real lifecycle and command drift: `/fork` and worktree isolation changed, and the executable exposes an `import` operator command plus a value-taking `--autocompact` option absent from the adapter table. Those facts justify neither certifying 2.1.222 nor rejecting it solely because its banner changed. They do require retaining and extending the independently enforced command/lifecycle boundary.

## Decision

A mutating Codex or Claude session SHALL attempt one shell-free, memory-bounded `--version` observation before SkillMount state access. An exact last-tested banner is silent; a different, malformed, nonzero, oversized, non-UTF-8, or unavailable result emits one bounded warning that identifies the last-tested evidence and MUST NOT by itself prevent launch or alter child-versus-cleanup exit precedence. The observation is ephemeral and MUST NOT enter a plan, lock key, journal, ownership record, or cleanup decision. `inspect` and `--dry-run` remain process-free and describe the last-tested policy without probing.

Only hard launch invariants repeat after lock stabilization and immediately before spawn. Codex retains its managed-configuration checks and selected plugin-name revalidation. Claude retains its environment, discovery-changing, detached, relocated, and non-session controls; the directly observed `import` command and `--autocompact` option shape join that fail-closed parser contract. Doctor reports an exact last-tested banner as `pass`, a different or unavailable banner as `unverified`, and keeps version uncertainty at exit `0` unless an independent executable, configuration, discovery, link, lock, or transaction failure requires data exit `65`.

All other decisions in ADRs 0023 and 0024 remain accepted: discovery roots and precedence, supported bounded commands, enterprise-policy handling, platform and unsafe boundaries, shell-free native arguments, foreground supervision, mutation ordering, and ownership-verified cleanup.

## Alternatives

- Keep exact versions as launch allowlists. Rejected because the banner is not an ownership or lifecycle capability, forces a SkillMount release for every Agent patch, and still cannot prove that an executable was not replaced after the final pathname probe.
- Remove version observation. Rejected because operators would lose the local evidence needed to interpret warnings and contribute a compatibility record.
- Accept release ranges. Rejected because the observed Claude patch releases changed lifecycle and command surfaces; a range would certify unexecuted behavior.
- Repeat advisory probes at every old boundary. Rejected because repeated evidence neither authorizes mutation nor binds the pathname to `spawn`, while adding latency and duplicate failure opportunities.
- Treat a successful child exit as compatibility evidence. Rejected because process success does not prove that the intended Skill won discovery or remained visible for the logical session.

## Consequences

- Operators may launch a newer or unobservable Agent after one explicit `unverified` warning. They must consult `docs/compatibility.md` and run the opt-in live smoke before recording compatibility.
- An Agent executable may change while SkillMount waits for locks without a second banner observation. The canonical path is still launched shell-free; actual spawn, child, and cleanup failures retain their existing authority and precedence.
- Unknown releases can add a detached, relocated, discovery-changing, or operator surface that the last-tested parser does not know. This residual lifecycle risk is disclosed rather than converted into a false compatibility claim. Each evidence review must update hard invariants when a concrete new surface is observed, as this change does for Claude `import` and `--autocompact`.
- No dependency, unsafe allowance, supported host, link primitive, permission mode, transaction schema, or filesystem ownership rule changes.

## Verification

- Shared version-observation unit tests cover exact and different banners, nonzero status, spawn failure, oversized output, and invalid UTF-8 with bounded rendering.
- `tests/codex_session.rs` and `tests/claude_session.rs` prove one observation, warning-and-continue behavior, child-status precedence, inherited streams, `--keep-mounts`, and normal ownership-verified cleanup.
- Claude adapter tests prove that `--autocompact` consumes its value before command classification and that `import` is rejected before state access.
- `tests/read_only.rs` proves `inspect` and Agent `--dry-run` launch no version or child process and create no state.
- `tests/transaction.rs` adds post-apply hard-invariant failure injection, preserving no-child and matching-evidence cleanup guarantees without treating version drift as a transaction precondition.
- `docs/compatibility.md` records dated banner observations separately from authenticated Agent, platform, and link evidence; unexecuted cells remain `unverified`.
