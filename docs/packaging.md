# Packaging SkillMount

This runbook is for repository maintainers operating the Homebrew and Chocolatey package channels.
It continues where [docs/releasing.md](releasing.md) ends: package publication starts only after a
stable GitHub release is published and verified, and nothing in this runbook may delete, rewrite,
or change the success state of that release, its tag, assets, notes, or checksums.
[ADR 0030](adr/0030-publish-selectable-packages-through-isolated-post-release-channels.md) records
the channel and isolation decisions;
[ADR 0031](adr/0031-use-release-archives-for-homebrew-formulae.md) records the Homebrew
release-archive decision; [docs/architecture.md](architecture.md) records the resulting baseline.

## Channel and identity contract

Both channels publish a pair of one-executable packages that consume the same immutable release
identity. Every entry below is reconciled separately and reported separately:

| Public identity | Channel | Installs | Exposes | Distribution model |
|---|---|---|---|---|
| `pashifika/tap/skillmount` | Homebrew | release member `skillmount` | `skillmount` | Checked Apple Silicon release archive |
| `pashifika/tap/skillmount-asm` | Homebrew | release member `asm` | `asm` | Checked Apple Silicon release archive |
| `skillmount` | Chocolatey | `skillmount.exe` | `skillmount` shim | Checked x86/x64 release archive |
| `skillmount-asm` | Chocolatey | `asm.exe` | `asm` shim | Checked x86/x64 release archive |

Neither pair member installs, depends on, aliases, or conflicts with the other. Installing both
members of a pair is supported, and each command owns only its own executable, shim, and completion
files. The supported package targets are Apple Silicon macOS for Homebrew and Windows x64 and x86
for Chocolatey. Homebrew Core, casks, bottles, Linux, macOS Intel, Windows ARM64, WinGet, Scoop,
and crates.io are out of scope.

Current state: the protected `pashifika/homebrew-tap` exists in its never-published bootstrap state,
but no Formula or publisher credential exists and no Chocolatey package ID is reserved.
Package-manager version `0.2.0` is not yet publicly available, so every install command in this
document remains unavailable and is documented as such.

## The package workflow

`.github/workflows/package.yml` runs from the default branch. It is triggered by successful
completion of the `Release` workflow (`workflow_run`) or by manual dispatch with an exact stable
tag, a channel selection (`both`, `homebrew`, or `chocolatey`), and a `verification_only` flag that
defaults to `true`. The `release.published` event is intentionally unused because publication with
the repository `GITHUB_TOKEN` suppresses it.

| Job | Runner | Credentials | Responsibility |
|---|---|---|---|
| `preflight` | `ubuntu-24.04` | none | Trigger policy, tag/ancestry/version/asset/checksum revalidation, immutable `package-inputs` artifact |
| `generate` | `ubuntu-24.04` | none | Renders both Formulae and both Chocolatey source trees, inspects the pairs, uploads `package-candidates` |
| `homebrew-acceptance` | `macos-15` | none | Native isolated-tap lifecycle harness (`homebrew_acceptance.py`) |
| `chocolatey-acceptance` | `windows-2025` | none | Native Chocolatey lifecycle harness (`chocolatey_acceptance.py`), packs and inspects both nupkgs |
| `homebrew-publish` | `ubuntu-24.04` | `homebrew` environment | Proposes the paired tap pull request through the tap-scoped GitHub App token |
| `chocolatey-publish` | `windows-2025` | `chocolatey` environment | Reconciles both package IDs against the Community Repository with the API key |
| `summary` | `ubuntu-24.04` | none | Per-entry status table; fails only on a non-publish predecessor failure |

Preflight treats every triggering-run and release value as untrusted until independently
revalidated: exact stable tag, annotated-tag dereference, `main` ancestry, `Cargo.toml` and
`Cargo.lock` version agreement, release identity, the exact three-archive-plus-`SHA256SUMS` asset
set, every digest, and the dual-binary archive layout. No job holding an external credential checks
out or executes triggering-tag code, extracts an archive, or runs a downloaded binary. Neither
publish job depends on the other, and each uses a non-cancelling per-version concurrency group.

