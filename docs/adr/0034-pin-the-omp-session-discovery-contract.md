# ADR 0034: Pin the OMP session discovery and launch contract

- **Status:** Accepted
- **Date:** 2026-08-06
- **Supersedes:** _none_

## Context

SkillMount wraps Codex CLI and Claude Code. Adding Oh My Pi (OMP) makes it a third session adapter,
and OMP's Skill namespace is materially larger than either existing model: nine registered providers
with numeric priorities, a five-layer settings stack that can disable or filter individual Skills, an
extension-package surface with its own lockfile, and a root command whose flags can change the
inspected root, profile, provider set, or process lifecycle after planning.

The contract was read from the tagged source, not documentation:
`https://github.com/can1357/oh-my-pi` at `v17.2.9`, commit
`f7f8e040ee04710414fbd775431091fa301b9786`. The banner the installed binary reports is `omp/17.2.9`.
The complete recorded contract, with `path:line` citations for every rule referenced below, is at
`rasen/changes/support-omp-sessions/evidence/omp-17.2.9-contract.md`.

Three facts from that review force decisions rather than merely informing them:

- OMP loads Skills once while constructing a session. It exposes no authenticated interface that
  makes an already-running process reload a temporary Skill root and later prove it released it.
- The highest-priority provider, `native` at priority 100, scans `<cwd>/.omp/skills` first and admits
  a symlinked entry as a first-class discovery entry
  (`packages/coding-agent/src/discovery/helpers.ts:418-420`). A directory link is therefore loadable
  without transforming any Skill.
- Several root flags silently move the ground the plan stands on. `--cwd` chdirs; the home escape
  chdirs into `/tmp` on its own when the launch cwd is the user home; `--profile` relocates the whole
  agent directory; `--config` and `PI_CONFIG_FILES` inject settings overlays; `--no-skills`,
  `--skills`, `-e`, `--hook`, `--no-extensions`, and `--plugin-dir` change the selected or discovered
  set.

## Decision

SkillMount supports OMP only as a new foreground session it launches itself, mounting into
`<launch-cwd>/.omp/skills`, and rejects before SkillMount state access any argument, environment
overlay, or operating mode whose resulting namespace or lifecycle cannot be proven at plan time.
Concretely, for the `omp` command in `src/agent/omp/`:

1. **Ownership.** `asm omp` resolves a shell-free OMP executable, mounts, launches one supervised
   foreground child with inherited standard streams, and cleans up only transaction-owned entries
   after the managed process domain is dead. Attaching to, signalling, or hot-reloading Skills into an
   OMP process SkillMount did not spawn is not supported.
2. **Destination.** `--mount-mode=auto` and `--mount-mode=project` select `<launch-cwd>/.omp/skills`;
   `--mount-mode=staging` is a usage error. Missing `.omp` and `skills` directories are ordinary
   transaction-owned directory actions; each selected Skill is a transaction-owned directory link
   unless an exact same-source link is safely reusable.
3. **Namespace.** The adapter reproduces the recorded 17.2.9 provider set, priorities, registration
   order, non-recursive `<root>/<entry>/SKILL.md` layout, description requirements, settings merge
   order, source toggles, and the filter order `disabledExtensions` then source toggle then
   `ignoredSkills` then `includeSkills`, as data plus bounded no-follow filesystem inspection. Every
   selected logical name is checked across that complete namespace before mutation. `error` fails on
   an existing different or unknown source; `skip` preserves the existing winner.
4. **No third-party execution.** The adapter never imports or runs plugin, extension, or hook code.
   Where a contribution cannot be proven from declarative manifests and on-disk state, the session
   fails as an unsupported environment instead of guessing.
