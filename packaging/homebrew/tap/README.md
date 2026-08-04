# SkillMount Homebrew tap

> Source material: this file is tracked in `pashifika/skillmount` under `packaging/homebrew/tap/`
> and is transferred to the separately managed `pashifika/homebrew-tap` repository through that
> repository's own reviewed change. It is not the live tap.

This tap publishes the two SkillMount Formulae. SkillMount is a Rust wrapper CLI that mounts
external Agent Skills into one Codex CLI or Claude Code session; the product itself lives at
[pashifika/skillmount](https://github.com/pashifika/skillmount).

## What the tap publishes

One product, two selectable Formulae. Each Formula consumes the same checked Apple Silicon release
archive and installs exactly one of the product's two equivalent commands:

| Formula | Installs | Completions installed for |
|---|---|---|
| `skillmount` | the `skillmount` command | `skillmount` (Bash, Zsh, Fish) |
| `skillmount-asm` | the `asm` command | `asm` (Bash, Zsh, Fish) |

Neither Formula installs, depends on, or conflicts with the other. Install either one or both;
each owns only its own executable and completion files, and uninstalling one never touches the
other. Completions are generated at install time by running the installed command's own
`completions` subcommand; no user profile is edited.

## Supported platform

Apple Silicon macOS only, installed from the protected GitHub Release archive. Linuxbrew, macOS
Intel, casks, and bottles are not supported.

## Install

Not yet available: the tap has not published its first Formula version. Each command below becomes
available once its Formula resolves version `0.2.0` in this tap and a clean installation passes on
a supported host. Homebrew refuses to install a Formula from an untrusted third-party tap, so
trust each Formula once before its first install — trust is keyed by name, so upgrades never
re-prompt, and `brew trust pashifika/tap` trusts the whole tap instead. Trusted entries are stored
in `${XDG_CONFIG_HOME}/homebrew/trust.json`, or in `~/.homebrew/trust.json` when `XDG_CONFIG_HOME`
is unset.

```bash
brew trust --formula pashifika/tap/skillmount        # required before install
brew install pashifika/tap/skillmount                # not yet available
brew trust --formula pashifika/tap/skillmount-asm    # required before install
brew install pashifika/tap/skillmount-asm            # not yet available
```

## How Formulae change

Formula updates are proposed by SkillMount's release automation, which opens one pull request per
version updating both Formulae together from the same verified release archive and SHA-256.
Automation authenticates as a GitHub App installed only on this repository and never pushes the
protected default branch. Every pull request must pass the tap CI checks — `brew style`,
`brew audit --strict` for both Formulae, both archive installs, both `brew test` runs,
selected-only install, co-installation, cross-uninstall, completion-ownership checks, and an
upgrade rehearsal from the base revision — before merge. See
[CONTRIBUTING.md](CONTRIBUTING.md).

## Reporting a problem

- A Formula problem — the archive cannot be installed, `brew install`/`upgrade`/`uninstall`
  misbehaves, a completion file is wrong or misplaced, or Formula metadata is incorrect — belongs
  in this repository's issue tracker.
- A product bug — an installed `asm` or `skillmount` command behaves incorrectly — belongs in
  [pashifika/skillmount issues](https://github.com/pashifika/skillmount/issues).
- A security concern of either kind follows [SECURITY.md](SECURITY.md).