## Run verification-only mode

Rehearse the complete package path without production credentials. Verification-only runs execute
preflight, generation, structural pair inspection, and both native acceptance harnesses while both
publish jobs are skipped and both environments remain unreachable:

```bash
gh workflow run package.yml --ref main \
  -f tag=v0.2.0 -f channels=both -f verification_only=true
gh run list --workflow package.yml --limit 1
gh run watch <run-id> --exit-status
```

A malformed tag, branch name, commit, or prerelease is rejected by preflight before any environment
approval is requested. Keep the run's `package-inputs`, `package-candidates`, and
`chocolatey-nupkgs` artifacts with the release evidence.

## Approve a production publication

Both publish jobs wait on protected GitHub Environments with required reviewers. Approve them
independently; approving one channel never publishes the other. Before approving either
environment, require:

1. a successful `preflight` for the exact intended tag, with `verification_only` reported `false`;
2. successful `generate` and the channel's acceptance job for the same run;
3. the recorded `inputs_sha256` matching between the run outputs and the retained artifact.

The `homebrew` environment exposes only the tap-scoped GitHub App credentials
(`HOMEBREW_TAP_APP_ID`, `HOMEBREW_TAP_APP_PRIVATE_KEY`). The `chocolatey` environment exposes only
`CHOCOLATEY_API_KEY`. A workflow run that requests any other secret, or a secret from the wrong
job, fails the tracked workflow policy check.

## Bootstrap the Homebrew tap

Before the first package release, create the tap with a minimal default-branch commit, then land
one reviewed tap-owned change that copies `packaging/homebrew/tap-ci.yml` to
`.github/workflows/tap.yml` and copies `packaging/homebrew/tap/{README,CONTRIBUTING,SECURITY}.md` to
the tap root. The `formulae` job reports this as an unpublished bootstrap and skips Homebrew
lifecycle work only when all four files exist, neither expected Formula exists, no other Ruby
Formula exists, and no Formula has ever existed in the checked-out history.

This allowance is one-way. A partial pair, an extra Formula, or deletion after either Formula has
appeared fails the classifier; it cannot turn a published tap back into the bootstrap state. After
the bootstrap push passes, protect `main`, require the `formulae` check and review, and only then
install the tap-scoped GitHub App or enable the publisher. Record the initial reviewed bootstrap as
the sole pre-protection change.

## Review the paired tap pull request

The Homebrew publisher never pushes the tap's protected default branch. It writes both rendered
Formulae to branch `skillmount/<version>` in `pashifika/homebrew-tap` and opens one pull request
for the pair. Before merging, require:

- both `Formula/skillmount.rb` and `Formula/skillmount-asm.rb` updated together, with identical
  release-archive URL, SHA-256, version, license, and platform requirements;
- each Formula installing only its named archive member and command, with the pair member's command
  appearing only inside the `test do` block;
- the tap CI checks green: `brew style`, `brew audit --strict` for both Formulae, both archive
  installs, both `brew test` runs, selected-only install, co-installation, cross-uninstall,
  completion-ownership checks, and the upgrade rehearsal from the base revision, which self-skips
  with a notice on a first publication;
- provenance comments naming the expected tag and commit from the run's `package-inputs`.

A retried run that finds the branch or pull request already correct resumes it instead of creating
a duplicate; it never force-pushes, closes, or merges an existing pull request.

## Chocolatey moderation and pair eligibility

Before either public package ID exists, confirm with the Chocolatey Community Repository that the
`skillmount` plus `skillmount-asm` pair — two packages with distinct installed executables but
behaviorally equivalent entrypoints — is eligible rather than prohibited duplicate packaging. A
refusal blocks Chocolatey publication entirely; automation must not fall back to one package that
installs both commands, because that denies the selection contract.

After a push is accepted, each package ID is moderated independently. Record `pending` per ID;
pending is upload acceptance, not public availability. A package ID's install command becomes
advertisable only after moderation approves and lists it, its public endpoint resolves `0.2.0`,
and a clean-host selected-only installation passes. One approved member never implies the other.

## Retry a partially published pair

