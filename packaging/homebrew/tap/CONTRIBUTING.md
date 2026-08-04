# Contributing to the SkillMount tap

> Source material: this file is tracked in `pashifika/skillmount` under `packaging/homebrew/tap/`
> and is transferred to the separately managed `pashifika/homebrew-tap` repository through that
> repository's own reviewed change. It is not the live tap.

This repository owns the published SkillMount Formulae, their CI, and their history. The Formula
content itself is generated in the product repository from templates under
`pashifika/skillmount:packaging/homebrew/`; treat that repository as the source of what a Formula
says and this repository as the source of what is published.

## How changes arrive

Version updates are machine-proposed. SkillMount's package workflow renders both Formulae from one
verified release identity and opens a single pull request on branch `skillmount/<version>` that
updates `Formula/skillmount.rb` and `Formula/skillmount-asm.rb` together. Automation authenticates
as a GitHub App installed only on this repository. It never pushes the protected default branch,
never force-pushes, and never closes or merges an existing pull request; a retried run resumes a
matching branch and stops on any mismatch.

Human changes are limited to tap infrastructure: CI, documentation, and repository policy. Do not
hand-edit a Formula's version, release-archive URL, digest, or selected binary; those values are
provenance generated from a verified release, and a hand edit breaks the pair-consistency checks
the publisher runs before every write. If a published version's bytes are wrong, the fix is a new
SkillMount patch release, never an in-place digest edit.

## Required checks

Every pull request must pass the tap CI workflow on Apple Silicon macOS before merge:

- `brew style` and `brew audit --strict` for both Formulae;
- an install of each Formula from its checked release archive;
- `brew test` for each Formula;
- selected-only installation: each Formula's keg contains exactly its own command and never the
  pair member's executable;
- co-installation of both Formulae, then cross-uninstall, proving each command keeps working while
  its own Formula remains installed;
- completion ownership: each shell has exactly one Formula-owned completion file per installed
  Formula, registering only that Formula's command;
- an upgrade rehearsal: both Formulae install at the base revision's version and `brew upgrade`
  reaches this pull request's version. It self-skips with a notice when the base branch has no
  published pair yet or the version is unchanged.

## Review expectations

For a version pull request, confirm before merging that:

1. both Formulae changed together and share the identical release-archive URL, SHA-256, version,
   license, and platform requirements;
2. each Formula names only its own release archive member and command, with the pair member's
   command appearing only inside the `test do` block;
3. the provenance comment names the expected release tag and commit;
4. every required check above is green.

A pull request that updates only one Formula of the pair, or changes a published version's
immutable metadata, is a conflict signal: stop and investigate in the product repository rather
than merging.
