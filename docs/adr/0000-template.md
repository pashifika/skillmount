# ADR 0000: Template

Copy this file to `docs/adr/NNNN-short-kebab-case-title.md`, where `NNNN` is the
next unused four-digit number, and replace every section. Keep the record short:
an ADR states a decision and the evidence behind it, not a design specification.

- **Status:** Proposed | Accepted | Superseded by ADR NNNN | Withdrawn
- **Date:** YYYY-MM-DD
- **Supersedes:** ADR NNNN, or _none_

## Context

What situation forces a decision now. State the constraint, the affected public
contract or platform behavior, and what is currently true in the repository.
Include the measurement, prototype, or failure that prompted the decision, and
link to the tracked evidence.

## Decision

The decision, in one or two sentences, written as a rule that a reviewer can
apply. Name the affected modules, public contracts, platform behavior, defaults,
or artifacts explicitly.

## Alternatives

Each alternative that was seriously considered, and the specific reason it was
rejected. An alternative rejected only because it was unfamiliar is not a
rejection; say what it would cost.

## Consequences

What this decision commits the project to, including the parts that are worse. At
minimum:

- What integrators must now do differently, if anything.
- What becomes harder to change later, and what the migration path would be.
- Which platform, packaging, licensing, or performance obligations follow.
- Which documentation, examples, policies, and tests changed in the same change.

## Verification

How the decision is enforced rather than merely stated:

- The tests, contract suites, or repository checks that fail if the decision is
  violated.
- The evidence that supports the decision, at a tracked path.

If a decision cannot be verified automatically, say so and name the review step
that catches a regression instead.

## When an ADR is required

The V2 design specification is the normative architecture baseline. It is a
machine-local input rather than a tracked file, so an ADR is the only repository
record of a divergence from it. An ADR is required for a change that:

- replaces a normative decision in the V2 design, such as an adapter
  responsibility, a discovery or conflict rule, a data-model contract, a
  dependency direction, or a public naming rule;
- changes a public contract in `CONTRIBUTING.md`, the CLI surface, or the
  sysexits-style exit-code mapping in `src/error.rs`;
- adds a dependency whose license or source falls outside `deny.toml`, or that
  carries a native deployment obligation;
- relaxes a crate-level lint in `Cargo.toml`, in particular `unsafe_code` or
  `missing_docs`;
- changes the minimum supported Rust version, the pinned toolchain, or a release
  target.

An ADR is not required for ordinary implementation of an already recorded
decision. Prefer a small explicit ADR over silently diverging from the design
baseline.
