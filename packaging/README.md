# SkillMount package-channel sources

Reviewable, token-substituted sources for the two first-party distribution channels. Nothing
here is a build input for the Cargo crate, and nothing here changes a release asset. These
files are rendered into channel-ready artifacts by `.github/scripts/package_channels.py`, which
`.github/workflows/package.yml` runs after a release has already been published.

## Layout

| path | purpose |
| --- | --- |
| `homebrew/skillmount.rb.in` | Formula template for the `skillmount` package |
| `homebrew/skillmount-asm.rb.in` | Formula template for the `skillmount-asm` package |
| `homebrew/tap-ci.yml` | Workflow the tap repository installs; not registered here |
| `chocolatey/skillmount/` | Chocolatey package source for the `skillmount` package |
| `chocolatey/skillmount-asm/` | Chocolatey package source for the `skillmount-asm` package |

The two Formula templates and the two Chocolatey template trees are byte-identical to their pair
member. Every difference between the two published packages is a substituted token, which is
what makes "these two packages differ only in their selection" a mechanically checkable claim
rather than a review promise.

## The two packages

SkillMount ships two behaviourally identical executables, `asm` (primary) and `skillmount`
(fallback), and `src/cli.rs` resolves the product identity from `argv[0]`, rejecting a renamed
alias. A package manager must therefore install exactly one of them under exactly its own name,
never a shim or symlink under another name.

| package id | cargo `--bin` | command | Windows executable | formula |
| --- | --- | --- | --- | --- |
| `skillmount` | `skillmount` | `skillmount` | `skillmount.exe` | `Formula/skillmount.rb` |
| `skillmount-asm` | `asm` | `asm` | `asm.exe` | `Formula/skillmount-asm.rb` |

Both packages may be installed side by side. Neither declares a conflict with the other, and
neither depends on the other.

## Template tokens

A template may use only the tokens listed for it, and must use every one of them.
`package_channels.render_template` fails closed on both halves of that rule: an unknown token in
the text, an unused value, an empty value, or a value containing a newline is an error rather
than a silently mis-rendered package. `test_package_workflow_policy.py` asserts the token sets
independently, so drift is caught before a workflow ever runs.

| template | tokens |
| --- | --- |
| `homebrew/<id>.rb.in` | `FORMULA_CLASS`, `PACKAGE_ID`, `DESCRIPTION`, `HOMEPAGE`, `SOURCE_URL`, `SOURCE_SHA256`, `VERSION`, `LICENSE`, `CARGO_BIN`, `COMMAND`, `OTHER_COMMAND`, `TAG`, `COMMIT` |
| `chocolatey/<id>/<id>.nuspec.in` | `PACKAGE_ID`, `VERSION`, `TITLE`, `SUMMARY`, `DESCRIPTION`, `PROJECT_URL`, `PROJECT_SOURCE_URL`, `LICENSE_URL`, `RELEASE_NOTES_URL`, `COMMAND`, `TAG` |
| `chocolatey/<id>/tools/chocolateyinstall.ps1.in` | `PACKAGE_ID`, `VERSION`, `TAG`, `COMMAND`, `SELECTED_EXECUTABLE`, `OTHER_EXECUTABLE`, `URL_X86`, `SHA256_X86`, `URL_X64`, `SHA256_X64`, `ARCHIVE_ROOT_X86`, `ARCHIVE_ROOT_X64` |

`homebrew/tap-ci.yml` is a finished workflow, not a template, and contains no token.

## Homebrew

Each Formula builds from the GitHub source tarball for one exact tag, requires macOS on arm64,
and installs one binary:

```ruby
system "cargo", "install", "--bin", "@CARGO_BIN@", *std_cargo_args
```

Do not add `--locked` to that call. `std_cargo_args` already expands to
`--locked --root=<keg> --path=.`, and Cargo rejects a repeated flag with
`error: the argument '--locked' cannot be used multiple times`, which fails the install after
Homebrew has already poured every build dependency. The lock file is still honoured, by
`std_cargo_args`.

`generate_completions_from_executable` is called with an explicit `base_name:`. Homebrew
otherwise defaults `base_name:` to the formula name, which would make the `skillmount-asm`
formula register completions for the command `skillmount-asm`, a name the product rejects.

