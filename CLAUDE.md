# Repository Guidelines

## Authority and scope

`docs/architecture.md` is the tracked current-state architecture baseline and the source of truth
for product scope, module responsibilities, dependency and mutation boundaries, safety invariants,
supported targets, and implementation status. `CONTRIBUTING.md` defines branch and pull-request
workflow. Focused architecture decision records (ADRs) under `docs/adr/` explain replacements to
normative baseline decisions.

When implementation, configuration, or tests change the architecture, update the baseline in the
same product change. Follow the complete mandatory ADR triggers in `docs/adr/0000-template.md`; a
replacement of a normative baseline decision is one such trigger, while ordinary implementation of
an already recorded rule does not need another ADR. Source and tests provide the evidence for
current status, so resolve a mismatch rather than silently choosing one side.

When present, `rasen/` is a nested, separately managed planning repository. Rasen proposals, specs,
designs, tasks, and evidence describe planned deltas or history; they are not a substitute for
tracked product documentation. Never stage `rasen/` or `.rasen/` in the product repository, and
keep product and planning-store commits separate. Root-anchored product ignore rules guard these
machine-local paths, but agents must still stage product files explicitly. Machine-local
`local_docs/` material is proposal or historical input only.

## Product definition

SkillMount is a Rust wrapper CLI that makes external Agent Skills visible for the intended lifetime
of a Codex CLI or Claude Code CLI session. The package installs `asm` as the primary binary and
`skillmount` as a behaviorally identical fallback; both delegate to `skillmount::run_from`.

The catalog is a deterministic rightmost-wins overlay. SkillMount validates only the selected
winner, never falls back from an invalid winner, and never lets source precedence replace a
project-owned Skill. It treats selected Skills as trusted user code and does not transform them,
manage authentication, elevate privileges, or weaken agent permissions.

Read `docs/architecture.md` before changing cross-module behavior. Its implementation-status
section distinguishes working catalog/planning/transaction/process-supervision behavior from
reserved agent-launch integration, operator commands, ownership binding, and release work.

## Commands

```bash
SKILLMOUNT_REQUIRE_LINKS=1 cargo test --locked --all-targets
cargo test --locked --test read_only
cargo test --locked --test transaction
cargo test --locked --all-features --test process_supervision
cargo clippy --locked --all-targets --all-features -- -D warnings
cargo fmt --all -- --check
env RUSTDOCFLAGS="-D warnings" cargo doc --locked --no-deps --all-features
cargo deny --locked check
cargo run --locked --bin asm -- inspect --skills-dir path/to/skills
cargo run --locked --bin asm -- codex --skills-dir path/to/skills --dry-run --verbose -- exec "prompt"
```

`SKILLMOUNT_REQUIRE_LINKS=1` turns an unavailable directory-link fixture into a failure. A skipped
fixture otherwise reports test success, so use the guard whenever claiming link coverage. CI sets
it for every job.

`tests/transaction.rs` kills and stalls real `asm` processes at named checkpoints. A test that runs
a mutating session must redirect both the project through `--project-root` and SkillMount state
through `SKILLMOUNT_STATE_DIR`. Omitting either can modify this checkout or the developer's real
application-support state. Failure injection is debug-only and lives in `src/checkpoint.rs`.

CI additionally builds the MSRV, release binaries, Windows x64 and x86 targets, and Apple Silicon
macOS. Platform-conditional behavior needs tests on each affected native side; a cross-compile does
not prove native link, console, or filesystem behavior.

## Architecture boundaries

The read-only pipeline is:

```text
cli -> paths -> catalog -> agent discovery -> mount plan -> render
```

`inspect` and `--dry-run` stop there and create no directories, links, locks, journals, recovery
mutations, or child processes. Extend `tests/read_only.rs` whenever adding a read-only path.

A mutating session performs discovery, locks the observed resources, recovers incomplete
transactions, builds the complete plan under locks, stabilizes any expanded lock set, persists the
journal, applies, and cleans up. Before the first lock, the flow performs the fail-closed read-only
journal preflight, creates the staging-state base when needed, mints the transaction
identity, and inspects discovery; it does not build a complete plan. See ADR 0012 before changing
this order.

Agent adapters observe and describe their discovery model; they never mutate. Shared application
and transaction code owns ordering and application. `src/link/` is the sealed boundary for
platform-specific discovery-entry and mount-link classification and for every mount-link creation,
placement, and removal. It exposes no recursive removal operation.

`unsafe_code` is denied crate-wide. Only `src/agent/codex/macos_ffi.rs`,
`src/paths/windows_ffi.rs`, `src/link/unix_ffi.rs`, `src/link/windows_ffi.rs`,
`src/process/unix_ffi.rs`, and `src/process/windows_ffi.rs` may allow it, under ADRs 0011, 0019,
and 0023.
Every unsafe block needs a `SAFETY` comment, and no raw `libc` or `windows_sys` type may cross those
module boundaries.

## Cross-cutting safety rules

- Keep paths and forwarded arguments as platform-native `PathBuf` and `OsString` values; never
  force them through UTF-8 or a shell.
- Keep production child launch shell-free with inherited standard streams; redirect only at test
  harness boundaries, and retain child/process failure precedence over cleanup diagnostics.
- Preserve rightmost-wins, validate-after-select, and no-fallback catalog behavior.
- Keep the implemented discovery model synchronized with the architecture baseline. Before adding
  child launch, revalidate the supported agent versions and inspect every scope the child will
  search; never rely on undocumented duplicate precedence.
- Fail closed at catalog, discovery, and mount-entry boundaries: no-follow inspection, bounded link
  resolution, atomic no-replace placement, and no replacement of a regular directory or mismatched
  link.
- Persist intent before applying any planned destination mutation and recheck apply preconditions.
  Require matching recorded evidence immediately before path-based removal, retain mismatches or
  uncertainty, and preserve the reserved object-binding hardening.
- Take every required logical and physical resource lock before apply, cleanup, or recovery.
- Never edit the user's Git state, request UAC or `sudo`, or change Codex or Claude permission modes
  as product behavior.

## Git workflow

Normal flow is `main <- dev/<major>.<minor>.x <- <type>/<slug>`. Before implementation, identify the
active development line and create a short-lived topic branch from it; do not assume the active
version from stale guidance. Topic branches target their development line, and only a development
line targets `main`. Read `CONTRIBUTING.md` for prefixes, required checks, merge policy, and
emergency ruleset recovery.
