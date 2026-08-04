# ADR 0031: Use Release Archives for Homebrew Formulae

- **Status:** Accepted
- **Date:** 2026-08-05
- **Supersedes:** ADR 0030's source-built Homebrew Formula decision only

## Context

ADR 0030 made both Homebrew Formulae build from GitHub's generated tag tarball. That left two
avoidable failure classes: GitHub may recompress that tarball after publication and invalidate its
pinned digest, and Homebrew's Rust toolchain may drift or fail independently of the binaries already
validated for the release. The source builds also download large build dependencies and duplicate
work already performed by the protected Release workflow.

The existing Apple Silicon release archive has the layout Homebrew needs: one top-level directory
containing `asm`, `skillmount`, both license files, and `VERSION`. Homebrew 6.0.15 changes into that
single directory before running `install`. In a disposable tap, two Formulae pointing at the real
`v0.1.0` Apple Silicon release asset and its published SHA-256 both passed `brew style --formula`
and `brew audit --strict --formula` after expressing the dual license through Homebrew's supported
`license any_of: ["MIT", "Apache-2.0"]` DSL. The same audit rejected the previous literal
`"MIT OR Apache-2.0"` as a non-standard SPDX identifier.

## Decision

Both Homebrew Formulae SHALL consume the immutable `aarch64-apple-darwin` GitHub Release archive
already validated by package preflight. Each Formula SHALL pin that asset's SHA-256, install only its
selected executable, retain the release license and version files as package-owned data, and
generate completions from the installed command. Formulae SHALL NOT download the generated source
tarball, invoke Cargo, or declare Rust as a build dependency.

This replaces only ADR 0030's source-build decision. Its separate workflow, protected tap pull
request, trust requirement, selected-command pair, credential isolation, and fail-closed
reconciliation decisions remain in force.

## Alternatives

- Keep source-built Formulae. Rejected because this preserves the generated-tarball recompression
  risk, toolchain drift, large build downloads, and duplicate release work without improving the
  supported platform or selected-command contract.
- Add Homebrew bottles for the source Formulae. Rejected because bottles introduce another
  published artifact and provenance path when the protected release archive already contains the
  required binary and metadata.
- Use a Cask. Rejected because SkillMount is an open-source CLI and a Cask would add macOS
  quarantine and notarization behavior without solving a product requirement.
- Copy the direct-to-default-branch publication pattern used by simpler binary Formulae. Rejected;
  ADR 0030's reviewed, pair-aware tap change remains necessary to detect conflicts and preserve tap
  CI as an independent gate.

## Consequences

- Homebrew and Chocolatey now consume binaries from the same protected GitHub Release identity;
  Homebrew uses the Apple Silicon archive while Chocolatey selects the checked Windows archive.
- The selected-only Homebrew invariant becomes structural: `bin.install` names exactly one archive
  member, and no Cargo target-selection command remains.
- Homebrew installs no Rust or LLVM build dependency, and native package CI no longer performs two
  source builds. Operators also lose the optional local source build previously performed by the
  Formula.
- Apple Silicon macOS remains the only Homebrew target. Adding Intel or Linux requires a matching
  protected release target and a separate supported-platform decision.
- Formula metadata and reconciliation compare the immutable release-asset URL and digest. The
  generated source-tarball recompression risk and the bottle question recorded by ADR 0030 are
  removed.
- The architecture baseline, package runbook, templates, generator, publisher, acceptance harness,
  tap material, and Rasen specification change together.

## Verification

- `.github/scripts/test_package_channels.py` verifies that both Formulae render the validated
  Apple Silicon archive URL and SHA-256, use Homebrew's dual-license DSL, install only the selected
  binary, retain no Cargo build path, and pass structural pair inspection.
- `.github/scripts/test_package_publish.py` verifies pair reconciliation against archive URL,
  digest, version, installed binary, and generated-completion command.
- `.github/scripts/test_homebrew_acceptance.py` verifies local release-archive construction and the
  native harness safety boundary. The guarded macOS acceptance job runs `brew style`, strict audit,
  install, test, selected-only, completion, co-installation, upgrade, and cross-uninstall phases.
  The upgrade rehearsal self-skips with a recorded reason while the only prior release, `v0.1.0`,
  predates the public completion command the Formula requires, so that phase stays unexercised
  until a completion-capable release exists to upgrade from.
- `.github/scripts/package_workflow_policy.py check` continues to prove that release bytes never
  enter either credentialed publisher job.
