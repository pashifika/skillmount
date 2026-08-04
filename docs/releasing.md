# Releasing SkillMount

This runbook is for repository maintainers. SkillMount publishes stable binary releases only from
an existing `vMAJOR.MINOR.PATCH` tag whose commit is contained in `main`. The release workflow never
creates, moves, or deletes a tag.

## Reviewed platform and action contract

The release dependencies below were checked against current official sources on 2026-08-04. Every
third-party workflow boundary is a GitHub-maintained action pinned to the immutable commit behind
its named release.

| Dependency | Reviewed release or label | Immutable revision / observed architecture |
|---|---|---|
| `actions/checkout` | `v7.0.1` | `3d3c42e5aac5ba805825da76410c181273ba90b1` |
| `actions/upload-artifact` | `v7.0.1` | `043fb46d1a93c77aae656e7c1c64a875d1fc6a0a` |
| `actions/download-artifact` | `v8.0.1` | `3e5f45b2cfb9172054b4087a40e8e0b5a5461e7c` |
| Windows x64 and x86 builds | `windows-2025` | x64 host; x86 binaries execute through Windows x64 compatibility |
| Apple Silicon build | `macos-15` | arm64, currently an M1 standard runner |
| Preflight, aggregate, publish | `ubuntu-24.04` | x64 |
| GitHub REST requests | `2026-03-10` | Explicit `X-GitHub-Api-Version` header |

The workflow logs `RUNNER_ARCH` and `rustc -vV`, then fails if the observed runner architecture,
Rust host, or requested target differs from the fixed matrix. Artifact download uses digest mismatch
as an error. Workflow permissions default to `contents: read`; only the final tag-push publication
job overrides that scope with `contents: write`.

GitHub's release-creation API rejects the Actions `GITHUB_TOKEN` when the release commit changes any
file under `.github/workflows` relative to the current default branch. Tag-push preflight therefore
also requires that workflow tree to match `origin/main`; `main` may advance through changes outside
that tree. Do not merge workflow changes between creating a release tag and publication.

Primary references:

- [GitHub-hosted runners](https://docs.github.com/en/actions/reference/runners/github-hosted-runners)
- [Workflow syntax and permissions](https://docs.github.com/en/actions/reference/workflows-and-actions/workflow-syntax)
- [Workflow concurrency](https://docs.github.com/en/actions/how-tos/write-workflows/choose-when-workflows-run/control-workflow-concurrency)
- [Workflow artifacts](https://docs.github.com/en/actions/tutorials/store-and-share-data)
- [GitHub Releases API](https://docs.github.com/en/rest/releases/releases?apiVersion=2026-03-10)
- [Release assets API](https://docs.github.com/en/rest/releases/assets?apiVersion=2026-03-10)
- [Available ruleset rules](https://docs.github.com/en/repositories/configuring-branches-and-merges-in-your-repository/managing-rulesets/available-rules-for-rulesets)
- [Rulesets API](https://docs.github.com/en/rest/repos/rules?apiVersion=2026-03-10)
- [Rule-suite evaluation API](https://docs.github.com/en/rest/repos/rule-suites?apiVersion=2026-03-10)

Recheck these sources and action release tags before changing a runner label, action pin, permission,
artifact behavior, API version, or ruleset. Update the table and `tests/release_contract.rs` in the
same change.

## Package contract

A validated release produces exactly these files, where `<tag>` includes the leading `v`:

```text
skillmount-<tag>-x86_64-pc-windows-msvc.zip
skillmount-<tag>-i686-pc-windows-msvc.zip
skillmount-<tag>-aarch64-apple-darwin.tar.gz
SHA256SUMS
```

Each archive has one top-level directory matching its filename stem. That directory contains only
`asm` and `skillmount` (with `.exe` on Windows), `LICENSE-APACHE`, `LICENSE-MIT`, and `VERSION`.
`VERSION` binds the package name, Cargo version, tag, target, and full commit ID. Archive ordering,
timestamps, owners, and modes are normalized; macOS executable modes are preserved.

Repackaging the same binary inputs and validated metadata is byte deterministic. The GitHub-hosted
runner image can change behind a fixed label, so independent binary builds are not claimed to be
reproducible across image updates. The pinned Rust toolchain, recorded runner evidence, and published
SHA-256 values identify the actual distributed bytes.

## Prepare and review a version

1. Create the version change through the normal topic-to-development pull-request flow. Update the
   root `Cargo.toml` version, let Cargo synchronize the root package entry in `Cargo.lock`, and
   review that no dependency version changed unexpectedly.
2. Run `cargo metadata --locked --no-deps --format-version 1` and the required checks from
   `CONTRIBUTING.md`.
3. Merge the topic pull request into the active `dev/<major>.<minor>.x` line.
4. Promote that exact development line to `main` through a pull request and wait for the required
   `CI / gate` result. Do not tag a development or topic commit.
5. Record the resulting full `main` commit ID. Confirm the version in locked Cargo metadata and
   prove ancestry without requiring equality to a later `main` tip:

```text
git fetch origin main --tags
git merge-base --is-ancestor <commit> origin/main
cargo metadata --locked --no-deps --format-version 1
```

A zero status from `git merge-base` is required. The release workflow repeats these checks from a
full checkout and also compares `.github/workflows` with `origin/main` before exposing version,
target matrix, or commit outputs. If that tree differs, do not move the tag: promote the intended
workflow state and release a new patch version.

## Rehearse without publication

`workflow_dispatch` exists to exercise the complete three-target build, smoke, package, aggregate,
and checksum path. The workflow definition must already exist on the default branch. Select the
workflow from `main`, and pass the commit, branch, or tag to verify as the `ref` input:

```text
gh workflow run Release --ref main -f ref=<selected-ref>
gh run list --workflow Release --event workflow_dispatch --limit 1
gh run watch <run-id> --exit-status
```

The run must contain successful `windows-x64`, `windows-x86`, `macos-arm64`, and `aggregate` jobs.
Its three `release-package-*` artifacts and `verified-release-assets` bundle must exist. The
`publish` job must be skipped. Manual preflight always emits `publish=false`, and the workflow's
publish condition independently requires a tag-push event, an exact tag ref, and every successful
predecessor.

## Install and verify version-tag protection

Apply tag protection only after `.github/workflows/release.yml` is present on `main`. Review
`.github/rulesets/version-tags.json`, verify that no ruleset with the same name already exists, then
create it once:

```text
gh api --method GET repos/{owner}/{repo}/rulesets -f targets=tag
gh api repos/{owner}/{repo}/rulesets --input .github/rulesets/version-tags.json
```

Read back the returned ID and require all of these values:

- name `Protect version tags`, target `tag`, enforcement `active`;
- include pattern exactly `refs/tags/v*`, with no exclusions;
- `update`, `deletion`, and `non_fast_forward` rules;
- `update_allows_fetch_and_merge=false`;
- no `creation` rule and an empty `bypass_actors` array.

GitHub currently documents an effective-rules endpoint only for branches, not tags. Do not simulate
effectiveness by moving or deleting a real version tag. After normal first-tag creation, query the
non-destructive rule-suite evidence for that exact ref and read its detailed active evaluations:

```text
gh api --method GET repos/{owner}/{repo}/rulesets/rule-suites \
  -f ref=refs/tags/<tag> -f time_period=day -f evaluate_status=active
gh api repos/{owner}/{repo}/rulesets/rule-suites/<suite-id>
```

Retain the ruleset payload/readback and the rule suite with the release evidence. Normal tag creation
must pass; subsequent update, force update, and deletion remain disallowed without a standing bypass.

## Create the release tag

Create one annotated tag from the reviewed `main` commit and push only that new ref:

```text
git tag -a <tag> <commit> -m "SkillMount <tag>"
git push origin refs/tags/<tag>
```

The broad `v*` trigger is intentional because GitHub tag filters are globs. Preflight then applies
the anchored stable-version grammar, compares Cargo metadata, resolves the tag to a commit, fetches
`origin/main`, proves ancestry, and checks the workflow tree required by GitHub's `GITHUB_TOKEN`
release restriction. Malformed or prerelease tags, version mismatches, off-main tags, and workflow
divergence stop before builds and publication.

Watch the tag run:

```text
gh run list --workflow Release --event push --limit 5
gh run watch <run-id> --exit-status
```

All three native build rows and `aggregate` must pass before the write-scoped `publish` job can run.
Publication concurrency is scoped to the validated tag and does not cancel an in-progress publisher.

## Verify the published release

Require one non-draft, non-prerelease release titled with the exact tag, generated notes, and exactly
the three archives plus `SHA256SUMS`. Download the files into an empty directory:

```text
gh release download <tag> --dir <empty-directory>
shasum -a 256 -c <empty-directory>/SHA256SUMS
```

From a repository checkout, the stricter cross-platform verification also reopens every archive and
checks layout, modes, licenses, version metadata, target, tag, commit, and SHA-256 values:

```text
python -B .github/scripts/release.py verify-set \
  --directory <empty-directory> \
  --version <MAJOR.MINOR.PATCH> \
  --tag <tag> \
  --commit <commit>
```

Run `asm --version` and `skillmount --version` after extracting each native package. Both must report
`SkillMount <MAJOR.MINOR.PATCH>`; the macOS files must remain executable.

## Failure and retry policy

The release boundary is deliberately asymmetric:

| Failure | Required outcome |
|---|---|
| Preflight, target build, smoke, package, aggregate, or checksum failure | No GitHub Release interaction occurs. Fix through a new commit and, if a version tag already exists, use a new patch version rather than moving it. |
| Upload interruption after draft creation | A marker-bound draft remains. Rerun the failed same-tag workflow job while its workflow artifacts are retained. |
| Matching workflow draft with an absent asset | Upload only the missing asset, then redownload and verify the complete set. |
| Matching workflow draft with an `open` incomplete asset | Delete only that incomplete asset and retry its upload. |
| Uploaded asset, ownership marker, tag commit, title, or asset-set conflict | Stop for human review. Never use `--clobber`, silently replace bytes, or publish the draft. |
| Retry after successful publication | Redownload and verify the existing complete release; perform no mutation. |

The workflow-owned marker binds the draft to the repository tag and full commit and records the
initial run URL. `release_publish.py` has no tag mutation operation. It publishes only after every
remote asset has been downloaded by asset ID and the complete archive/checksum contract passes again.

If a published binary is defective, increment the patch version, merge the correction through the
normal development and `main` promotion flow, and create a new immutable tag. Use explicit GitHub
release administration only for release-description metadata corrections; never rewrite the tag or
published asset bytes in place.

## Evidence to retain

For every release, retain:

- version-preparation and promotion pull requests, final `main` commit, tag object, and ancestry result;
- workflow run/attempt, action revisions, runner labels, `RUNNER_ARCH`, and `rustc -vV` output;
- workflow artifact names and final archive SHA-256 values;
- release ID, draft/retry outcome, final asset listing, generated-notes confirmation, and parity smoke;
- tag-ruleset ID/payload/readback and exact-ref rule-suite evaluation;
- explicit scope note that signing, notarization, crates.io, Homebrew, Chocolatey, and other
  package-manager publication remain deferred.
