# ADR 0035: Isolate per-Agent inspect failures and model OMP provider suppression

- **Status:** Accepted
- **Date:** 2026-08-07
- **Supersedes:** _none_

## Context

Two defects found while reviewing the merged OMP work (PR #41, PR #42) each break a rule the
baseline already states, and neither can be fixed without changing something the baseline records.

`asm inspect` defaults to every registered Agent. [ADR 0034](0034-pin-the-omp-session-discovery-contract.md)
gives the OMP adapter an environment gate inside `inspect_discovery` that refuses `OMP_PROFILE`,
`PI_PROFILE`, and `PI_CONFIG_FILES`, because those relocate every OMP root and would make the
description name a namespace no child reads. The inspect loop propagated that refusal with `?`, so
adding a third Agent silently made an OMP-only environment variable blank the Codex and Claude
sections too. Reproduced against the built binary: with a redirected `HOME` and a clean
environment, `asm inspect --skills-dir <dir>` exits `0` and reports three Agents; with
`OMP_PROFILE=work` exported it prints one error line, reports nothing, and exits `64`, while
`--agent codex` still exits `0`. Before OMP joined the default selection no OMP configuration could
reach the command at all. `doctor` already had the opposite rule recorded at
`src/operator/doctor.rs:79-81` — "one Agent's failure must not suppress another Agent's".

Separately, the recorded 17.2.9 contract omits `disabledProviders`. It is a real top-level OMP
setting (`packages/coding-agent/src/config/settings-schema.ts:529`, array, default empty) that
`initializeWithSettings` loads into the capability registry before any Skill root is scanned
(`capability/index.ts:285-289`), after which `filterProviders` removes every provider whose id it
names (`capability/index.ts:239`). Skill discovery passes no provider allow-list
(`extensibility/skills.ts:172`), so that filter is the only provider gate. The native provider's id
is `native` (`discovery/builtin.ts:39`) and it is the sole provider serving
`<launch-cwd>/.omp/skills`. The key is path-scoped (`config/settings.ts:154`) and merges from
project-owned layers, so a repository can set it. A merged `disabledProviders: ["native"]`
therefore made every existing visibility check pass while the mount became unreadable — the silent
success the design exists to prevent.

## Decision

1. **Inspect isolates per-Agent failures.** `asm inspect` renders every Agent section that
   succeeded, reports each refusal as a named warning, and exits with the first refusal's category.
   When no section is reportable the refusal remains the whole result and keeps its own message and
   exit category, so a single-Agent selection is unchanged.
2. **`disabledProviders` is part of the modelled namespace.** `settings::SkillSettings` projects the
   bounded top-level list, `source_enabled` returns false for a listed provider ahead of every
   per-level toggle, and `verify_selected_visibility` refuses a session whose `native` provider the
   list removes, naming that setting rather than `enablePiProject`. `custom` is not a provider id,
   so the list can never reach a `skills.customDirectories` entry.
3. **A root's occupancy is separate from its Skills, and an entry OMP skips is skipped, not fatal.**
   Every immediate child of a Skill root is recorded as a destination occupant under its on-disk
   name, before OMP's dot-name, entry-kind, `SKILL.md`, `enabled`, and description filters — the
   rule `agent::inspect_scope` already applies for Codex and Claude. Conversely, every condition
   under which OMP's own `readFile` returns null — missing, dangling, non-regular, unreadable, or
   empty `SKILL.md` — is a silent skip rather than a refusal of the whole run. Only a file OMP
   would load but this release cannot model stays fatal: oversize, not UTF-8, or a non-Unicode
   containing directory name.
4. **The project extension-package anchor is the launch CWD.** `<launch-cwd>/.omp` counts as
   present during the anchor walk even before it exists, because every OMP session creates it
   before the child starts.
5. **The spawn boundary reports drift as transient.** A refusal raised by re-running the
   selected-visibility gate after apply is re-reported as `AppError::Temporary`, matching the two
   sibling rechecks. The same refusal at plan time stays a data error.

## Alternatives

- **Keep propagating the first inspect failure.** Rejected: it makes every Agent's report hostage to
  the strictest Agent's preconditions, and the same patch had already rejected that shape for
  `doctor`.
- **Exit `0` when some Agent refused.** Rejected: the refusal is a real precondition failure and a
  script that reads only the exit status must still see it.
- **Drop OMP from the default `inspect` selection.** Rejected: it would hide the Agent an operator
  most needs to inspect, and the same refusal would still blank a deliberate `--agent all`.
- **Treat `disabledProviders` as out of scope because an operator sets it deliberately.** Rejected:
  it is path-scoped and merges from project-owned settings layers, so it is not only an operator's
  own choice, and the failure mode is a silently ignored mount rather than a visible error.

## Consequences

- `asm inspect` can now exit non-zero while still writing a report to stdout. A caller that treated
  any non-zero status as "no output" must read stdout as well.
- The refusal text moves to stderr as `<Agent> inspection was skipped: <reason>` whenever another
  Agent reported. The single-Agent wording is unchanged.
- The recorded 17.2.9 contract and ADR 0034's namespace rule now include `disabledProviders`. A
  future OMP release that renames or rescopes that key needs a new source review, exactly like the
  other settings keys.
- Modelling `disabledProviders` also removes a spurious-conflict source: a provider an operator
  disabled no longer contributes occupants to the conflict inventory, so `--conflict=error` stops
  failing sessions OMP would have satisfied.

## Verification

- `tests/read_only.rs::one_agents_inspect_refusal_keeps_the_other_agents_reports` fails if a
  refusing Agent discards another Agent's section, if the refusal is not named, or if the exit
  category is lost.
- `src/agent/omp/settings.rs::disabled_providers_outranks_every_source_toggle`,
  `::disabled_providers_is_read_from_the_merged_top_level_key`, and
  `::a_custom_directory_is_not_gated_by_a_source_toggle` fail if the list stops outranking the
  per-level toggles, stops being read from the merged tree, or starts reaching a custom directory.
- The upstream evidence for both decisions is recorded at
  `rasen/changes/support-omp-sessions/evidence/omp-17.2.9-contract.md`.
