# ADR 0032: Establish Chocolatey Ownership Through Submission

- **Status:** Accepted
- **Date:** 2026-08-05
- **Supersedes:** ADR 0030's Chocolatey pre-reservation and pre-moderation-confirmation decision only

## Context

ADR 0030 required maintainers to own both Chocolatey package IDs and obtain an explicit duplicate-
eligibility decision before either ID existed. The Community Repository does not document a
separate ID-reservation or ownership-binding operation. Its current
[Quick Start](https://docs.chocolatey.org/en-us/create/create-packages-quick-start/) orders first
publication as account registration, API-key retrieval, and `choco push`. The
[naming guide](https://docs.chocolatey.org/en-us/create/create-packages/#naming-your-package) says
the first submitted version fixes an ID's casing even before moderation, and the
[moderation workflow](https://docs.chocolatey.org/en-us/community-repository/moderation/)
begins when a maintainer submits a package and the package enters `Pending`.

The authoritative ownership check also occurs on push. Chocolatey's
[`choco push` troubleshooting](https://docs.chocolatey.org/en-us/create/commands/push/#troubleshooting)
states that HTTP 403 can mean the package already exists under another user, including an unlisted
package that a public-feed lookup cannot reveal. Therefore an empty OData result cannot prove that
an ID is unreserved or already owned by the submitting account, and ownership cannot be acquired as
a prerequisite to the operation that creates the package record.

The implementation also used one URL for reads and writes. Chocolatey's
[`choco push` command reference](https://docs.chocolatey.org/en-us/create/commands/push/) requires
`https://push.chocolatey.org/` for Community Repository uploads, while immutable package metadata
is read from the Community OData feed at `https://community.chocolatey.org/api/v2`. Candidate tests
had not exercised a real credentialed upload, so they could not expose this endpoint error.

Production-shaped inspection on 2026-08-05 exposed two further mismatches. Community OData records
declare `PackageHashAlgorithm` as `SHA512` and encode the exact nupkg digest as base64; that value is
not the SHA-256 digest recorded for the locally generated nupkg. The records also do not expose the
`Listed` or `IsListed` field assumed by the fake gateway. Chocolatey's
[API guidance](https://docs.chocolatey.org/en-us/community-repository/api/) supports the
Chocolatey CLI, not custom OData listing queries, so public availability needs a separate
CLI-resolution check.

## Decision

Chocolatey publication SHALL query immutable package/version metadata and moderation state through
the Community OData feed, compare its SHA-512 package hash with a SHA-512 digest computed from the
validated nupkg, and prove current public resolution with
`choco search <id> --version=<version> --exact --all-versions --approved-only --limit-output`. It
SHALL send `choco push` only to the documented Community upload endpoint. The credential-free
generation step's nupkg SHA-256 remains a separate binding to the candidate. An empty feed result is
a preflight observation, not ownership proof.

For a first publication, the approved Chocolatey lane SHALL recheck both IDs before any write, then
submit the exact validated `skillmount` package followed by `skillmount-asm`. Each accepted upload
establishes that package record under the API-key account and enters per-ID moderation. A prior
explicit moderator refusal or an ownership/forbidden-name response SHALL stop publication; a later
moderation rejection SHALL block further writes and public install guidance. Automation SHALL NOT
replace the pair with one package that installs both commands.

This replaces only ADR 0030's impossible pre-reservation ordering and its requirement for a
pre-submission duplicate decision. The selected-executable pair, independent moderation states,
non-atomic/idempotent reconciliation, protected credential boundary, and no-fallback decisions
remain in force.

## Alternatives

- Wait indefinitely for an advisory Community Hub answer before creating either ID. Rejected because
  the documented moderation process starts with submission, and the Hub is not the ownership or
  package-moderation API. A moderator answer remains useful and any explicit refusal is binding, but
  silence is not a separate reservation state.
- Treat an empty public feed as proof that both names are available. Rejected because Chocolatey's
  own 403 guidance identifies existing unlisted packages that the public page may not expose.
- Push packages to the OData query URL. Rejected because the current `choco push` command reference
  names `https://push.chocolatey.org/` as the Community upload source.
- Compare the OData package hash with the release-preflight SHA-256. Rejected because the Community
  Repository records a SHA-512 digest of the nupkg; comparing different algorithms would turn every
  accepted package into a false immutable-metadata conflict.
- Infer public listing from an absent OData `Listed` field. Rejected because observed records omit
  that field, and treating omission as either true or false would respectively advertise unlisted
  packages or prevent any approved package from becoming advertisable.
- Submit only `skillmount` and abandon `skillmount-asm`. Rejected because that silently removes the
  recorded install-time command selection instead of letting moderation decide the reviewed pair.
- Make the pair atomic. Rejected because the Community Repository accepts and moderates package IDs
  independently; no transaction spans two uploads.

## Consequences

- The existing `pashifika` Chocolatey account and its API key are sufficient to attempt first
  publication; package-ID ownership is observed from each accepted upload or explicit rejection,
  not configured beforehand.
- A successful first upload can remain pending when the second upload fails. Retry must preserve the
  accepted member and submit only the absent member after rechecking both IDs.
- Duplicate-package eligibility is decided by the real per-ID moderation records. Until each package
  is approved, listed, and clean-host verified, its install command remains unavailable.
- An ownership conflict, forbidden name, or moderator rejection is a product-review boundary. The
  publisher reports the partial state and stops; it does not repush, overwrite, unlist, or invent a
  replacement package.
- `package_publish.py`, its state-machine tests, the architecture baseline, and the packaging runbook
  change together. The publisher retains both the credential-free candidate nupkg SHA-256 and
  Community package SHA-512, and uses the supported Chocolatey CLI only for public-listing
  resolution. The GitHub Release and Homebrew lane remain unaffected.

## Verification

- `.github/scripts/test_package_publish.py` verifies that OData reads use
  `https://community.chocolatey.org/api/v2`, uploads use `https://push.chocolatey.org/`, observed
  base64 `SHA512` metadata binds to the local nupkg bytes, and public listing is proved through the
  exact versioned approved-only `choco search` command.
- The same state-machine tests verify that both IDs are observed before either write and that a
  retry after second-upload failure preserves the accepted first member and pushes only the absent
  member.
- `.github/scripts/package_workflow_policy.py check` continues to prove that the API key exists only
  in the protected Chocolatey publisher job.
- The first production workflow run must retain both upload responses, both digest identities, and
  subsequent moderation states. A public install is not claimed until the supported CLI resolves
  the corresponding listed version and the clean-host lifecycle passes.
