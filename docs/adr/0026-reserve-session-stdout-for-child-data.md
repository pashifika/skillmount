# ADR 0026: Reserve Session Stdout for Child Data

- **Status:** Accepted
- **Date:** 2026-08-04
- **Supersedes:** _none_

## Context

SkillMount launches agents with inherited standard streams, but it previously wrote the human
session summary to stdout immediately before launch. Codex `exec --json` and Claude
`--output-format json` therefore produced a stream beginning with `Mounted ...` instead of the
child's JSON or JSONL. Executable-seam reproductions in `tests/codex_session.rs` and
`tests/claude_session.rs` fail JSON parsing under that ordering.

## Decision

For mutating agent sessions, stdout is exclusively the inherited child data stream. SkillMount
writes every wrapper-owned session summary, warning, informational line, and cleanup diagnostic to
stderr; read-only reports and explicit operator-command reports retain their documented stdout
contract.

## Alternatives

- Detect known machine-output flags and suppress the summary only for those invocations. Rejected
  because each pinned agent has multiple evolving output modes, and parsing passthrough arguments
  into a second compatibility policy would be incomplete and version-fragile.
- Pipe, parse, and multiplex the child output. Rejected because that would replace inherited
  streams, alter interactive and non-Unicode behavior, and make SkillMount responsible for agent
  output formats.
- Keep the prefix and require consumers to discard it. Rejected because stdout would no longer be
  a composable machine-data interface and a valid JSON/JSONL parser could not consume it directly.

## Consequences

- Integrations that previously read SkillMount's human session summary from stdout must read
  stderr instead.
- Redirecting stdout now preserves the child's bytes without wrapper prefixes; SkillMount still
  does not capture, transform, or interpret production child output.
- `inspect`, `--dry-run`, `doctor`, and `cleanup` remain report-producing commands on stdout.
- Session tests, the architecture baseline, and the live-agent smoke harness are synchronized with
  this stream contract.

## Verification

- `machine_readable_codex_stdout_is_not_prefixed_by_wrapper_diagnostics` parses the complete Codex
  fixture stdout as JSON.
- `machine_readable_claude_stdout_is_not_prefixed_by_wrapper_diagnostics` does the same for Claude.
- The ordinary session tests assert that summaries and launch notices appear on stderr and that
  otherwise-silent child stdout stays empty.
