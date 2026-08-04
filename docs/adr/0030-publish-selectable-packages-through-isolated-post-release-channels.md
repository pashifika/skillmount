# ADR 0030: Publish Selectable Packages Through Isolated Post-Release Channels

- **Status:** Accepted
- **Date:** 2026-08-04
- **Supersedes:** _none_

## Context

`v0.1.0` shipped as a GitHub-only release, and the release baseline recorded Homebrew, Chocolatey,
and every other package manager as deferred. Operators on the supported targets asked for native
package-manager install and upgrade instead of manually placing release archives, so `v0.2.0` adds
first-party Homebrew and Chocolatey distribution. Three verified constraints shape the decision:

- The release publisher acts with the repository `GITHUB_TOKEN`, and GitHub suppresses workflow
  events caused by `GITHUB_TOKEN` actions. A `release.published` trigger therefore never fires for
  our own publication, so package work cannot chain from that event.
- A `workflow_run` workflow receives secrets and write privileges while consuming data influenced
  by the triggering run. Everything it reads from that run is untrusted until independently
  validated, and no job holding an external credential may execute triggering-tag code.
- One Cargo package builds two behaviorally equivalent executables, `asm` and `skillmount`, and
  `src/cli.rs` resolves the product identity from `argv[0]` and rejects a renamed alias. Every
  release archive intentionally contains both executables, but a package installation is a
  user-facing selection boundary, and neither executable may ever be installed, symlinked, or
  shimmed under another name.

## Decision

Package publication chains from successful completion of the `Release` workflow through a separate
privileged default-branch workflow (`.github/workflows/package.yml`) with a `workflow_run` trigger
plus a reviewed `workflow_dispatch` retry that requires an exact stable tag and defaults to
verification-only. It is never triggered by `release.published` and never called synchronously from
the release publisher. A credential-free preflight job revalidates tag, `main` ancestry, Cargo
version, release identity, the exact asset set, and every checksum before any channel job runs.

Homebrew and Chocolatey are independent failure domains: separate jobs, separate protected GitHub
Environments (`homebrew`, `chocolatey`), separate non-cancelling per-version concurrency groups,
and no `needs` edge between the publish jobs. One channel's failure, outage, or moderation delay
never cancels the other and never mutates the published GitHub release.

Homebrew distributes two source-built Formulae in the separately managed `pashifika/homebrew-tap`
repository, both pinning the same tag source tarball and SHA-256. Chocolatey distributes two
metadata-and-script packages that download the immutable Windows release archives and verify their
architecture-specific `SHA256SUMS` digests before extraction; the `.nupkg` files never embed a
product binary.

Each channel publishes a selectable one-executable pair: Formula/package `skillmount` builds or
retains only the `skillmount` executable and exposes only the `skillmount` command;
`skillmount-asm` builds or retains only `asm` and exposes only the `asm` command. Neither member
installs, depends on, aliases, or conflicts with the other, and co-installation is supported.

Pair reconciliation is pair-aware but not atomic. Before any write, the publisher queries both
member identities in its channel; an identical existing member is an idempotent success or status
check, and only an absent member receives creation work. Any existing version with mismatched
immutable metadata — source URL, digest, version, or selected executable — is a hard conflict that
blocks further pair publication, reports both observed and expected identities, and requires human
review; the remedy for published bytes is a new patch version, never an in-place edit.

Credentials are channel-scoped: the `homebrew` environment holds only a GitHub App installation
token scoped to the tap repository, and the `chocolatey` environment holds only the Chocolatey API
key. Preflight, generation, and acceptance jobs hold no external credential.

## Alternatives

- Trigger from the `release.published` event. Rejected because publication performed with the
  repository `GITHUB_TOKEN` does not emit that workflow event, so the chain silently never runs.
- Call package publication synchronously from the release publisher. Rejected because an external
  registry outage would turn an already-successful immutable GitHub release into a misleading
  failed release run, and it mixes unrelated credentials into the release workflow's trust domain.
- One Formula or Chocolatey package installing both executables. Rejected because it denies the
  requested install-time selection and creates completion entries and shims the operator did not
  select.
- Retain both executables and suppress one shim with a Chocolatey `.ignore` marker. Rejected
  because it makes only `PATH` selective, not installation: the unselected binary still ships in
  the package directory.
- A dependency or metapackage relationship between the pair members. Rejected because either
  direction implicitly installs the executable choice the operator declined.
- A broad personal access token or repository-wide secret available to preflight or build jobs.
  Rejected because those jobs consume untrusted triggering-run data, and least privilege requires
  that no credential exist where validation has not yet completed.

## Consequences

- Every stable release now implies package-channel work: four public identities
  (`pashifika/tap/skillmount`, `pashifika/tap/skillmount-asm`, and Chocolatey `skillmount` and
  `skillmount-asm`) must be reconciled, and their names become effectively permanent once public.
- The project owns external state: the tap repository, a GitHub App installation, a Chocolatey
  account with both package IDs and an API key, and two protected environments, each with
  documented rotation and revocation duties.
- Non-atomic pairs mean one install command can become public before its pair member. Status and
  documentation must report each identity separately and advertise a command only after its public
  endpoint resolves the version and a clean selected-only install passes.
- Installing both Formulae duplicates source build time and keg data. Accepted for explicit
  ownership; bottles remain a separate evidence-driven decision.
- The Formulae pin GitHub's generated source tarball (`archive/refs/tags/<tag>.tar.gz`) by SHA-256,
  and GitHub has historically re-compressed such tarballs. Preflight validates the digest at
  publication time; a later upstream re-compression invalidates an already-published Formula digest
  and requires a new patch version rather than an in-place edit.
- Chocolatey moderation can delay or reject either package ID independently, and pair eligibility
  must be confirmed with the Community Repository before either public ID exists.
- `docs/packaging.md`, `docs/architecture.md`, `README.md`, the tap source material under
  `packaging/homebrew/tap/`, and the package workflow policy tests changed in the same change.

## Verification

- `.github/scripts/test_package_channels.py` proves trigger policy (only a successful `Release`
  tag push or an exact-stable-tag dispatch), full preflight validation, template rendering, and
  structural pair inspection.
- `.github/scripts/test_package_publish.py` proves both reconcilers are pair-aware, idempotent for
  identical members, fail closed on any conflicting member, and never push twice or write a tap
  default branch.
- `.github/scripts/package_workflow_policy.py check`, with
  `.github/scripts/test_package_workflow_policy.py`, fails CI when the workflow drops action pins,
  adds caches, widens permissions, leaks a secret outside its publish job, links the publish jobs,
  or loses the verification-only gate.
- `.github/scripts/test_homebrew_acceptance.py` and `.github/scripts/test_chocolatey_acceptance.py`
  prove the native lifecycle harnesses that enforce selected-only installation, completion and shim
  ownership, co-installation, and cross-uninstall.
- External publication itself cannot be verified before the accounts and `v0.2.0` exist;
  `docs/packaging.md` records the verification-only rehearsal, the clean-host acceptance evidence
  each install command requires before it may be advertised, and the retry and conflict runbook
  that reviewers apply to a partially published pair.