Retry through manual dispatch with the same exact tag and the affected channel. Reconciliation is
pair-aware and idempotent:

- an existing member identical to the expected provenance (release-archive URL, digest, version,
  selected executable) is left unchanged and reported as an idempotent success or status check;
- only an absent member receives creation work, and only when the existing member matches;
- nothing is pushed twice for one ID in one run, and the GitHub release is never touched.

A **conflict** means an existing external version — a tap branch, pull request, Formula, or
Community Repository package — carries different immutable metadata than preflight expects. The
channel job then fails, reports the expected and observed identities for the complete pair, and
performs no write. A conflict is a stop-for-human-review signal: someone or something else owns
that version. Never resolve it by overwriting, unlisting, or repushing; if the published bytes are
wrong, release a new patch version through the normal flow.

## Read per-entry channel status

The `summary` job writes a per-entry table to the run summary. Interpret the states as:

| State | Meaning |
|---|---|
| `created` | The entry was written or pushed for the first time by this run. |
| `resumed` | An existing partial branch or pull request was safely continued. |
| `unchanged` | The entry already matched the expected provenance; nothing was written. |
| `pending` | Chocolatey accepted the upload; moderation has not approved or listed it. |
| `listed` | The Community Repository publicly resolves the approved package version. |

GitHub Release state, the paired tap pull request, each Formula's clean install, each Chocolatey
package's upload/moderation/clean install, and pair co-installation are reported separately. A
failed or pending entry never rewrites another entry's state, and one channel's failure never marks
the other channel's outcome.

## Credential rotation and revocation

Never print, echo, or paste a secret value into a log, issue, or evidence file; reference secrets
by name and rotation date only.

- **Homebrew (tap-scoped GitHub App).** The App is installed only on `pashifika/homebrew-tap`.
  Rotate by generating a new private key in the App settings, updating
  `HOMEBREW_TAP_APP_PRIVATE_KEY` in the `homebrew` environment, and deleting the old key. Revoke by
  deleting the key or suspending/uninstalling the App installation; either action disables only the
  Homebrew lane.
- **Chocolatey (API key).** Rotate from the Chocolatey account's API-key page, update
  `CHOCOLATEY_API_KEY` in the `chocolatey` environment, then invalidate the old key. Revoke the key
  immediately on suspected compromise; the Homebrew lane and the GitHub release are unaffected.

Record every rotation and revocation (date, actor, reason, affected environment) with the release
evidence. Environment reviewer lists are part of the credential boundary: review them whenever
maintainers change.

## Clean-host acceptance evidence

An install command may be advertised only with retained clean-host evidence. For each of the four
entries, the evidence must contain:

- the exact package version, tag, commit, release/archive URLs, and SHA-256 values, plus the Formula
  file or nupkg digest;
- the external identities involved: tap repository and pull request, GitHub App installation,
  Chocolatey account and package ID;
- the workflow run and action revisions, runner labels, and manager/platform versions (Homebrew,
  macOS, and shell versions; Windows, Chocolatey, and PowerShell versions);
- for a Homebrew entry, the exact `brew trust` invocation that preceded the install and the
  observed Homebrew version — Homebrew 6.0.12 proved that installing from an untrusted tap is
  refused, so a transcript without its trust step is not reproducible;
- the public endpoint response resolving `0.2.0` for that entry;
- the clean supported-host install transcript showing the selected-only contract: exactly the
  selected executable and shim or keg present, the pair member's command absent, `--version`
  reporting the expected version, and command-specific completion ownership;
- co-installation and cross-uninstall results for the pair, and the observed cleanup state.

## Release-archive integrity

Both Formulae pin the protected `aarch64-apple-darwin` GitHub Release archive by exact URL and
SHA-256. Package preflight requires the digest published in `SHA256SUMS`, GitHub's asset digest, and
the downloaded bytes to agree before rendering either Formula. Homebrew therefore consumes the same
immutable release bytes as the other package channel; it does not depend on GitHub's generated tag
tarball, a Homebrew Rust toolchain, or a second build. A mismatch blocks publication. Never repair
published metadata or bytes in place; publish a new patch release through the normal flow.