5. **Rejected inputs.** Reject `--cwd`, `--profile`, `--alias`, `--config`; `--no-skills`,
   `--skills`, `--extension`/`-e`, `--hook`, `--no-extensions`, `--plugin-dir`; `--continue`/`-c`,
   `--resume`/`-r`/`--session`, `--fork`, `--from-claude`, `--from-codex`, `--export`; `--mode` with
   any of `rpc`, `rpc-ui`, `acp`; and every recognized OMP subcommand including `acp`. Reject the
   environment overlays `OMP_PROFILE`, `PI_PROFILE`, and `PI_CONFIG_FILES`. Require the operator's
   explicit `--allow-home` when the launch CWD is the user home, and never inject it. Forward
   `--auto-approve`, `--yolo`, and `--approval-mode` only when the operator supplied them.
6. **Version evidence.** `omp/17.2.9` is the adapter's dated last-tested banner supplied to the shared
   observer from ADR 0033. It is advisory: a different, malformed, or unavailable banner warns and
   never blocks, and version observation is not repeated after locks or before spawn.
7. **Journal.** `omp` is a new value in the journal's existing agent field. This is not a schema
   change; an older binary fails closed on the unknown value and retains the state. Downgrading
   below the OMP-capable release requires clearing active OMP journals with `asm cleanup` first.

## Alternatives

- **Attach to a running OMP process and hot-reload Skills.** Rejected: discovery is startup-scoped,
  and cleanup could not prove the process stopped reading the mount, so removal would be unsound.
- **Generate a `--config` overlay pointing `skills.customDirectories` at a session root, avoiding any
  project mount.** Rejected on three source facts. The transaction owns no atomic regular-file
  action, so the overlay could not be applied or removed with the same ownership evidence as a link.
  OMP's `#deepMerge` replaces arrays wholesale
  (`packages/coding-agent/src/config/settings.ts:2161-2183`), so an overlay cannot append to an
  operator's `customDirectories`. And reproducing `ignoredSkills`/`includeSkills` without exposing
  extra Skills would mean rewriting operator policy. `--config` is also on the rejected list, so
  accepting it from SkillMount while refusing it from the operator would be incoherent.
- **Mount into a user-global OMP scope.** Rejected: it broadens visibility and lock contention beyond
  the requested project session.
- **Inspect only `<launch-cwd>/.omp/skills`.** Rejected: custom directories and eight other providers
  can change the effective winner, so an apparently successful mount could be ignored or could shadow
  project-owned content.
- **Allow the root-changing flags and emulate their effect.** Rejected: the plan would target a
  different namespace from the child. Rejecting a small unstable surface keeps the model, prompt,
  tool, output-mode, and approval options available.
- **Promote Codex, Claude, and OMP provider traversal into one generic provider framework.** Rejected:
  the three models share no invariants beyond the primitives already shared, and a framework would add
  type erasure and compatibility obligations with no product requirement.

## Consequences

- Operators gain `asm omp` and `skillmount omp` for a new local foreground session only. Resume,
  import, export, named profiles, per-run config overlays, CLI-selected extensions, and every service
  or protocol mode remain unsupported, and each rejection names the conflicting token and the safe
  new-session alternative.
- Because the destination is the highest-priority provider scope, a mounted Skill wins over the
  operator's other sources by OMP's own precedence. Planning therefore checks the complete namespace
  first and never replaces a project-owned entry.
- Mounting inside an existing linked `.omp/skills` can temporarily change its canonical target, so
  visibility may extend to another project that intentionally shares that directory. Logical and
  physical locks, no-replace placement, and recorded identity protect the entries; diagnostics must
  show the canonical backing path.
- OMP requires a readable non-empty description for `native` provider discovery, so SkillMount keeps
  that as an OMP catalog requirement even when generic metadata validation is `none`; otherwise the
  child would silently drop the selected Skill.
- A present frontmatter name must equal the portable mount name. OMP itself accepts a mismatch and
  indexes by the frontmatter name, so without this requirement a mount could load under a name
  SkillMount never planned.
- OMP's provider and settings graph changes often. Only a new source review can certify a new model;
  version warnings route operators to `docs/compatibility.md` rather than implying coverage.
