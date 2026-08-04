# SkillMount architecture baseline

This document is the tracked current-state architecture baseline for SkillMount. It records the
product boundary, execution flow, module responsibilities, dependency and mutation boundaries,
cross-module safety rules, supported targets, and implementation status.

It is deliberately a repository baseline rather than a complete product specification. Detailed
state tables, failure scenarios, algorithms, and platform call contracts stay in tests, focused
architecture decision records (ADRs), or the code closest to the behavior. A behavior appears here
as current only after it exists in the tracked source and can be traced to repository evidence.

Status: static shell-completion generation, catalog resolution, discovery inspection, read-only
planning, cross-platform link primitives, resource locking, durable transactions, cleanup, stale
recovery, the generic child process supervisor, the Codex and Claude session adapters, operator
diagnosis and explicit recovery, the repository evidence workflow, versioned release packaging
and publication, and the Homebrew and Chocolatey package-channel contract are implemented.
Executed live-agent compatibility certification, external package-channel publication, and the
remaining transaction-lifetime hardening named under [Reserved work](#reserved-work) are not
implemented.

## Product definition

SkillMount is a Rust wrapper CLI that makes Agent Skills stored in external directories visible for
the intended lifetime of a Codex CLI or Claude Code CLI session. It resolves an ordered catalog,
inspects the discovery scopes implemented by the selected adapter, produces a deterministic mount
or preservation outcome for each selected Skill, and removes only entries that still match the
transaction's recorded ownership evidence.

One package installs two behaviorally identical binaries:

- `asm`, the primary command used by documentation;
- `skillmount`, a fallback name that delegates to the same `skillmount::run_from` entry point.

The `test-fixtures` feature additionally exposes fake-agent and supervisor-harness binary targets
for native integration tests. Default builds, release builds, and normal installation leave that
feature disabled and continue to produce only the two product binaries.

The initial release targets are:

- `i686-pc-windows-msvc`;
- `x86_64-pc-windows-msvc`;
- `aarch64-apple-darwin`.

Native completion acceptance covers Bash, Zsh, and Fish on Apple Silicon macOS and PowerShell on
both native Windows release architectures. Generation itself is portable and state-free, but a
shell is not added to the supported set without syntax and representative exact-candidate behavior
evidence on its claimed native host.

Windows mutation requires Windows 10 version 1709 or later so verified entries can be unlinked with
POSIX handle disposition even while unrelated inspect handles remain open. [ADR 0016](adr/0016-require-posix-handle-disposition-on-windows.md)
records that runtime baseline.

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
| `asm completions <bash\|zsh\|fish\|powershell>` | Rebuilds the shared CLI graph and writes one deterministic static script bound to `asm`; `skillmount completions` binds only `skillmount`. Generation stops before project, catalog, agent, state, lock, journal, recovery, or process work. Wrapper completion stops at a session `--` before the active cursor. Filesystem candidates are emitted as literal shell argument text, and executable hints admit only directories and executable files. |
| `asm codex --dry-run -- exec ...` or `-- review ...` | Produces the bounded Codex plan without directories, links, locks, journals, recovery, or child launch. |
| `asm claude --dry-run` | Produces the isolated Claude staging plan under the same read-only contract. |
| `asm claude --mount-mode=project` | Uses the project's `.claude/skills` namespace instead of isolated staging; `--dry-run` keeps that plan read-only. |
| `asm codex -- exec ...` or `-- review ...` without `--dry-run` | Resolves a shell-free executable, locks, recovers, replans, journals, applies, launches the bounded Codex command with the requested CWD and passthrough, then cleans up after the managed process domain is dead. Interactive TUI passthrough fails before state access. |
| `asm claude` without `--dry-run` | Requires exactly Claude Code 2.1.220, stages under a unique state-owned root, locks, recovers, replans, journals, applies, injects that root through one `--add-dir` pair, supervises the shell-free child, and cleans up after proven process-domain death. |
| Session with `--keep-mounts` | After a child reaches the supervision boundary, records terminal kept state instead of removing owned entries. A pre-spawn compatibility or supervision-intent failure overrides the request and removes every verified owned entry. |
| Session with `--no-recover` | Refuses when incomplete state requires reconciliation; otherwise continues through the normal mutating path. |
| Session encountering a free `supervising` journal | Refuses with category 75 and retains every recorded mount because wrapper-lock release does not prove child-domain death. |
| `asm doctor` | Resolves both pinned agents, inspects discovery links, visible-name conflicts, lock liveness, and journals, and runs isolated link-capability probes without SkillMount-owned mutation of project, agent, lock, or journal state. Version capture executes each selected trusted agent with literal `--version`; authenticated real-agent loading remains an `unverified` finding until live evidence exists, and any failed check exits with category 65. |
| `asm cleanup --project-root <path>` | Reconciles every structurally valid, non-completed journal for the canonical project after taking that journal's complete recorded lock set. A `supervising` or kept journal is eligible only because invoking this command is the operator's assertion that its process domain is dead or its retained mounts may be released. |
| `asm cleanup --all` | Applies the same journal-by-journal engine across the bounded state store. Corrupt state blocks all mutation; live locks retain their journals and produce category 75; an ownership or filesystem failure takes category 73 precedence. |

A mutating agent invocation returns the child's ordinary status after successful cleanup. A spawn
or supervision failure uses the shared typed exit mapping. Cleanup failure replaces child success
with category 73 and remains secondary evidence behind a failed child. Ordinary cleanup attempts
to release every owned entry, while `--keep-mounts` retains them intentionally after the child
boundary. A compatibility or supervision-intent failure before spawn forces verified cleanup;
anything cleanup cannot prove safe to remove remains reported and journal-backed.

Session stdout is exclusively the inherited child data stream. Wrapper-owned session summaries,
warnings, informational lines, errors, and cleanup diagnostics use stderr, so Codex JSONL, Claude
JSON, and ordinary pipelines receive no SkillMount prefix. Read-only plans, explicit operator
reports, and requested completion scripts remain stdout data. Completion generation treats
BrokenPipe as success and maps another output failure to category 70.
[ADR 0026](adr/0026-reserve-session-stdout-for-child-data.md) records the session stream boundary;
[ADR 0029](adr/0029-generate-static-completions-from-the-shared-cli-graph.md) records the static
completion boundary.

## Execution architecture

Completion generation branches at the CLI boundary and never enters path or product-state
resolution:

```text
argv -> CLI parsing + recognized product identity -> fresh shared clap graph
     -> fixed-shell static generator + opaque-passthrough guard -> stdout
```

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

The operator diagnosis path is independently read-only with respect to every user-visible or
durable SkillMount resource:

```text
argv -> canonical operator context -> pinned agent probes -> discovery inspection
     -> lock observation -> journal scan -> isolated temporary link probes -> typed findings
```

The executable-version steps are shell-free child launches, not filesystem observations. An
explicit or `PATH`-resolved agent is trusted external code; SkillMount supplies only `--version`
and does not claim to sandbox side effects implemented by that executable.

The temporary probe root is unique, owner-restricted, and outside the project and application
state. Probe cleanup removes links and now-empty directories only while their platform identities
still match; it removes the create-new sentinel pathname only while it remains a regular file with
the transaction-unique recorded bytes. There is no recursive deletion. Failure or incomplete
cleanup is itself a failed finding and reports the retained path.

Explicit cleanup reuses the transaction recovery engine rather than implementing a second removal
path:

```text
bounded journal scan -> reject the whole operation on corrupt state -> canonical scope filter
  -> per journal: claim missing keys into one shared complete lock set
  -> reload and verify immutable fields
  -> fail closed on disappearance or drift; otherwise classify the refreshed status
  -> dependency-ordered adopt -> shared verified cleanup
```

The command never trusts PID-looking holder text as liveness evidence and never treats a free lock
as proof that a child is dead. Invoking explicit cleanup supplies that otherwise unavailable
operator assertion for `supervising` journals. [ADR 0025](adr/0025-share-operator-inspection-and-recovery-engines.md)
records these public contracts. The scan is candidate discovery rather than mutation evidence:
automatic and explicit recovery both reload each journal under its recorded locks before acting,
as required by [ADR 0027](adr/0027-reload-recovery-journals-under-lock.md).

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
  -> journal active
  -> persist supervising intent, then shell-free child supervision
  -> after proven process-domain death: one reverse-order cleanup operation or terminal kept state
     after uncertain liveness: retain the supervising journal and every mount
```

Discovery can run before the lock because it independently identifies the resources that may be
mutated. The conflict-producing plan cannot: crash residue may look like an ordinary destination
conflict until recovery removes it. This ordering is the accepted decision in
[ADR 0012](adr/0012-acquire-locks-before-building-the-plan.md).

## Module responsibilities

| Module | Responsibility |
|---|---|
| `src/cli.rs` | Shared `clap` contract and conversion into typed commands. |
| `src/completion.rs` | Static Bash, Zsh, Fish, and PowerShell generation from the shared command graph, exact product-name binding, literal filesystem candidates, executable filtering, and cold-start opaque-passthrough guards. |
| `src/paths.rs` | Invocation CWD, launch CWD, project root, source occurrence, and executable resolution. |
| `src/catalog/` | No-follow source discovery, ordered overlay selection, and selected-winner validation. |
| `src/agent/` | Codex and Claude discovery inspection and declarative plan construction. |
| `src/mount/` | Destination conflict policy and deterministic, read-only mount actions. |
| `src/render.rs` | Read-only plans, normal/verbose session diagnostics, warnings, and reversible native-value rendering. |
| `src/lock/` | Logical/physical resource identities and sorted operating-system advisory locks. |
| `src/journal/` | Versioned, checksummed write-ahead ownership records and durable storage. |
| `src/transaction/` | Apply, rollback, ordinary cleanup, kept state, and stale recovery. |
| `src/process/` | Shell-free direct child launch, inherited streams, reusable platform interruption, liveness-gated cleanup coordination, structured status, and exit-policy mapping. |
| `src/link/` | Sealed platform boundary for no-follow inspection, link creation, no-replace placement, and verified entry removal. |
| `src/state.rs` | Computes state locations and, only after the mutation boundary, creates their requested final directories with platform-specific access restrictions. |
| `src/native.rs` | Lossless platform-native path encoding for journals and lock keys. |
| `src/domain.rs` | Shared values crossing the catalog, adapter, plan, lock, and transaction boundaries. |
| `src/error.rs` | Typed failures and stable sysexits-style wrapper categories. |
| `src/diagnostic.rs` | Non-fatal structured observations returned by lower layers. |
| `src/operator/` | Typed `doctor` observations, isolated capability probes, and explicit-cleanup presentation over shared lower-layer engines. |
| `src/app.rs` | The only top-level orchestration of read-only and mutating flows. |

The completion module depends on the shared CLI command factory, `clap_complete`, and an injected
`Write` target only. It cannot call the application path or any catalog, adapter, state, lock,
journal, transaction, link, or process module. `src/app.rs` dispatches the typed completion command
before constructing any of those contexts.

### Dependency and mutation boundaries

The catalog, adapters, and mount planner describe state; they do not change it. In particular, an
`AgentAdapter` observes agent-specific discovery and returns `DiscoverySnapshot` and `MountPlan`
values. Discovery classification reaches the sealed link backend through `mount::resolve` for
no-follow inspection, but the adapter never invokes a mutating platform operation.

The shared application layer owns ordering and policy. The transaction layer owns multi-step
mutation and durable ownership. The sealed link backend provides both read-only inspection and
narrow mutation primitives; it does not decide when a plan should be applied, recovered, or cleaned
up. There is no recursive removal operation in the link contract.

The process layer consumes a completed `LaunchPlan` and a single-use cleanup operation. It does not
select an agent executable, inject agent-specific arguments, apply a mount transaction, or decide
retention policy. `src/app.rs` composes that boundary for both implemented session adapters.

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
on undocumented duplicate precedence would make the selected Skill ambiguous. Each session
adapter must reconcile that model with the supported agent versions and cover every scope the child
will search before its launch boundary is enabled.

Adapters materialize those observations as one deterministic visible-name index that retains every
same-name declaration and its scope. Immediate occupancy of the namespace receiving new mounts is
separate evidence: recursive frontmatter identity answers what the child can select, while direct
path identity answers whether a destination can be created. Conflict policy consumes both indexes.

SkillMount validates every name it creates against the portable `SkillName` grammar. Existing
entries are stored as their raw platform-native name plus a comparison key because users and other
tools are not required to follow that grammar. Dropping such an entry would make conflict detection
unsound; [ADR 0010](adr/0010-discovery-entry-identity.md) records this decision.

## Agent discovery models

| Concern | Codex | Claude Code |
|---|---|---|
| Current modeled discovery | Codex 0.146.0 recursive regular-file `SKILL.md` discovery under project and ancestor `.agents/skills` and `.codex/skills`, `$HOME/.agents/skills`, deprecated `$CODEX_HOME/skills`, bundled `$CODEX_HOME/skills/.system`, and the platform administrator root; file links are ignored, logical identity uses the supported frontmatter parser and directory-name fallback, and all same-name declarations are retained | Claude Code 2.1.220 direct-entry discovery in the platform managed Skill root, project `.claude/skills` from launch CWD through project root, the effective `CLAUDE_CONFIG_DIR/skills` user scope, the selected destination, and user-supplied `--add-dir` scopes; descendant collisions below launch CWD are namespace-qualified and custom standalone Skills override bundled names |
| Compatibility | `.codex/skills` is a visible legacy conflict scope but never a placement candidate; an existing `.agents/skills -> .codex/skills` link is respected as operator configuration | No project compatibility store is created |
| Planned destination | Always `<project>/.agents/skills`; a missing path is planned as a regular directory chain | Default: unique state-root staging tree at `<session>/root/.claude/skills`; project mode: `<project>/.claude/skills` |
| Project mutation | Transaction-owned Skill links may be added only through `.agents/skills` | Project mode may add transaction-owned entries; default staging does not modify the project namespace |
| Launch integration | Implemented for bounded `exec` and `review` launches on exactly `codex-cli 0.146.0`, re-probed before state, after lock stabilization, and at the spawn boundary; interactive TUI is rejected because it can reload higher-precedence managed configuration after spawn; child `current_dir`, canonical explicit `CODEX_HOME`, injected native `-C` and session discovery overrides, validated passthrough, and no `--add-dir` | Implemented on exactly `2.1.220 (Claude Code)`, re-probed at the same three boundaries; default staging injects one `--add-dir <session>/root` pair and every mode injects a session-only selected-name `skillOverrides` object before unchanged validated passthrough |

Scopes that resolve to one terminal directory and use the same traversal policy are folded for
conflict evaluation while their visible aliases remain available for diagnostics. A bundled-system
scope is never folded with a root that follows directory links: sharing a terminal does not make
their inventories equivalent. Every visible root still contributes its logical lock resource, and
every canonical directory traversed through recursive discovery contributes a physical key. That
prevents two distinct links into one nested collection from escaping serialization and prevents a
conventional `.agents/skills -> .codex/skills` layout from being mistaken for two competing
namespaces without losing alias-level contention.

The Codex model was revalidated on 2026-08-03 against `codex-cli 0.146.0`, current official Skill
documentation, the pinned open-source loader, home resolver and bundled-Skill installer, and
black-box prompt discovery. A mutating session accepts exactly that reported release; dry-run and
inspection describe the pinned contract without launching an executable. This evidence replaced
the older direct-directory-name and two-entry backing models;
[ADR 0021](adr/0021-merge-codex-visible-names-and-mount-through-agents.md) records the proof, merged
index, placement rule, and deferred compatibility work. Repository, user, and administrator roots
follow bounded directory links. A linked bundled-system root is canonicalized and traversed, while
directory links encountered beneath that root are skipped. Before Codex can replace or create its
`.system` cache, the adapter reserves all six
embedded 0.146.0 logical names in the merged conflict index. No bundled-cache observation is
accepted as stable reuse or `--conflict=skip` evidence because Codex may delete or replace the
cache before loading; a collision there fails closed under either policy. On Windows the
administrator root is `%ProgramData%\OpenAI\Codex\skills`, resolved through the same Known Folder
API and `C:\ProgramData` fallback as Codex. Existing metadata uses Codex's
whitespace-delimited frontmatter envelope, scalar repair, single-line sanitization, and absent or
blank name fallback; a stricter local read bound fails the whole inventory closed rather than
omitting an uncertain Skill. The adapter also accounts conservatively for Codex's 4 MiB serialized
walk-response limit and fails closed instead of treating an inventory Codex would truncate as
complete. A later source audit showed that Codex cannot join child names onto an opaque root
`PathUri`. The adapter therefore rejects an existing canonical Windows discovery root, and the
canonical anchor of the planned preferred root, unless it has an ordinary file-URI representation.
The conservative UNC subset also rejects `localhost`, whose authority the pinned round trip
removes, and WHATWG numeric-host or IPv6 spellings that can normalize to another path.
It also rejects a non-Unicode directory-entry name on every
platform because Codex converts that name lossily before joining and can abort or incompletely
traverse the root. Accepted Windows paths are charged under the ordinary file-URI bound. Codex may
qualify a Skill name from the nearest valid `.codex-plugin`,
`.claude-plugin`, or `.cursor-plugin` manifest above its canonical source. Before planning and
again immediately before spawn, the adapter rejects any selected source for which that lookup
would produce a plugin namespace; otherwise the injected portable base-name rule could miss the
mounted Skill. Each potential manifest is reopened with a post-open regular-file check and a
64 KiB local bound; an unreadable, replaced, special, or oversized first candidate fails closed
instead of being mistaken for Codex's malformed-first-file suppression. Existing namespaced Skills
remain conservatively indexed by their unqualified
frontmatter name. That can add a false conflict, but a colon-qualified Codex name cannot equal a
portable selected name, so it cannot hide one.

SkillMount injects native `-C <launch-cwd>`, `project_root_markers=[".git"]`, and one name-enable
rule for every selected, non-skipped Skill in a bounded `exec` or `review` launch. It does not edit
persistent configuration. Forwarded Codex `-C`,
`--cd`, `-c`, `--config`, `-p`, `--profile`, `--enable`, `--disable`, and
`--ignore-user-config` forms are rejected before SkillMount state is inspected because they could
replace that discovery contract. Remote sessions, interactive TUI launches, explicit
`resume`/`fork`, and service or operator subcommands are also rejected; bounded `exec`,
`exec review`, and root `review` sessions remain supported. Codex 0.146.0's TUI rereads legacy
managed layers during lifecycle transitions, and those layers outrank the injected flags; three
pre-spawn probes cannot prove that runtime discovery interval. Command-position parsing
distinguishes subcommands from prompt text and option values. Bare variadic `-i`/`--image` is
rejected because a later option can end its values and expose a nested command; attached
`-iVALUE`/`--image=VALUE` remains classifiable. Command-free `inspect` remains an inventory
operation and does not certify a launch command.
An explicit SkillMount
`--project-root` is accepted for either launched agent only when it equals the root inferred from
the launch CWD by the supported default marker model; any successfully followed `.git` entry is a
marker, matching both pinned loaders. For Codex, normal system, cloud-managed, user, and profile
layers cannot replace the injected marker value. A higher-precedence legacy managed file, or the
corresponding macOS MDM preference, is conservatively rejected before state access and rechecked at
both later compatibility boundaries. Full plugin-qualified display and duplicate modeling remains
uncertified; the selected source gate above prevents that deferred modeling from changing a
launched Skill's effective name.

Codex reads `CODEX_HOME` only when it is non-empty Unicode. An explicit value must already name a
directory and is canonicalized relative to SkillMount's invocation CWD; the canonical Unicode path
is then set explicitly for the child so `current_dir(launch_cwd)` cannot reinterpret a relative
override. An absent, empty, or non-Unicode value is not replaced; Codex ignores it in both
processes and uses the user-home default, which it does not require to exist before startup.

Codex's discovery walk omits a `SKILL.md` file link even when its terminal target is a readable
regular file. Existing scopes therefore ignore those entries, and Codex catalog validation rejects
a selected source with a file-linked or special `SKILL.md` before any lock or mount mutation.
Other adapters retain their separately validated contained-link behavior. Codex also retains the
adapter-required frontmatter name and description checks when optional metadata validation is set
to `none`, because the injected name-enable rules must address the same logical names the child
loads.

The Claude model was revalidated on 2026-08-03 against Claude Code 2.1.220, its official Skills,
settings, and configuration-directory documentation, the changelog, and native black-box probes.
The platform managed root, project direct-entry scopes from the launch CWD through the project
root, user Skills below the effective `CLAUDE_CONFIG_DIR`, the proposed staging root, and every
user-added directory participate in one conflict index. Relative configuration and `--add-dir`
values resolve from the child launch CWD, while drive-relative Windows `CLAUDE_CONFIG_DIR` values
fail as ambiguous. A foreign managed collision fails under both conflict policies; an exact-source
managed entry can be reused. Terminal aliases never erase the managed classification during
physical-scope deduplication. Bundled names are not conflicts because the pinned release gives
custom standalone Skills precedence.

Exact `--bare`, `--safe-mode`, and `--disable-slash-commands` tokens in option position, a truthy
inherited `CLAUDE_CODE_SAFE_MODE` or `CLAUDE_CODE_SIMPLE`, and user `--settings`,
`--managed-settings`, or `--setting-sources` passthrough fail before state access. Background,
worktree, and tmux controls also fail because they detach the logical child or relocate discovery
outside the supervised and inspected session. Every pinned service/operator subcommand also fails:
the first unconsumed positional token selects a command even after standalone `--`, and those
commands do not share the certified foreground lifecycle. The bounded parser distinguishes option
values, the first command position, and later prompt text. The default launch prepends one
`--add-dir <session>/root` pair, and every launch injects a temporary `skillOverrides` map that
enables each selected catalog name before preserving validated user passthrough. It edits no
persistent setting or permission mode.
[ADR 0024](adr/0024-pin-the-claude-session-discovery-contract.md) records the corrected scope and
launch contract. Enterprise managed settings can intentionally suppress all non-plugin Skills,
outrank command-line settings, arrive asynchronously, and refresh during a child session.
SkillMount does not bypass that policy; managed-policy sessions, authenticated real-agent link
loading, and native Windows junction loading remain explicit compatibility-hardening gaps.

### Codex permission separation

Skill discovery and sandbox filesystem access are separate. A Skill directory linked to an external
source can be discoverable while a command run by Codex cannot read a bundled script, reference, or
asset outside the active permission boundary. SkillMount emits a typed warning for each selected
Skill whose canonical source lies outside the project and which the final plan exposes through a new
link or an exact-source reuse. A source omitted by conflict policy does not receive that warning.
SkillMount never edits persistent Codex configuration, changes the active profile, grants write
access, or injects `--add-dir`. Its session-only marker and selected-name overrides preserve the
discovery contract; they do not change permissions.

When access is actually required, the operator can add the narrow external root as read-only in a
Codex permission profile, subject to any managed organization policy. For example:

```toml
default_permissions = "skillmount-project"

[permissions.skillmount-project]
extends = ":workspace"

[permissions.skillmount-project.filesystem]
"/absolute/path/to/external/skills" = "read"
```

The syntax and platform enforcement rules are version-sensitive; use the current
[Codex permission-profile documentation](https://learn.chatgpt.com/docs/permissions) rather than
copying an environment-specific path from diagnostics.

## Locks, journals, and recovery

Resource descriptions separate a logical identity from an optional physical identity. The logical
key uses an existing canonical anchor plus a normalized suffix, so the first process creating a
missing directory and a later process observing it contend on the same key. A physical identity
makes aliases and worktrees that reach one existing directory contend as well.

Advisory locks are acquired in sorted key order. A lock file is not liveness evidence because it
survives process death; only the operating-system lock is. Human-readable holder information lives
in a sidecar so it remains readable while Windows holds a mandatory byte-range lock.

A transaction persists its journal before each planned destination mutation can become externally
visible. The journal distinguishes intent, staged identity, final placement, active use, child
supervision, cleanup, kept state, and failure. Its path codec round-trips arbitrary Unix bytes and Windows UTF-16,
including unpaired surrogates, rather than passing ownership evidence through UTF-8.

Apply rechecks every planned precondition and uses evidence-bearing, atomic same-filesystem
no-replace placement. Successful placement returns identity for the object established at the final
path before the journal advances to `applied`; a visible mismatch remains journal-backed residue.
Rollback and ordinary cleanup share the same reverse-order removal path. Windows derives
attributes, strongest identity, and reparse data from one no-follow handle and retains that handle
through rename or disposition. Kind and target are eligibility checks at the Windows handle
boundary; retained identity is the authority for later object-bound mutation because attribute-only
access is exempt from Windows share-mode enforcement. Disposition never traverses a reparse target.
Unix performs final no-follow verification before pathname mutation
while the application holds all cooperating-session locks; those advisory state-file locks do not
exclude another program, so ADR 0014 records the residual final pathname race. Path-based link and
directory creation returns no object capability on the supported APIs. The first no-follow
observation establishes evidence for later operations but cannot prove continuity from the create
call; ADR 0015 records that residual window. Failure before initial evidence is reported and
retained rather than followed by unchecked pathname rollback. Recovery eligibility comes from the
held lock set before child exposure, never a recorded PID: PIDs are reusable and cannot authorize
cleanup. Immediately before an agent spawn attempt, the journal enters `supervising`. If a later
invocation finds that journal with free locks, it cannot infer that the child process domain is
empty; automatic recovery quarantines the journal and mounts and blocks mutation with exit category
75. `asm cleanup` may release it only after the operator has asserted that its process domain is
dead and the command has acquired its complete recorded lock set. Read-only lock observations use
the operating-system lock alone; holder sidecars are diagnostic text and never authority.
Unreadable, corrupt, or future-schema journals block both automatic and explicit mutation and are
retained for operator inspection. A current-schema journal with no resource lock is likewise
rejected because no mutating session can produce it. [ADR 0022](adr/0022-quarantine-supervising-journals.md) records
the post-launch recovery boundary, and
[ADR 0025](adr/0025-share-operator-inspection-and-recovery-engines.md) records the explicit operator
decision layered on it.

## Platform and unsafe boundary

The crate sets `unsafe_code = "deny"`. Exactly six modules may opt in:

- `src/agent/codex/macos_ffi.rs`;
- `src/paths/windows_ffi.rs`;
- `src/link/unix_ffi.rs`;
- `src/link/windows_ffi.rs`;
- `src/process/unix_ffi.rs`;
- `src/process/windows_ffi.rs`.

The Codex macOS FFI module synchronizes the application preference domain and asks Core Foundation
whether the exact managed configuration preference Codex reads has a value; it never decodes or
exposes the property-list object. The paths Windows FFI module resolves `FOLDERID_Profile`,
`FOLDERID_ProgramData`, and `FOLDERID_ProgramFiles`, copies the returned UTF-16 path, and releases
its COM task allocation. The two `src/link/` modules wrap filesystem operations that have no safe
standard-library equivalent, including atomic no-replace placement, Windows reparse-point
observation, handle rename, and handle disposition. The process FFI modules wrap process-lifetime
Unix signal registration and Windows console-handler and Job Object operations. Each unsafe block has a
`SAFETY` justification, raw platform types do not cross its module boundary, and event storage,
process policy, and reparse decoding stay in safe Rust.
[ADR 0011](adr/0011-scoped-unsafe-for-platform-link-backends.md) records why `deny` with an audited
scope replaced crate-wide `forbid`; [ADR 0019](adr/0019-supervise-process-domains-through-reusable-native-dispatchers.md)
records the two process boundaries;
[ADR 0023](adr/0023-pin-the-codex-session-discovery-contract.md) records the fifth and sixth
boundaries and the pinned Codex child overrides;
[ADR 0024](adr/0024-pin-the-claude-session-discovery-contract.md) extends the Windows Known Folder
allowlist for Claude's managed Skill root.

Paths and forwarded arguments remain `PathBuf` and `OsString` through every public seam. They are
never joined into a shell command or converted lossily for policy, journal, lock, or ownership
decisions. Diagnostics may render a reversible representation only at the output boundary.

macOS uses directory symbolic links and `renameatx_np(RENAME_EXCL)`. Windows prefers a directory
symbolic link and falls back to a junction only for `ERROR_PRIVILEGE_NOT_HELD` in automatic mode;
that fallback emits a pinned-version compatibility warning until native real-agent evidence has
verified junction loading. Explicit symbolic-link mode fails instead of weakening the request, and
SkillMount never seeks elevation.
placement uses `SetFileInformationByHandle(FileRenameInfo)` with replacement disabled, and removal
uses `FileDispositionInfoEx` with delete and POSIX-semantics flags on a verified handle that excludes
ordinary write and delete access. Attribute-only reparse mutation can still occur without changing
that object's identity; the removal handle closes before the backend confirms that its recorded
identity is no longer visible. The variable-length rename layout is compiled and tested on both
Windows x86 and x64. The Linux test branch uses `renameat2(RENAME_NOREPLACE)` but does not establish
Linux product support.

Child launch always constructs `Command` directly from the platform-native executable, CWD, and
the injected-then-passthrough `OsString` arrays. All three streams use `Stdio::inherit()`. On Unix,
a process-lifetime handler snapshots a generation-tagged token at callback entry and records
occurrences in an atomic ledger. A leased session remains armed until the platform topology can
expose a child exactly once; shared Unix and Windows sessions therefore return pre-exposure events
to default handling, while a dedicated Unix session may queue the sole delivery before spawn.
Interactive Unix children remain in SkillMount's foreground group and receive shared-group
`SIGINT` directly; non-interactive children use a dedicated process group for forwarding, force,
and liveness probing. The interactive managed domain ends at the direct child: foreground
descendants and `SIGINT` targeted only at SkillMount remain the explicit residual boundaries in
ADR 0019. Once a dedicated-group leader has been reaped, SkillMount never signals that reusable
numeric process-group identifier. It uses a bounded passive emptiness probe and defers cleanup if
identity-safe proof is unavailable. On Windows, a raw process-lifetime handler preserves Ctrl+C
versus Ctrl+Break identity, the child inherits the wrapper's console group, and a kill-on-close Job
Object contains ordinary descendants.
The private driver distinguishes running, proven-dead, and uncertain domains, retries force and
liveness checks within fixed bounds, and exposes a cleanup permit only after no child was spawned
or the managed domain is proven empty. Its lifecycle guard performs only a best-effort force and
nonblocking reap on drop. The cleanup callback can report success or structured failure; only the
supervisor can select deferred cleanup. [ADR 0019](adr/0019-supervise-process-domains-through-reusable-native-dispatchers.md)
records the native evidence, residual containment limits, and replacement of ADRs 0017 and 0018.

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
7. Windows placement verifies and mutates the same no-follow object handle. Windows removal checks
   kind, target, and identity, then treats the retained identity as object authority; it excludes
   ordinary write and delete access, uses POSIX disposition, closes that handle, and confirms the
   recorded identity is no longer visible before reporting success. Attribute-only metadata access
   does not transfer object authority. Unix pathname mutation performs the last available identity
   check under cooperating-session locks; visible uncertainty is retained and reported, and advisory
   locks are never described as excluding other programs.
8. Initial creation evidence begins at the first no-follow observation, not at a preceding
   status-only create call. Failure before that boundary retains the path without pathname rollback.
9. Link removal never recursively descends into a target directory.
10. Shared application and transaction code own policy and sequencing; agent adapters inspect and
   plan, while the sealed platform backend executes only the link primitives it is asked for.
11. Product behavior never edits Git state, escalates privileges, or weakens agent permissions.
12. Child launch never uses a shell string, never reorders or duplicates injected/passthrough
    arguments, and never replaces inherited standard streams with product-owned pipes. Session
    stdout is reserved for child data; every wrapper-owned session diagnostic uses stderr.
13. Exactly one orderly cleanup operation runs when no child was spawned or the managed process
    domain is proven dead. Uncertain liveness defers cleanup and preserves recovery evidence. A
    cleanup failure replaces only child success; otherwise it remains structured secondary
    evidence behind the primary child or process failure. Recovery never turns free wrapper locks
    into process-domain death proof for a `supervising` journal.
14. The process-lifetime event dispatcher preserves the first two handler occurrences for one
    active session, linearizes finalization against event recording, and returns inactive or
    finalizing events to platform default handling.
15. SkillMount-owned `doctor` inspection never mutates project discovery, lock files, holder
    sidecars, journals, or agent state. Capability probes are isolated under a unique
    owner-restricted temporary root. Link and directory removal requires matching recorded kind,
    target where applicable, and platform identity; sentinel removal requires its create-new
    regular-file kind and transaction-unique recorded bytes. The selected trusted agent executable
    is run with literal `--version`; its own behavior is outside this non-mutation guarantee.
16. Explicit cleanup scans only the bounded journal store, rejects the whole operation before
    mutation when any journal is corrupt, acquires each eligible journal's complete lock set, and
    reloads and validates that journal under those locks before using the same ownership-checked
    reverse-order cleanup as sessions and automatic recovery. Overlapping journals share one
    claimed lock set and clean descendant owners before shared helper-directory owners. A journal
    absent after its complete locks are held is unknown ownership and blocks mutation. A live lock
    always wins over holder text; an ownership mismatch is retained and reported.
17. `completions` accepts only the owned Bash, Zsh, Fish, and PowerShell values, binds output only
    to the recognized platform-native invoked product name, stops wrapper candidates after a `--`
    before the active cursor starting with the first completion, emits filesystem candidates as
    literal shell argument text, never falls back to unrelated files from constrained wrapper
    states, limits directory hints to directories and executable hints to directories and
    executable files, and performs no project, catalog, agent, state, lock, journal, recovery,
    link, or process work.

Tests enforce the observable parts of these rules. Local comments retain the narrower preconditions
needed to preserve them inside an implementation.

## Release and package distribution

Distribution starts at the immutable GitHub release described in [docs/releasing.md](releasing.md)
and, beginning with `v0.2.0`, continues into two package channels operated through
[docs/packaging.md](packaging.md).
[ADR 0030](adr/0030-publish-selectable-packages-through-isolated-post-release-channels.md) records
the channel decisions. The release asset set and the dual-binary archive layout are unchanged by
packaging: the channels consume the published release and never add, remove, or rewrite an asset.

The release-to-package flow chains from workflow completion rather than from a release event,
because publication with the repository `GITHUB_TOKEN` suppresses `release.published`:

```text
tag push -> Release workflow -> immutable GitHub release
  -> workflow_run: credential-free preflight revalidates tag, ancestry, Cargo version, release
     identity, the exact asset set, every checksum, and the dual-binary archive layout
  -> Formula/package generation + structural pair inspection
  -> native acceptance harnesses (Apple Silicon macOS, Windows x64/x86), still credential-free
  -> independent homebrew-publish and chocolatey-publish lanes -> per-entry status summary
```

The package workflow introduces new trust and dependency seams. A `workflow_run` workflow is
privileged, so `.github/workflows/package.yml` uses only default-branch workflow and package logic,
treats every triggering-run and release value as untrusted data until preflight independently
revalidates it, and never checks out, extracts, or executes triggering-tag code or downloaded
product binaries in a job holding an external credential. A tracked workflow policy checker
(`.github/scripts/package_workflow_policy.py`) enforces action pinning, cache absence, permission
and secret scoping, publish-lane independence, and the verification-only gate. Manual dispatch
defaults to verification-only, which exercises preflight, generation, inspection, and both native
acceptance harnesses while both publish jobs are unreachable.

The supported selectable installation targets are a package-manager selection layer over the
existing release targets, and each installs exactly one of the two product executables:

| Public identity | Platform | Installs | Model |
|---|---|---|---|
| `pashifika/tap/skillmount` | Apple Silicon macOS | `skillmount` | Source-built Homebrew Formula |
| `pashifika/tap/skillmount-asm` | Apple Silicon macOS | `asm` | Source-built Homebrew Formula |
| Chocolatey `skillmount` | Windows x64/x86 | `skillmount.exe` | Checksum-verified release archive |
| Chocolatey `skillmount-asm` | Windows x64/x86 | `asm.exe` | Checksum-verified release archive |

Both members of a channel pair pin the same immutable source or release identity, may be
co-installed, and own only their selected executable, shim, and command-specific completion files.
Because `src/cli.rs` resolves the product identity from `argv[0]` and rejects a renamed alias, no
package may install, symlink, or shim either executable under another name. Publication is
pair-aware but not atomic: an identical existing member is an idempotent success, only absent
members are created, and any member with mismatched immutable metadata blocks the pair fail-closed
for human review.

The channels also introduce external state and credential boundaries. The separately managed and
protected `pashifika/homebrew-tap` repository owns the published Formulae, their CI, and their
history; it is never nested in this repository, and automation only proposes paired pull requests
through a GitHub App token scoped to that repository. The Chocolatey Community Repository owns
package moderation and public listing per package ID. Each publisher runs behind its own protected
GitHub Environment — `homebrew` and `chocolatey` — holding only that channel's credential;
preflight, generation, and acceptance jobs hold no external credential.

Package-channel non-goals: Homebrew Core, casks, bottles, Linuxbrew, macOS Intel, Windows ARM64,
WinGet, Scoop, and crates.io are out of scope, as are signing, notarization, attestations, editing
shell or PowerShell profiles, and running SkillMount release binaries in credentialed jobs.

## Implementation status

### Implemented

- shared CLI parsing, path resolution, stable exit categories, and equivalent binary entry points;
- deterministic static Bash, Zsh, Fish, and PowerShell completion generation for both recognized
  product names, with graph-derived values and path hints, opaque-passthrough guards, stdout-only
  output, read-only regression coverage, and native shell acceptance;
- catalog discovery, overlay selection, selected-winner validation, and provenance;
- Codex and Claude discovery inspection and deterministic read-only planning;
- `inspect`, `--dry-run`, concise/verbose plan rendering, and read-only regression tests;
- normal session summaries on stderr, child-data-only session stdout, verbose
  scope/link/provenance and cleanup diagnostics, reversible recovery arguments, and retained-path
  reporting;
- Unix/macOS symbolic-link and Windows symbolic-link/junction backends;
- no-follow link-chain resolution, evidence-bearing atomic no-replace placement, Windows
  handle-bound mutation after initial observation, Unix ownership-checked pathname mutation under
  cooperative locks, and documented creation-to-observation residual scope;
- logical and physical resource locks, versioned journals, write-ahead apply, rollback, cleanup,
  terminal kept state, and stale recovery;
- generic shell-free child supervision with inherited streams, typed child/interrupt/cleanup
  outcomes, stable exit precedence, reusable native event dispatch, liveness-gated cleanup, Unix
  signal-group handling, Windows console identity and Job Object containment, and feature-gated
  native fake-agent coverage;
- complete bounded Codex `exec`/`review` session composition through executable preflight, locked
  reinspection, durable apply, pre-spawn supervising intent, fake-child acceptance, liveness-gated
  cleanup, quarantined uncertain journals, and child/cleanup exit precedence;
- complete Claude 2.1.220 session composition through isolated staging, ancestor/user/add-dir
  preflight, exact passthrough injection, fake-child acceptance, concurrent roots, and the same
  liveness-gated cleanup and exit precedence;
- `doctor` with typed pinned-version, discovery, lock, journal, conflict, and isolated
  link-capability findings, plus read-only mutation regression tests;
- project-scoped and bounded all-state explicit cleanup through shared recovery and ownership
  engines, including active, corrupt, supervising, kept, replaced, missing, and mixed outcomes;
- operator quick-start, lifecycle, recovery, safety, compatibility, and manual smoke-test
  documentation, plus a dispatch-only native real-agent evidence workflow with integrity-locked
  agent packages, provider-scoped credentials, redacted artifacts, and process-tree timeouts;
- stable-tag and read-only manual release preflight, fixed native target builds, deterministic
  dual-binary archives, complete-set SHA-256 verification, and least-privilege marker-owned draft
  publication with main-ancestry and workflow-tree parity rechecks
  ([ADR 0028](adr/0028-require-workflow-tree-parity-for-github-token-releases.md)), plus
  immutable-action policy tests and reviewed tag-protection/runbook material;
- the package-channel publication contract for Homebrew and Chocolatey, implemented and CI-verified
  on this branch: shared credential-free release preflight and identity model, deterministic
  Formula and Chocolatey package generation with structural pair inspection, pair-aware fail-closed
  tap and Community Repository publishers, the isolated `workflow_run` package workflow with its
  tracked policy checker, native selected-only lifecycle acceptance harnesses for both channels,
  tap repository source material, and the packaging runbook
  ([ADR 0030](adr/0030-publish-selectable-packages-through-isolated-post-release-channels.md));
- crash-boundary, concurrency, path-encoding, ownership, and native platform test coverage.

### Reserved work

- compatibility work to add any Codex release beyond the exact 0.146.0 contract or Claude release
  beyond exact 2.1.220, plus native Windows junction discovery, asynchronously delivered Claude
  managed-policy handling, and executed authenticated real-agent certification;
- lock-file reclamation;
- binding a public transaction's lifetime to the lock guard validated when it is opened or adopted;
- rejecting pre-existing links in application-state directory paths before creation or permission changes;
- external package-channel publication, reserved pending external account creation and the `v0.2.0`
  release: creation, protection, and registration of `pashifika/homebrew-tap`, the tap-scoped
  GitHub App installation, the Chocolatey account with both public package IDs and pair-eligibility
  confirmation, both protected environments with their credentials, and the four publicly resolved,
  clean-host-verified install commands;

Both session adapters use the supervisor in the product application path, but real-agent and
Windows-junction certification remain non-blocking compatibility evidence gaps rather than claims
of fake-agent coverage. The deterministic fake-agent and native filesystem suites remain the
release gate. The manual workflow records evidence but does not update the compatibility table
automatically. Lock files still accumulate for distinct logical and physical lock keys. Owner
sidecars are removed on ordinary release but may remain after a crash or failed removal; neither
file's presence blocks or proves a live session.

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
