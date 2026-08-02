# ADR 0017: Preserve the interactive Unix foreground group

- **Status:** Accepted
- **Date:** 2026-08-03
- **Supersedes:** _none_

## Context

The process supervisor must both inherit the child's terminal streams and relay interruption to a
non-interactive child tree. Those requirements do not admit one unconditional Unix process-group
layout. [POSIX job control](https://pubs.opengroup.org/onlinepubs/009695399/basedefs/xbd_chap11.html)
stops a background process group that reads its controlling terminal with `SIGTTIN`. A macOS
prototype confirmed the consequence: the inherited-group child remained
`S+`, while an otherwise identical child placed in a new group entered stopped state `T` as soon
as it read the pseudo-terminal. This contradicts the initial change design's unconditional child
group assumption.

The tracked regression in `tests/process_supervision.rs` runs the supervisor beneath the macOS
`script` pseudo-terminal and requires the fake child to read an inherited line and complete
cleanup. It hangs or fails if `src/process/unix.rs` moves that child into a background group.

## Decision

`src/process/unix.rs` SHALL keep a child whose inherited stdin is a terminal in SkillMount's
foreground process group. It SHALL create a dedicated child process group for non-terminal stdin,
forward the first supported signal to that group, and use that group for the second-interrupt force
path.

A terminal-origin signal in the shared foreground group is recorded as platform-delivered rather
than sent to the child a second time. A signal explicitly delivered only to SkillMount is forwarded
to the child process.

## Alternatives

- Always create a child process group. This made an inherited terminal reader a background job and
  reproducibly stopped it with `SIGTTIN`.
- Give a dedicated child group terminal foreground ownership with `tcsetpgrp`. SkillMount would no
  longer receive terminal `SIGINT`, so it could not observe the documented first/second-interrupt
  state while preserving direct stream inheritance.
- Proxy the session through a pseudo-terminal. This would replace direct `Stdio::inherit()` with a
  stream intermediary and change TUI, descriptor, and terminal behavior.
- Keep every child in the parent group. This preserves interaction but cannot safely use `killpg`
  for non-interactive descendant termination because it would signal SkillMount itself.

## Consequences

- Interactive agents retain ordinary foreground terminal reads and receive terminal interrupts
  directly alongside SkillMount.
- Non-interactive supervision can forward to and force a child group, including descendants that
  have not deliberately changed groups.
- The shared-foreground second-interrupt path can force the direct child but cannot atomically
  `SIGKILL` its whole group without also killing SkillMount before orderly cleanup. Terminal
  `SIGINT` still reaches the foreground descendants; arbitrary daemonized or signal-ignoring
  descendants remain outside the guarantee, and durable transaction recovery remains the fallback.
- The portable Unix branch also runs in Ubuntu quality tests, but this decision does not add Linux
  to the release targets.
- Replacing direct terminal inheritance with a PTY proxy would require a new ADR and a different
  public stream contract.

## Verification

- `tests/process_supervision.rs::interactive_child_keeps_foreground_tty_read_access` proves native
  macOS foreground read access through inherited streams.
- `tests/process_supervision.rs::first_interrupt_reaches_a_child_process_group_descendant` proves
  group delivery in the non-terminal layout.
- `tests/process_supervision.rs::second_interrupt_forces_the_waiting_child_then_cleanup_runs_once`
  proves the non-terminal force path and exactly-once cleanup.
- `src/process/unix.rs` is the only Unix grouping and forwarding implementation; review rejects a
  second process-spawn path outside this boundary.