## Operator guidance (not yet available)

None of the four install commands below works today. Each becomes available only when its own
public endpoint resolves version `0.2.0` and a clean selected-only installation passes on a
supported host, per the evidence contract above. Until then, install from
[GitHub Releases](https://github.com/pashifika/skillmount/releases) as described in the
[README](../README.md). Installing both Formulae or both Chocolatey packages together is supported;
each command owns only its own executable, shim, and completion files, and uninstalling one never
removes the other.

Homebrew adds a trust prerequisite to the install path, proven with Homebrew 6.0.12 on `macos-15`:
`brew install` refuses a Formula from an untrusted third-party tap, while `brew style` and
`brew audit` accept one. Trust survives every upgrade of a Formula that stays installed: entries are
name-keyed plain JSON in `${XDG_CONFIG_HOME}/homebrew/trust.json`, or in `~/.homebrew/trust.json`
when `XDG_CONFIG_HOME` is unset (verified on Homebrew 6.0.15).
`brew trust --formula pashifika/tap/<id>` trusts one Formula; `brew trust pashifika/tap` trusts
the whole tap, including Formulae added later.

Uninstalling a Formula drops its trust entry. The acceptance harness proved this on `macos-15`:
after `brew uninstall` removed both Formulae, the read taken immediately before the next install
no longer listed either reference, and the install was refused. Re-trust before reinstalling a
Formula you previously removed, which is why every command block below repeats its trust step and
why the harness re-asserts trust before each install rather than once per run. Trusting the whole
tap avoids the repetition at the cost of trusting Formulae added later.

### Homebrew `pashifika/tap/skillmount` — unavailable

Available once the tap's `skillmount` Formula resolves `0.2.0` publicly and its clean-host
selected-only install passes on Apple Silicon macOS.

```bash
brew trust --formula pashifika/tap/skillmount    # required before install
brew install pashifika/tap/skillmount
brew upgrade pashifika/tap/skillmount
brew uninstall skillmount
```

Installs only the `skillmount` command from the checked Apple Silicon release archive. The Formula
generates Bash, Zsh, and Fish completions by running `skillmount completions <shell>` at install
time and places them in Homebrew-managed completion directories; they register only `skillmount`,
and uninstalling removes only them. No user profile is edited.

### Homebrew `pashifika/tap/skillmount-asm` — unavailable

Available once the tap's `skillmount-asm` Formula resolves `0.2.0` publicly and its clean-host
selected-only install passes on Apple Silicon macOS.

```bash
brew trust --formula pashifika/tap/skillmount-asm    # required before install
brew install pashifika/tap/skillmount-asm
brew upgrade pashifika/tap/skillmount-asm
brew uninstall skillmount-asm
```

Installs only the `asm` command from the checked Apple Silicon release archive. Bash, Zsh, and Fish
completions are generated through `asm completions <shell>`, register only `asm`, and are owned and
removed by this Formula alone. No user profile is edited.

### Chocolatey `skillmount` — unavailable

Available once the Community Repository approves, lists, and publicly resolves `skillmount 0.2.0`
and its clean-host selected-only install passes on supported Windows.

```powershell
choco install skillmount
choco upgrade skillmount
choco uninstall skillmount
```

Downloads and checksum-verifies the matching x86 or x64 release archive, retains only
`skillmount.exe`, and exposes only the ordinary `skillmount` shim. The package installs no
completion files and never edits a PowerShell profile; generate PowerShell completion manually with
`skillmount completions powershell` and dot-source the saved file from your own profile.

### Chocolatey `skillmount-asm` — unavailable

Available once the Community Repository approves, lists, and publicly resolves
`skillmount-asm 0.2.0` and its clean-host selected-only install passes on supported Windows.

```powershell
choco install skillmount-asm
choco upgrade skillmount-asm
choco uninstall skillmount-asm
```

Downloads and checksum-verifies the matching x86 or x64 release archive, retains only `asm.exe`,
and exposes only the ordinary `asm` shim. The package installs no completion files and never edits
a PowerShell profile; generate PowerShell completion manually with `asm completions powershell`
and dot-source the saved file from your own profile.
