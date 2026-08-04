# Security policy for the SkillMount tap

> Source material: this file is tracked in `pashifika/skillmount` under `packaging/homebrew/tap/`
> and is transferred to the separately managed `pashifika/homebrew-tap` repository through that
> repository's own reviewed change. It is not the live tap.

## Ownership model

This repository is owned and reviewed separately from the SkillMount product repository. Its
default branch is protected: no direct pushes, required CI, and required review. Automation writes
through a GitHub App whose installation is scoped to this repository alone — it holds no
credential for the product repository, the Chocolatey channel, or any other target, and the
product repository's release workflow holds no credential for this tap. Revoking the App's key or
suspending its installation disables automated Formula updates without affecting anything else.

## What the tap distributes

The tap distributes Formula text only. Each Formula pins one immutable SkillMount release source
tarball (`archive/refs/tags/<tag>.tar.gz`) by SHA-256 and builds it from source on the user's
machine; the tap hosts no binaries, no bottles, and no install scripts beyond the Formula itself.
A Formula never edits shell profiles and installs completions only for its own selected command.

The pinned digest is validated against the released source at publication time. If GitHub later
re-compresses a generated tarball, an already-published digest stops matching; the remedy is a new
SkillMount patch version with a freshly validated digest, never an in-place edit of a published
Formula.

## Reporting

- A vulnerability in SkillMount itself — the `asm`/`skillmount` executables or their behavior —
  follows the product repository's security process at
  [pashifika/skillmount](https://github.com/pashifika/skillmount).
- A vulnerability in the tap — a Formula whose source URL or digest does not match the
  corresponding SkillMount release, an unexpected Formula change, or a compromise of tap CI or the
  publishing App — should be reported privately to the tap maintainers through this repository's
  GitHub security advisory form. Include the Formula file, the observed URL and digest, and the
  release you compared against.

Do not open a public issue for a suspected supply-chain mismatch before maintainers have
confirmed it; a mismatched digest is treated as a compromise signal, and the affected Formula is
pulled from review rather than patched in place.
