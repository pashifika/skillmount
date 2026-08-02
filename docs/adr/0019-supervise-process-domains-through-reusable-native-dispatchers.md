# ADR 0019: Supervise process domains through reusable native dispatchers

- **Status:** Accepted
- **Date:** 2026-08-03
- **Supersedes:** ADR 0017, ADR 0018

## Context

The first supervisor implementation coupled each session to a disposable event observer and treated
successful signal or kill requests as sufficient to enter cleanup. Native regressions disproved
both assumptions. A macOS pseudo-terminal delivered one `SIGINT` to the shared foreground group,
after which sender-origin inference relayed a duplicate to the child. Windows isolated the child in
a new console group and forced only its root process, leaving ordinary descendants outside the
termination boundary. The observer APIs also either coalesced handler occurrences or could not be
installed again for a second session in the same wrapper process.

The cleanup callback will eventually remove transaction-owned discovery entries. It therefore
cannot run merely because delivery was requested: the managed process domain must first be proven
empty. The regressions and deterministic failure cases are tracked in
`tests/process_supervision.rs`, `src/process/event.rs`, and `src/process/driver.rs`.

## Decision

`src/process/driver.rs` SHALL own a private `Running` / `ProvenDead` / `Uncertain` state machine,
and the cleanup permit SHALL be constructible only for the no-child state or `ProvenDead`.
Process-lifetime native dispatchers SHALL record handler occurrences in the private atomic ledger
in `src/process/event.rs`. Each lease SHALL carry a non-repeating generation and begin armed rather
than active. Each callback SHALL snapshot its generation-tagged token before decoding or otherwise
classifying the event, so a handler already executing for one lease cannot write into a later
lease. A shared topology SHALL activate only after its child is exposed; inactive, armed, and
finalizing events return to platform default handling. A dedicated Unix topology MAY activate
before spawn because the child cannot receive that platform event directly and the queued
occurrence becomes the sole forwarded delivery after spawn.

Unix SHALL retain ADR 0017's interactive foreground versus non-interactive dedicated-group split,
but classify shared-group `SIGINT` from topology rather than sender metadata. Windows SHALL keep the
child in the wrapper's console group for one physical console delivery, preserve raw Ctrl+C versus
Ctrl+Break identity, and place the spawned process in a kill-on-close Job Object for tree-wide
force and liveness probing. Windows `.cmd` and `.bat` launch paths SHALL be rejected before spawn.

Spawned-child setup failures SHALL enter the same bounded force-and-proof driver as ordinary
liveness failures. Drop SHALL never perform a blocking wait. After a dedicated Unix group leader
is reaped, no forward or force operation may target its reusable numeric process-group identifier;
only bounded, signal-free emptiness probes may establish a terminal domain. If they cannot, cleanup
SHALL remain deferred. The caller-supplied cleanup operation SHALL report only success or structured
failure; the private cleanup permit is the sole authority that can select deferral.

Only `src/process/unix_ffi.rs` and `src/process/windows_ffi.rs` may add process-supervision
`unsafe_code`. They expose safe Rust results and types; raw platform types stay inside those
modules.

## Alternatives

- Keep sender-origin inference on Unix. The pseudo-terminal regression demonstrated two child
  deliveries from one terminal Ctrl+C, so metadata did not encode the required topology rule.
- Unregister and reinstall handlers per session. That creates an event-loss interval between child
  death and cleanup and does not support observer APIs whose global handler is single-install.
- Keep the Windows child in a new console group and relay Ctrl+Break. This duplicates policy in the
  parent, loses Ctrl+C identity, and does not contain descendants for the force path.
- Treat `kill` or Job termination success as death proof. Both calls report a request, not the
  terminal state needed to authorize destructive cleanup.
- Copy Tokio's asynchronous process tooling. SkillMount needs the state invariants, not an async
  runtime, captured I/O, or a new process API dependency; the smaller private synchronous seam is
  sufficient.

## Consequences

- A spawned process whose liveness remains uncertain produces `CleanupOutcome::Deferred`, retains
  every recorded process failure, and leaves the lifecycle guard armed for one best-effort,
  nonblocking force/reap attempt on drop. Callers must treat that result as recovery-required, not
  as a completed session.
- Non-interactive Unix groups and Windows Job Objects are probed until empty before cleanup. Unix
  probing is bounded after the group leader has been reaped; a still-present or reused numeric
  group yields uncertainty rather than a signal to an unbound identity.
  Interactive Unix descendants that share SkillMount's own foreground group remain outside the
  containment proof because killing that group would kill SkillMount before cleanup.
- Windows Job assignment occurs immediately after `Command::spawn`, but standard `Command` cannot
  make assignment atomic with suspended creation. A deliberately escaping process in that window
  remains a documented residual risk; attachment failure enters bounded termination and defers
  cleanup whenever the process domain cannot be proven empty.
- The audited unsafe allowlist grows from three modules to four. Windows console registration,
  Job Object handle ownership, queries, and termination now share the existing process FFI module;
  Unix raw registration is isolated in its own FFI module. Every unsafe block requires a local
  `SAFETY` explanation.
- `ctrlc 3.5.2` remains only an optional fake-agent fixture dependency. Product event dispatch uses
  the pinned `signal-hook 0.4.4` low-level registration API without extended signal metadata;
  `signal-hook`'s iterator is enabled only by the fake-agent fixture. Windows uses the pinned
  `windows-sys 0.61.2` bindings.
- Replacing inherited streams with a PTY proxy or making Windows Job assignment atomic would change
  these boundaries and require a new ADR.

## Verification

- `src/process/event.rs` tests occurrence retention, pre-exposure handling, generation-based stale
  writer rejection, finalization linearization, and sequential session reuse;
  `src/process/driver.rs` deterministically tests bounded setup-failure termination, uncertain
  liveness, complete event-batch accounting, and cleanup permission.
- `tests/process_supervision.rs::one_terminal_ctrl_c_reaches_an_interactive_child_once` proves the
  native macOS shared-group delivery rule.
- `tests/process_supervision.rs::interrupt_during_cleanup_returns_to_platform_default_handling`
  and `sequential_supervision_sessions_reuse_the_process_dispatcher` cover dispatcher lifecycle.
- `tests/process_supervision.rs::reaped_group_leader_with_live_descendant_defers_cleanup_within_bound`
  proves that Unix post-root probing terminates within a fixed bound without cleanup or signalling
  a reusable process-group identity.
- Native Windows tests prove raw Break identity, case-insensitive batch rejection, and Job Object
  descendant termination before cleanup. Windows x64 and x86 CI remain required because
  cross-compilation proves only types and layout.
- `cargo clippy --locked --all-targets --all-features -- -D warnings` enforces the crate unsafe lint;
  raw-diff review verifies that no module outside the four named boundaries opts in.
