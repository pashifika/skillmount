# ADR 0028: Require Workflow-Tree Parity for GitHub Token Releases

- **Status:** Accepted
- **Date:** 2026-08-04
- **Supersedes:** _none_

## Context

Release provenance requires a stable version tag whose commit is reachable from `origin/main`.
Ancestry alone permits `main` to advance while a tag-triggered workflow is queued, which is normally
desirable.

GitHub's `2026-03-10` Releases API adds a narrower authorization constraint. Creating or updating a
release for a commit that adds or modifies any file under `.github/workflows` relative to the
repository's current default branch requires workflow-write authorization. The Actions
`GITHUB_TOKEN` cannot receive that authorization; GitHub documents a `404 Not Found` or, for some
authentication paths, `403 Resource not accessible by integration` response.

An ancestry-only preflight could therefore complete all native builds before encountering an opaque,
unrecoverable publication failure. A same-tag retry would continue to fail after default-branch
workflow divergence, and moving the protected tag would violate the immutable release contract.

## Decision

Tag-push preflight must fetch `origin/main` and require both of these conditions before exposing
build outputs:

- the tagged commit is an ancestor of `origin/main`;
- `.github/workflows` is identical at the tagged commit and fetched `origin/main`.

The write-scoped publish job must fetch and recheck the same conditions immediately before GitHub
Release interaction, because `main` can advance while native builds run. GitHub's Releases API
remains the final fail-closed authority if the default branch changes after that recheck.

Unrelated `main` changes remain allowed. Manual `workflow_dispatch` builds remain read-only and do
not require workflow-tree parity. Publication continues to use only the ephemeral `GITHUB_TOKEN`
with `contents: write`; no personal token, GitHub App credential, workflow-write permission, or tag
mutation operation is introduced.

If workflow parity is lost after a version tag is created, maintainers must not move or recreate the
tag. They must promote the intended workflow state and release the correction under a new patch
version.

## Alternatives

- Require equality with the current `main` commit. Rejected because unrelated documentation or
  product commits do not affect Release API authorization and should not invalidate a queued tag.
- Supply a classic personal access token, fine-grained token, or GitHub App credential with workflow
  write access. Rejected because it adds a standing secret, broader authority, rotation and
  ownership obligations, and an unnecessary bypass around the repository's least-privilege design.
- Let the Releases API reject publication. Rejected because the failure occurs only after expensive
  native builds and obscures the actionable workflow-tree mismatch.
- Create a draft before the build matrix starts. Rejected because it grants write authority before
  artifact verification and still cannot make a divergent commit publishable with `GITHUB_TOKEN`.

## Consequences

- Maintainers must not merge `.github/workflows` changes between version-tag creation and successful
  publication.
- A version tag whose workflow tree diverges from the default branch cannot be automatically
  published or resumed; correction requires a new immutable patch version.
- `main` may continue to advance outside `.github/workflows` while a release runs.
- Preflight catches existing divergence before the build matrix. The publication-boundary recheck
  catches divergence introduced while builds were running.
- A final race remains possible between the last fetch and the API request, but it is safe: GitHub
  rejects the request and the workflow never moves the tag or publishes an unverified release.

## Verification

- `.github/scripts/test_release.py` proves unrelated `main` movement passes while workflow-tree
  divergence fails against a real local Git remote.
- `tests/release_contract.rs` requires the publish job to revalidate the current `main` source after
  asset verification and before invoking the controlled publisher.
- `.github/scripts/test_release_publish.py` proves publication has no tag-mutation path and retains
  incomplete workflow-owned drafts for same-tag review or retry.
- `docs/releasing.md` records the workflow-freeze, recheck, and new-patch recovery procedure.
- The authoritative API constraint is documented under `Create a release` and `Update a release` in
  the [GitHub Releases API](https://docs.github.com/en/rest/releases/releases?apiVersion=2026-03-10).
