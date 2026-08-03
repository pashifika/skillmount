# ADR 0024: Pin the Claude Session Discovery Contract

- **Status:** Accepted
- **Date:** 2026-08-03
- **Supersedes:** ADR 0023's two-identifier Windows Known Folder allowlist only

## Context

The planning-only Claude adapter assumed that inspecting only the explicit project root, reading
user Skills only from `~/.claude`, and resolving relative `--add-dir` values from SkillMount's
invocation directory described every Skill the child could see. Current Claude Code documentation
and black-box probes of the local 2.1.220 native executable disprove all three assumptions. Claude
loads project Skills from the launch directory and its ancestors through the repository root,
interprets added directories in the child launch context, and relocates its user Skill root when
`CLAUDE_CONFIG_DIR` is set.

The official [Skills documentation](https://code.claude.com/docs/en/skills),
[configuration-directory documentation](https://code.claude.com/docs/en/claude-directory), and
[2.1.220 changelog](https://raw.githubusercontent.com/anthropics/claude-code/main/CHANGELOG.md)
establish the bounded contract used here: `.claude/skills` under an added directory is loaded,
nested project collisions below the launch directory are qualified, and `--bare`, `--safe-mode`,
and `--disable-slash-commands` defeat normal Skill discovery. A local `claude --version` probe
reported `2.1.220 (Claude Code)`, and that release's `--help` fixed the option arities needed to
distinguish controls from values and prompt text.

Further probes overturned the original enterprise and settings assumptions. The 2.1.220 debug log
reported the macOS managed Skill root as
`/Library/Application Support/ClaudeCode/.claude/skills`; official settings documentation places
the corresponding Windows root below `C:\Program Files\ClaudeCode`. A custom `debug` Skill from an
added directory remained selected over Claude's bundled `debug`, so bundled names do not need a
synthetic conflict inventory in this pinned release. User `skillOverrides.debug = "off"` removed
that Skill from the model attachment, while a command-line `--settings` override restored it.

Adversarial review exposed five more counterexamples to the first implementation. A project entry
can be a terminal alias of the managed Skill root, but physical deduplication must not erase the
managed root's semantic precedence. `--setting-sources` can exclude the user or project source
whose existing Skill was accepted during planning, and inherited `CLAUDE_CODE_SIMPLE` selects the
same minimal mode as `--bare`. Finally, background execution returns before the logical child
session ends, while worktree and tmux launch controls can move discovery or relaunch outside the
inspected CWD and supervised process domain. Those controls invalidate cleanup and visibility
proofs rather than merely changing presentation.

The fifth counterexample is command dispatch. In 2.1.220, the first unconsumed positional token can
select a service or operator subcommand such as `agents`; a standalone `--` does not suppress that
dispatch. Such a command does not provide the foreground session lifecycle or discovery behavior
the adapter certified.

Enterprise policy is different. Both the official
[settings hierarchy](https://code.claude.com/docs/en/settings) and a local
`--managed-settings '{"strictPluginOnlyCustomization":["skills"]}'` probe show that managed
policy can suppress every user, project, and added-directory Skill and cannot be overridden by
ordinary command-line settings. Server-managed policy can arrive asynchronously at startup and
refresh hourly. SkillMount must not weaken that administrative policy, and this adapter cannot
prove its absence for the whole child lifetime.

## Decision

A mutating Claude launch SHALL accept exactly `2.1.220 (Claude Code)` and re-probe before state
access, after lock stabilization, and immediately before spawn. The adapter SHALL inspect direct
Skill entries in:

- the platform managed Skill root;
- project `.claude/skills` from the launch CWD through the inferred project root;
- the user `skills` directory below the effective `CLAUDE_CONFIG_DIR` or default `~/.claude`;
- the unique proposed staging scope; and
- every user `--add-dir` scope, resolving relative values from the launch CWD.

An explicit wrapper project root SHALL equal the root inferred from that same CWD. A foreign
managed Skill collision SHALL fail under both conflict policies because enterprise precedence means
`skip` cannot expose the selected source; an exact-source managed entry may be reused. Terminal
deduplication SHALL preserve the `ClaudeManaged` classification when a lower-precedence scope aliases
the same physical directory.

The effective `CLAUDE_CONFIG_DIR` SHALL pass through the shared fallible native path resolver.
Absolute and ordinary relative values retain Claude's launch-CWD semantics; drive-relative Windows
forms such as `C:config` SHALL fail with a usage error because their resolution depends on hidden
per-drive process state.

On Windows, `src/paths/windows_ffi.rs` MAY additionally resolve `FOLDERID_ProgramFiles` through its
existing audited `SHGetKnownFolderPath` boundary, copy the UTF-16 path into a safe `PathBuf`, and
release the COM task allocation before returning. Resolution failure SHALL use Claude Code's
documented `C:\Program Files` fallback. This supersedes only ADR 0023's requirement that the module
resolve exactly `FOLDERID_Profile` and `FOLDERID_ProgramData`; ADR 0023's unsafe-module allowlist,
allocation, type-boundary, and other Codex decisions remain in force.

The passthrough scan SHALL preserve platform-native arguments and consume only the pinned option
shapes needed to locate discovery controls. Option recognition SHALL stop at standalone `--`, but
the first unconsumed positional token on either side of that separator SHALL still be checked
against the pinned non-session subcommand set. The scan SHALL reject exact
`--bare`, `--safe-mode`, or `--disable-slash-commands` tokens in option position, a truthy inherited
`CLAUDE_CODE_SAFE_MODE` or `CLAUDE_CODE_SIMPLE`, and user `--settings`, `--managed-settings`, or
`--setting-sources` arguments before state creation. Settings controls could replace or exclude the
selected-name visibility contract after planning.

The scan SHALL also reject `--bg`, `--background`, `--worktree`, `-w`, and `--tmux`, including
supported attached-value forms, in option position. SkillMount does not claim staging lifetime,
root, or process-domain guarantees for a detached or relocated Claude session. Flag-shaped values
consumed by another pinned option remain opaque. Once a non-subcommand prompt occupies the first
positional slot, later positional tokens remain opaque. Every pinned service/operator subcommand
(`agents`, `auth`, `auto-mode`, `doctor`, `gateway`, `install`, `mcp`, `plugin`/`plugins`,
`project`, `setup-token`, `ultrareview`, and `update`/`upgrade`) SHALL fail before state access.

The launch plan SHALL prepend one native `--add-dir <session>/root` pair. It SHALL also pass a
session-only `--settings` JSON object that sets every selected logical Skill name to `on`, so
ordinary user and project visibility preferences cannot silently hide an accepted catalog key.
The user's validated passthrough remains byte-for-byte unchanged after those injected arguments.
The shared shell-free process supervisor and liveness-gated transaction cleanup own the child
lifecycle.

Descendant project scopes below the launch CWD SHALL not be treated as portable-name conflicts
while 2.1.220 qualifies their collisions. Bundled Skills SHALL not be indexed as conflicts while
the pinned release gives custom standalone Skills precedence. Enterprise-managed Skill files are
modeled, but sessions governed by managed settings that restrict custom Skills are outside this
adapter's supported contract. SkillMount SHALL preserve such policy rather than attempting to
override it; detection of asynchronously delivered server policy remains release-hardening work.

## Alternatives

- Inspect only the project root. Rejected because it misses ancestor scopes between a nested launch
  CWD and that root.
- Always inspect `~/.claude/skills`. Rejected because `CLAUDE_CONFIG_DIR` relocates that user scope,
  including when its value is relative to the child CWD.
- Ignore the managed Skill root as a non-goal. Rejected because a managed duplicate has highest
  precedence and can prevent the selected staged source from being visible.
- Read `%ProgramFiles%` directly. Rejected because the process environment is mutable and Windows
  exposes the machine installation base through the same Known Folder API already audited for
  profile and program-data discovery.
- Inspect every descendant scope. Rejected because 2.1.220 qualifies nested collisions; treating
  them as one portable key creates false conflicts without matching the child namespace.
- Preflight ordinary `skillOverrides` files. Rejected because settings are live-reloaded; a
  session-level CLI override provides the stable lower-tier result without editing persistent
  configuration.
- Override enterprise policy. Rejected because managed settings outrank command-line arguments and
  bypassing them would violate SkillMount's permission and policy boundary.
- Support a release range. Rejected because discovery, settings, and disabling behavior changed
  repeatedly within the 2.1 line; a range would borrow unverified semantics.

## Consequences

- Claude sessions cross the real child boundary and propagate the child status with the same
  cleanup precedence and supervising-journal quarantine as Codex.
- Default staging remains project-neutral and concurrent: every session owns a unique
  `<session>/root/.claude/skills` tree and grants only its enclosing root as an added directory.
- SkillMount changes no persistent Claude setting or permission mode. The generated
  `skillOverrides` object exists only in child argv and only names the selected catalog keys.
- Forwarded arguments remain native-value equivalent, but `--settings`, `--managed-settings`,
  `--setting-sources`, detached launch, worktree, tmux controls, and non-session subcommands are now
  rejected because their effects cannot coexist with the mounted visibility and lifetime
  guarantees.
- Enabled inherited `CLAUDE_CODE_SIMPLE` is rejected with the same pre-state behavior as safe mode,
  and drive-relative Windows `CLAUDE_CONFIG_DIR` values are rejected as ambiguous.
- Three version probes reduce executable-replacement races but do not bind the executable pathname
  atomically to spawn. Real Claude link loading, Windows junction loading, and asynchronous managed
  settings remain release-hardening evidence.
- The Windows FFI boundary gains one safe resolver over the same three raw operations and ownership
  protocol; no raw Windows type crosses the module boundary.

## Verification

- Local 2.1.220 black-box probes recorded ancestor/add-dir discovery, relative
  `CLAUDE_CONFIG_DIR`, the macOS managed path, bundled-name shadowing, ordinary
  `skillOverrides` restoration, `CLAUDE_CODE_SAFE_MODE`, and managed
  `strictPluginOnlyCustomization` suppression.
- `src/agent/claude.rs` unit tests cover exact disabling, settings-source, detachment, worktree,
  tmux, and non-session command controls; environment-flag semantics; option-value and command-slot
  disambiguation across standalone `--`; attached and variadic add-dir forms;
  Unicode-independent native values; and the exact release label.
- `src/agent/tests.rs` covers ancestor, relocated user, relative add-dir, and managed-scope
  conflicts; managed aliases with foreign and exact-source entries; user-scope error/skip behavior;
  case variants; deterministic staging; and selected-name settings injection.
- `tests/claude_session.rs` proves three probes, rightmost winners, effective argv/CWD/streams,
  active link visibility, pre-state rejection of discovery and lifecycle controls, child/cleanup
  precedence, project/user preservation, binary parity, and two overlapping children with distinct
  roots.
- Windows-target path tests reject drive-relative `CLAUDE_CONFIG_DIR` through the same native path
  boundary as wrapper arguments.
- `tests/transaction.rs` injects a conflict after preliminary discovery, exercises every durable
  recoverable boundary against Claude staging, and retains replaced or unprovable objects.
- Native macOS and Windows CI provide the supported link and path implementations. Authenticated
  real-agent, native Windows junction, and managed-policy sessions are not represented by the
  fake-agent contract.