- OMP 17.2.9 publishes a 64-bit Windows binary but no 32-bit one, and no `SHA256SUMS.txt` for that
  exact release. Windows x86 OMP evidence is therefore permanently unavailable for this version, and
  every native OMP cell — including Windows x64 junction loading — stays `unverified` until an
  opt-in smoke run records it.
- External writers do not honour SkillMount locks. Lock stabilization plus a pre-spawn recheck of
  non-owned settings and provider evidence narrows the final read-to-start race but cannot remove it,
  and the adapter never compensates by overriding configuration. The pre-spawn recheck covers three
  distinct classes, because the non-owned comparison alone cannot see all of them: non-owned
  namespace drift through the non-owned evidence comparison, a configuration edit that would hide a
  selected Skill through a re-run visibility gate, and a retargeted destination through each owned
  entry still resolving to the source the plan recorded.
- A read-only run enforces the same launch invariants as a mutating one. `--dry-run` describes the
  session the mutating run would start, so an invariant that refuses the session refuses the
  description; otherwise the plan would describe a namespace no child would load.
- The `claude-plugins` provider root is whatever `installPath` an `installed_plugins.json` entry
  names, and one of the three registries OMP consults lives inside the project. OMP validates only
  that the value is a non-empty string, so a repository can name any absolute path that SkillMount
  then inspects, reports, and locks. SkillMount reproduces that domain rather than narrowing it,
  because narrowing would under-report a pre-existing Skill and let a mount silently shadow it; the
  value is lexically normalized so containment checks compare like with like, and no path outside the
  `<launch-cwd>/.omp` destination is ever mutated.
- Every comparison against the user home normalizes both operands the way OMP normalizes them -
  resolve, then realpath, then a case fold on Windows. Comparing a canonicalized launch CWD against
  the raw `HOME`/`USERPROFILE` value would leave the `--allow-home` consent gate inert on Windows,
  where canonicalization yields a verbatim path prefix, and defeated anywhere the home directory is
  reached through a symbolic link.
- Untrusted arrays and registries are bounded: the total provider-root count, the per-registry install
  path count, and each settings string array. OMP is unbounded there, but an untrusted document would
  otherwise decide how much work planning does. Crossing a bound fails closed with the same
  incomplete-inventory reason a too-large Skill root uses.

## Verification

- `tests/read_only.rs` fails if `inspect --agent omp` or `asm omp --dry-run` creates a directory,
  link, lock, journal, recovery change, version process, or child.
- `src/agent/omp/settings.rs` unit fixtures fail if settings merge order, array-replacement
  semantics, source toggles, or filter order diverge from the recorded contract, and
  `src/agent/omp/discovery.rs` unit fixtures cover the ancestor walk, tilde expansion, frontmatter
  identity, and destination occupancy. Provider order, priority, registration order, per-provider
  description requirements, entry layout, symlink admission, realpath dedup, and custom-directory
  override are pinned by integration fixtures in `tests/omp_session.rs`, which assert the rendered
  discovery-scope sequence rather than one function's return value.
- Passthrough and environment rejection tests fail if any listed token or variable reaches a child,
  or if a rejection creates project or SkillMount state; each asserts usage status `64`.
- `tests/transaction.rs` fails if an OMP session mutates outside its journal, replaces a non-owned
  entry, removes an entry without matching recorded evidence, or spawns a child after a
  spawn-boundary change. The `spawn-boundary` checkpoint stalls a real session in that window, and
  one case per recheck class — retargeted destination, hidden selection, non-owned drift — asserts
  no child was launched and the transaction was released.
- The static-derivability rule is enforced by
  `tests/omp_session.rs::no_declared_extension_plugin_or_hook_entry_point_is_ever_executed`, whose
  declared extension, plugin manifest, and project-owned registry hook all point at a script that
  writes a sentinel if it is ever spawned, sourced, or imported; the test fails if any sentinel
  appears.
- Live compatibility is not verified by any of the above. `docs/compatibility.md` records the OMP
  cells, and a cell stays `unverified` until the opt-in native smoke passes.
