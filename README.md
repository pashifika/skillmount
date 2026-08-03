# SkillMount

SkillMount is a pre-release Rust wrapper that makes external Agent Skills visible for one Codex
CLI or Claude Code CLI session. The package builds `asm` as its primary executable and
`skillmount` as a behaviorally identical fallback name.

SkillMount resolves an ordered Skill catalog, creates only session-owned directory links, starts
the selected agent directly without a shell, and removes its verified entries after the managed
process domain exits. It does not install agents, manage authentication, change agent permission
modes, request elevation, or copy and rewrite Skills.

## Build locally

The repository currently targets Rust 1.85.0 or newer and has no published package contract yet.

```text
cargo build --locked --release --bins
target/release/asm --help
target/release/skillmount --version
```

Both binaries delegate to the same implementation. Examples below use `asm`.

## Quick start

A `--skills-dir` value can be one direct Skill directory or a catalog whose immediate child
directories are Skills. Repeat the option to overlay sources; the rightmost occurrence wins each
logical name.

Inspect both adapters without creating links, state directories, locks, journals, or child
processes:

```text
asm inspect --skills-dir path/to/base-skills --skills-dir path/to/team-skills
```

Preview the exact Codex plan and forwarded arguments:

```text
asm codex \
  --skills-dir path/to/base-skills \
  --skills-dir path/to/team-skills \
  --dry-run --verbose \
  -- exec "Use the selected Skill"
```

Start a Codex session:

```text
asm codex \
  --skills-dir path/to/base-skills \
  --skills-dir path/to/team-skills \
  -- exec "Use the selected Skill"
```

Start a Claude Code session. SkillMount creates an isolated staging root and injects it through
Claude's supported `--add-dir` discovery path; it does not edit the project:

```text
asm claude \
  --skills-dir path/to/base-skills \
  --skills-dir path/to/team-skills \
  -- -p "Use the selected Skill"
```

Arguments after the standalone `--` are passed as distinct platform-native values. They are never
joined into a shell command. Use `--agent-bin path/to/codex` or `--agent-bin path/to/claude` to pin
an executable; otherwise the agent is resolved through `PATH`.

## Catalog and conflict rules

The catalog is a deterministic rightmost-wins overlay. SkillMount retains every displaced origin
for verbose diagnostics, but validates only the selected winner. An invalid winner fails; an older
valid candidate is never used as a fallback. Repeating the same canonical source is recorded as a
repeat and does not count as a logical override.

Agent discovery is inspected before mutation. A project-owned Skill or incompatible destination is
never replaced. The default `--conflict error` stops with the existing and selected paths;
`--conflict skip` preserves the existing entry and omits that selected mount without revealing a
shadowed source. `--validation none` relaxes metadata checks only—safe names, regular Skill
directories, containment, and destination-cycle checks remain mandatory.

Selected Skills are trusted user code. Review their contents and provenance before making them
visible to an agent. SkillMount validates catalog structure, not the safety or intent of Skill
instructions, scripts, or bundled resources.

## Session lifecycle and recovery

A mutating session performs these observable phases:

1. Validate the agent executable, version, configuration, catalog, and discovery model.
2. Acquire every logical and physical resource lock and reconcile eligible stale transactions.
3. Persist a write-ahead journal before each destination mutation.
4. Create and atomically place session-owned directory links without replacing anything.
5. Launch the agent directly with inherited standard streams.
6. After process-domain death is proved, remove entries in reverse order only when live kind,
   target, platform identity, and directory contents still match the journal.

Successful ordinary cleanup is silent unless `--verbose` is requested. `--keep-mounts` reaches a
terminal kept state instead. A crash before child exposure is recovered under the recorded locks;
a journal that reached `supervising` is quarantined because wrapper death does not prove that an
agent descendant exited.

Diagnose the environment without modifying project or SkillMount state:

```text
asm doctor --project-root path/to/project
asm doctor --project-root path/to/project \
  --codex-bin path/to/codex --claude-bin path/to/claude
```

Doctor reports pass, warning, failure, and unverified findings for pinned agent versions,
discovery layouts and link chains, visible conflicts, advisory locks, journals, and isolated link
capability probes. Probe entries are created only in a unique owner-restricted temporary directory
and never removed recursively. Links and directories require matching platform identity; the
create-new source sentinel requires its regular-file kind and transaction-unique recorded bytes.
Retained residue is always named.

Version capture starts the explicit or `PATH`-resolved agent directly with only `--version` and no
shell. Treat that executable as trusted code: SkillMount does not change project state during its
own checks, but it is not a sandbox for side effects an external binary might implement.

After confirming that no related agent process or descendant is using the mounts, explicitly
clean one canonical project or every validated SkillMount journal:

```text
asm cleanup --project-root path/to/project
asm cleanup --all
```

Invoking cleanup is the operator's process-domain-death assertion for quarantined transactions and
the release decision for kept transactions. Cleanup still takes every recorded lock and calls the
same evidence-checked removal path as automatic recovery. `--all` scans only SkillMount journal
files under the current user's application-state root; it never searches arbitrary paths for
similarly named entries. Active, corrupt, replaced, or non-empty entries are retained and reported.

## Permissions and compatibility

Skill discovery and agent sandbox access are separate. In particular, a Codex Skill link can be
discoverable while a bundled file outside the workspace remains unreadable. Configure read access
through Codex's own permission controls when appropriate. SkillMount never injects a broader
permission mode, changes authentication, requests UAC or `sudo`, or treats `--add-dir` as a Codex
permission workaround.

The implemented release targets are:

- Windows x86_64 and i686, including native junction support;
- macOS on Apple Silicon.

Adapters currently pin Codex CLI 0.146.0 and Claude Code 2.1.220. Exact dated observations and
unverified live-agent gaps are maintained in [`docs/compatibility.md`](docs/compatibility.md).
Windows `--link-mode auto` may fall back to a junction when symlink creation is denied. Until the
matrix contains passing real-agent junction evidence for the pinned release, that fallback emits a
compatibility warning. Use `--link-mode symlink` to fail instead of falling back; SkillMount never
elevates itself.

The manual `Live agent smoke` workflow installs the pinned agents, runs three-source ordered
overlay discovery on macOS symlinks and Windows x64/x86 junctions, and uploads versioned evidence.
It requires external credentials, is not a pull-request gate, and never turns an absent run into a
compatibility claim. Windows jobs resolve native `.exe` files explicitly; command shims that would
need `cmd.exe` are not accepted.

## Development verification

Required deterministic checks use fake agents and isolated state:

```text
SKILLMOUNT_REQUIRE_LINKS=1 cargo test --locked --all-targets
cargo test --locked --all-features
cargo clippy --locked --all-targets --all-features -- -D warnings
cargo fmt --all -- --check
env RUSTDOCFLAGS="-D warnings" cargo doc --locked --no-deps --all-features
cargo deny --locked check
```

See [`docs/architecture.md`](docs/architecture.md) for mutation ordering, module boundaries, and
implementation status, and [`CONTRIBUTING.md`](CONTRIBUTING.md) for the branch and pull-request
workflow.
