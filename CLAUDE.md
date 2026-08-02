# Repository Guidelines

## Authority and scope

`docs/architecture.md` is the tracked current-state architecture baseline and the source of truth
for product scope, module responsibilities, dependency and mutation boundaries, safety invariants,
supported targets, and implementation status. `CONTRIBUTING.md` defines branch and pull-request
workflow. Focused architecture decision records (ADRs) under `docs/adr/` explain replacements to
normative baseline decisions.

When implementation, configuration, or tests change the architecture, update the baseline in the
same product change. Add an ADR when replacing a normative decision; ordinary implementation of an
already recorded rule does not need another ADR. Source and tests provide the evidence for current
status, so resolve a mismatch rather than silently choosing one side.

The `rasen/` directory is a nested, separately managed planning repository. Rasen proposals,
specs, designs, tasks, and evidence describe planned deltas or history; they are not a substitute
for tracked product documentation. Never stage `rasen/` as a product-repository gitlink or
submodule, and keep product and planning-store commits separate. Ignored `local_docs/` material is
proposal or historical input only.

## Product definition

SkillMount is a Rust wrapper CLI that makes external Agent Skills visible for the intended lifetime
of a Codex CLI or Claude Code CLI session. The package installs `asm` as the primary binary and
`skillmount` as a behaviorally identical fallback; both delegate to `skillmount::run_from`.

The catalog is a deterministic rightmost-wins overlay. SkillMount validates only the selected
winner, never falls back from an invalid winner, and never lets source precedence replace a
project-owned Skill. It treats selected Skills as trusted user code and does not transform them,
manage authentication, elevate privileges, or weaken agent permissions.

Read `docs/architecture.md` before changing cross-module behavior. Its implementation-status
section distinguishes working catalog/planning/transaction behavior from reserved agent launch,
operator commands, ownership binding, and release work.

## Commands

```bash
SKILLMOUNT_REQUIRE_LINKS=1 cargo test --locked --all-targets
cargo test --locked --test read_only
cargo test --locked --test transaction
cargo clippy --locked --all-targets --all-features -- -D warnings
cargo fmt --all -- --check
RUSTDOCFLAGS=-D warnings cargo doc --locked --no-deps --all-features
cargo deny --locked check
cargo run --locked --bin asm -- inspect --skills-dir <PATH>
cargo run --locked --bin asm -- codex --skills-dir <PATH> --dry-run --verbose
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
journal, applies, and cleans up. Discovery—not a complete plan—is the only filesystem observation
allowed before the first lock. See ADR 0012 before changing this order.

Agent adapters observe and describe their discovery model; they never mutate. Shared application
and transaction code owns ordering and application. `src/link/` is the sealed platform boundary and
the only module tree that touches real links. It exposes no recursive removal operation.

`unsafe_code` is denied crate-wide. Only `src/link/unix_ffi.rs` and
`src/link/windows_ffi.rs` may allow it, under ADR 0011. Every unsafe block needs a `SAFETY` comment,
and no raw `libc` or `windows_sys` type may cross those module boundaries.

## Cross-cutting safety rules

- Keep paths and forwarded arguments as platform-native `PathBuf` and `OsString` values; never
  force them through UTF-8 or a shell.
- Preserve rightmost-wins, validate-after-select, and no-fallback catalog behavior.
- Inspect every discovery scope the child would search; never rely on undocumented duplicate
  precedence.
- Fail closed at filesystem boundaries: no-follow inspection, bounded link resolution, atomic
  no-replace placement, and no replacement of a regular directory or mismatched link.
- Persist intent before mutation, recheck apply preconditions, and retain anything whose ownership
  cannot be proved.
- Take every required logical and physical resource lock before apply, cleanup, or recovery.
- Never edit the user's Git state, request UAC or `sudo`, or change Codex or Claude permission modes
  as product behavior.

## Git workflow

Normal flow is `main <- dev/<major>.<minor>.x <- <type>/<slug>`. Before implementation, identify the
active development line and create a short-lived topic branch from it; do not assume the active
version from stale guidance. Topic branches target their development line, and only a development
line targets `main`. Read `CONTRIBUTING.md` for prefixes, required checks, merge policy, and
emergency ruleset recovery.
