# Security policy for the SkillMount tap

> Canonical source: `pashifika/skillmount:packaging/homebrew/tap/SECURITY.md`. Changes are
> transferred to this tap through its own reviewed pull requests.

## Ownership model

This repository is owned and reviewed separately from the SkillMount product repository. Its
default branch is protected: no direct pushes, required CI, and required review. Automation writes
through a GitHub App whose installation is scoped to this repository alone — it holds no
credential for the product repository, the Chocolatey channel, or any other target, and the
product repository's release workflow holds no credential for this tap. Revoking the App's key or
suspending its installation disables automated Formula updates without affecting anything else.

## What the tap distributes

The tap distributes Formula text only. Each Formula pins the immutable Apple Silicon SkillMount
GitHub Release archive by SHA-256 and installs exactly one named executable from it; the tap hosts
no binaries, no bottles, and no install scripts beyond the Formula itself. A Formula never edits
shell profiles and installs completions only for its own selected command.

Package preflight validates the pinned digest against `SHA256SUMS`, GitHub's asset digest, and the
downloaded release bytes before proposing the Formula. A later mismatch is therefore a compromise
or release-integrity signal, not metadata to repair in place; the remedy is a new protected
SkillMount patch release and reviewed Formula update.

## Reporting

- A vulnerability in SkillMount itself — the `asm`/`skillmount` executables or their behavior —
  follows the product repository's security process at
  [pashifika/skillmount](https://github.com/pashifika/skillmount).
- A vulnerability in the tap — a Formula whose release-archive URL or digest does not match the
  corresponding SkillMount release, an unexpected Formula change, or a compromise of tap CI or the
  publishing App — should be reported privately to the tap maintainers through this repository's
  GitHub security advisory form. Include the Formula file, the observed URL and digest, and the
  release you compared against.

Do not open a public issue for a suspected supply-chain mismatch before maintainers have
confirmed it; a mismatched digest is treated as a compromise signal, and the affected Formula is
pulled from review rather than patched in place.
