# ADR 0013: Track Current Architecture and Shared Agent Guidance

- **Status:** Accepted
- **Date:** 2026-08-02
- **Supersedes:** _none_

## Context

SkillMount's implemented architecture was recoverable from source comments, accepted ADRs,
pull-request history, a separately managed Rasen planning repository, and a machine-local design.
A fresh product checkout did not designate one tracked current-state explanation of module
responsibilities, mutation ordering, safety invariants, or implemented-versus-reserved behavior.

The local `CLAUDE.md` also mixed durable repository instructions with transient change context and
treated the ignored design as canonical. Codex and Claude Code had no shared tracked guidance, and
human contributors had no route from `CONTRIBUTING.md` to the current architecture. Adding that
route changes a public contributor contract, which the existing ADR policy requires this record to
justify.

## Decision

`docs/architecture.md` is the normative current-state architecture baseline. Focused ADRs explain
replacements to its decisions, code comments retain local implementation reasoning, and pull
requests and Rasen changes describe deltas or history rather than current product authority.

`CLAUDE.md` is the single tracked source of durable coding-agent guidance. `AGENTS.md` is a Git
symbolic link to it. `CONTRIBUTING.md` directs human contributors to the same baseline and requires
an affected baseline section—and, for a replacement decision, a focused ADR—to change with the
implementation.

## Alternatives

- Keep current architecture in ignored local material and pull-request history. Rejected because a
  fresh checkout would still be incomplete and readers would have to reconstruct current state by
  replaying history.
- Put the full architecture in `CLAUDE.md`. Rejected because a tool-facing instruction file is not
  the right authority for human contributors and would again mix durable architecture with agent
  workflow.
- Track two regular copies of the agent guidance. Rejected because every edit would need manual
  synchronization and a silent divergence would give Codex and Claude Code different repository
  rules.
- Copy the separately managed Rasen artifacts into the product repository. Rejected because those
  artifacts record planned changes and evidence, and the two repositories intentionally have
  independent review and delivery histories.

## Consequences

- A change that alters current architecture must update `docs/architecture.md`. Replacing a
  normative decision also requires a focused ADR; ordinary implementation of an existing decision
  does not.
- Contributors must use a checkout that materializes Git symbolic links for `AGENTS.md`. A checkout
  that writes only the target name does not provide Codex with the shared guidance; this limitation
  is documented in `CONTRIBUTING.md` rather than hidden behind a second maintained copy.
- Keeping the baseline current adds review work. Source and tests remain the evidence for what is
  implemented, so a mismatch is resolved explicitly instead of allowing either side to win
  silently.
- Rasen configuration may point planning at the tracked baseline, but the nested planning
  repository remains a separate unit of work and must never enter the product Git index.
- Root-anchored product `.gitignore` entries keep the separately managed `rasen/`, `.rasen/`, and
  `local_docs/` paths out of ordinary staging. Agents still stage product paths explicitly because
  ignore rules do not protect against every index operation or an already tracked entry.
- Comprehensive user guidance remains outside this decision; the architecture baseline does not
  become a partial quick start, compatibility matrix, or recovery manual.

## Verification

- Git records `CLAUDE.md` as a regular file and `AGENTS.md` with symbolic-link mode `120000` and
  target `CLAUDE.md`.
- `CONTRIBUTING.md`, this ADR, the ADR template, and `docs/architecture.md` link to tracked product
  paths that resolve in a fresh checkout.
- Review searches current guidance and source comments for local-only authority claims and permits
  them only when explicitly labeled historical or non-authoritative.
- Product-repository ignore, status, and index inspection prove that neither `rasen/` nor
  `.rasen/` is tracked and that no user-facing README was introduced by this change.
- The authority split is semantic and cannot be fully enforced by a formatter or compiler. Changes
  to these documents therefore require the repository verification and pre-landing review recorded
  with the corresponding change.