`test do` asserts the reported version, that `--help` succeeds, that the pair member's
executable is absent from `bin`, and that each generated completion names this package's command
and never the pair member's. The pair member's command therefore appears only inside `test do`;
`package_channels.inspect_formulae` enforces that, along with the exact `depends_on` set and the
absence of `conflicts_with`.

The tap lives in a separate repository, `pashifika/homebrew-tap`, which Homebrew addresses as the
tap reference `pashifika/tap`. Homebrew 6 refuses to load a formula from a non-official tap until
it is trusted, so an install is a trust, install sequence rather than one command. Trust is
recorded by name in `${XDG_CONFIG_HOME}/homebrew/trust.json`, or `~/.homebrew/trust.json`. It
survives later versions of an installed formula, but `brew uninstall` drops the entry, so a
reinstall must trust it again — which is why `homebrew_acceptance.py` re-asserts trust before every
install:

```sh
brew trust --formula pashifika/tap/skillmount pashifika/tap/skillmount-asm
brew install pashifika/tap/skillmount
brew install pashifika/tap/skillmount-asm
```

`brew trust --formula` only records names, so it needs no prior `brew tap`, and `brew install`
auto-taps the reference it resolves.

Neither command is available yet; see `docs/packaging.md` for the gating rule.

`homebrew/tap-ci.yml` is that repository's own CI. It is deliberately self-contained — only
`brew`, `git`, and the system Python — so the tap never depends on a script that lives here. It
runs `brew style`, `brew audit --strict` for both Formulae, an explicit per-formula `brew trust`
because `brew install` would otherwise refuse the tap, both source builds, both `brew test`,
selected-only install and uninstall checks, co-installation, cross-uninstall, completion
ownership, and an upgrade rehearsal from the pull request's base revision.

## Chocolatey

One release archive carries both executables, so `chocolateyinstall.ps1` must keep one and
discard the other. It runs under `$ErrorActionPreference = 'Stop'` and
`Set-StrictMode -Version 2`, and:

1. refuses to install over an existing product executable in `tools`;
2. selects the expected archive root for the running architecture, honouring
   `$env:ChocolateyForceX86` the same way `Get-ChocolateyWebFile` does;
3. downloads with `Get-ChocolateyWebFile`, passing both architecture URLs and both SHA-256
   checksums, so Chocolatey verifies the bytes before anything is extracted;
4. extracts with `Get-ChocolateyUnzip` into a package-owned directory under `$env:TEMP` — never
   into `tools`, because Chocolatey discovers shim candidates there and would otherwise expose
   the executable this package did not select;
5. requires the extracted root to be exactly the expected name and to contain **both**
   executables plus `LICENSE-APACHE`, `LICENSE-MIT`, and `VERSION`, proving the dual-binary
   release contract from the extracted bytes;
6. copies only the selected executable and `VERSION` into `tools`; and
7. removes the temporary directory in a `finally` block.

It never writes an `.ignore` file, never edits `$PROFILE`, never calls `Install-ChocolateyPath`,
never requests elevation, and never references the unselected executable after validation.
`Install-ChocolateyZipPackage` is not used, because it extracts into `tools`.

`package_channels.generate_chocolatey_sources` adds `LICENSE-APACHE`, `LICENSE-MIT`, and a
generated `VERIFICATION.txt` to each package's `tools` directory. `inspect_nupkg` then opens each
packed `.nupkg` as a ZIP, without extracting, and requires the exact member set, no executable or
archive member, no absolute or traversing member name, and both architecture URLs and digests
equal to the validated release inputs.

## Rendering and checking locally

Generation needs a validated `inputs.json`, which `package_channels.py preflight` produces from a
published release. Given one:

```sh
python -B .github/scripts/package_channels.py generate-homebrew \
  --inputs inputs.json --template-directory packaging/homebrew --output-directory candidates
python -B .github/scripts/package_channels.py generate-chocolatey \
  --inputs inputs.json --template-directory packaging/chocolatey --output-directory candidates
python -B .github/scripts/package_channels.py inspect-homebrew \
  --inputs inputs.json --directory candidates
python -B .github/scripts/package_channels.py inspect-chocolatey \
  --inputs inputs.json --directory candidates
```

The token sets, the pair-identity of the templates, and the safety properties listed above are
also checked without a release, on any platform, with no `brew`, no `choco`, and no network:

```sh
python -B .github/scripts/test_package_workflow_policy.py
```
