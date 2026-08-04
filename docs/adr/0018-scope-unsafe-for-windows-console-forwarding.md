# ADR 0018: Scope unsafe for Windows console forwarding

- **Status:** Superseded by ADR 0019
- **Date:** 2026-08-03
- **Supersedes:** _none_

## Context

Windows creates the supervised child with
[`CREATE_NEW_PROCESS_GROUP`](https://learn.microsoft.com/en-us/windows/win32/procthread/process-creation-flags).
Microsoft documents that a nonzero process-group identifier cannot target `CTRL_C_EVENT`; graceful
targeted delivery therefore requires `CTRL_BREAK_EVENT` through
[`GenerateConsoleCtrlEvent`](https://learn.microsoft.com/en-us/windows/console/generateconsolectrlevent).
Stable Rust has no standard-library API for that call, and the selected safe `ctrlc` crate observes
parent console events but does not send a targeted event.

Calling `GenerateConsoleCtrlEvent` through `windows-sys` is unsafe at the binding boundary even
though its arguments are integers and it retains no Rust memory. The crate previously allowed
unsafe code only in the two audited link FFI modules under ADR 0011, so adding another opt-in is a
lint-boundary decision rather than ordinary implementation.

## Decision

Only `src/process/windows_ffi.rs` may additionally opt in to `unsafe_code`, solely to wrap
`GenerateConsoleCtrlEvent(CTRL_BREAK_EVENT, process_group_id)` as a safe `io::Result<()>` function.
Console observation, event queuing, command grouping, waiting, force termination, and cleanup stay
in safe Rust in `src/process/windows.rs` and the shared process module.

## Alternatives

- Send targeted `CTRL_C_EVENT`. Windows explicitly does not deliver it to a nonzero process group,
  even when the API reports success.
- Call only `Child::kill`. This removes the graceful first-interrupt contract and does not notify the
  child group before termination.
- Install a raw `SetConsoleCtrlHandler` callback in SkillMount. That expands unsafe code and places
  more logic near a system-created handler thread with severe allocation and I/O restrictions.
- Launch a shell or helper command to generate the event. This would violate the shell-free launch
  boundary and add quoting and executable-discovery failure modes.

## Consequences

- The audited unsafe allowlist grows from two modules to three; raw Windows types still do not cross
  any of those module boundaries.
- Every unsafe block in `src/process/windows_ffi.rs` requires a local `SAFETY` explanation. New
  console APIs or raw handles require another review and, if they broaden the rule, another ADR.
- `ctrlc 3.5.2` (MIT/Apache-2.0) supplies the safe observer thread. `windows-sys 0.61.2`, already
  pinned by the link backend, supplies the console and process-group bindings. Unix uses
  `signal-hook 0.4.4` (MIT OR Apache-2.0) and `nix 0.31.3` (MIT) so it needs no new unsafe opt-in.
- Windows x64 and x86 remain native test obligations; cross-compilation verifies types and layout
  but not console delivery.

## Verification

- `tests/process_supervision.rs` creates a native Windows console group, sends a break through the
  safe feature-gated test seam, and verifies child observation plus cleanup.
- `src/process/windows.rs` contains no unsafe block; all targeted console sending resolves through
  `src/process/windows_ffi.rs`.
- `cargo clippy --locked --all-targets --all-features -- -D warnings` compiles the allowed boundary
  under the crate lint, while `cargo deny --locked check` enforces the recorded dependency sources
  and licenses.
- Review of the raw diff verifies that no other module adds `allow(unsafe_code)`; this scope is not
  currently expressible as an automatic Cargo lint allowlist.
