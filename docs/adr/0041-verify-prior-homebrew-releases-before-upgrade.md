# ADR 0041: Verify prior Homebrew releases before upgrade

- **Status:** Accepted
- **Date:** 2026-10-31
- **Supersedes:** ADR 0030's Homebrew cross-version conflict rule only

## Context

The v0.3.0 package publisher rejected the existing v0.2.0 tap pair as conflicting provenance
before writing anything ([run 41093386125](https://github.com/pashifika/skillmount/actions/runs/41093386125)).
A Formula tracks the current version, unlike a version-addressed Chocolatey package. Applying
same-version identity equality to the default tap branch prevents every normal upgrade.

## Decision

Before any tap write, `package_publish.py` may classify a default-branch Formula as an upgrade
only when its stable version is strictly older, its archive URL is canonical for the same release
repository and Apple Silicon target, its selected executable/command are unchanged, and its SHA-256
matches the unique uploaded asset in that prior non-draft, non-prerelease published release.
Only the version-specific review branch receives new bytes. On retry, that branch may contain the
exact verified old default Formula bytes inherited before the first write; other mismatched
provenance remains a hard conflict. No release, tag, default branch, or Chocolatey behavior changes.

## Alternatives

- Treat every older version as safe: rejected because it accepts altered digests and foreign sources.
- Manually replace the tap pair: rejected because it bypasses the normal publisher and retry checks.
- Require new release binaries: rejected because this defect is isolated to post-release publishing.

## Consequences

Prior GitHub release asset metadata must remain available and include its SHA-256 digest. Missing
or contradictory metadata requires human review, not a fallback. The credentialed publisher reads
metadata only and never downloads or executes prior binaries. Existing tap PR review/native CI and
protected environment approval remain mandatory. Architecture, packaging runbook, and publisher
state-machine tests change together.

## Verification

`test_package_publish.py` covers paired upgrades, retry after zero/one writes, idempotency,
unchanged default Formulae, and refusal of mismatched/missing digests, unpublished releases,
duplicate assets, downgrades, foreign sources, wrong executables, malformed versions, and changed
old-version branch bytes. Native package acceptance and tap CI remain required before publication.
