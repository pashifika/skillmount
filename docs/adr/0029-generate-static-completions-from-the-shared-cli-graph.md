# ADR 0029: Generate Static Completions from the Shared CLI Graph

- **Status:** Accepted
- **Date:** 2026-08-04
- **Supersedes:** _none_

## Context

The public `asm` and `skillmount` commands expose one shared `clap` model, but operators and package
maintainers had no first-party shell-completion interface. Committed handwritten scripts would
repeat that model, while runtime dynamic completion would execute SkillMount on every tab press and
cross the state-free completion boundary.

`clap_complete` 4.6.8 provides stable ahead-of-time generators under the repository's MSRV and
license policy. Source inspection showed that its PowerShell generator omits possible values and
path hints. Native execution also showed that its Bash, Zsh, and Fish output continues to offer
wrapper options after the session `--` delimiter. Repository-owned generator tests and required
native acceptance jobs cover both compatibility gaps.

## Decision

Both product binaries expose `completions <bash|zsh|fish|powershell>`. `src/completion.rs` rebuilds
the shared command graph from `src/cli.rs`, binds it only to the recognized product name that was
invoked, and emits a static script to stdout before project, catalog, agent, state, lock, journal,
recovery, or process work can begin.

Bash, Zsh, and Fish use the exactly pinned stable `clap_complete` ahead-of-time generators plus
narrow static compatibility code. Every shell suppresses wrapper completion after `--`. Bash
disables Readline's default filename fallback, owns directory and executable-path filtering, and
asks Readline to quote filename metacharacters. Zsh replaces the upstream directory and executable
file completers with strict path-only helpers and keeps an owned guard bound across the pinned
generator's autoload bootstrap. Fish replaces the upstream unfiltered executable hint with an
owned helper that admits only directories and executable files. PowerShell uses a
SkillMount-owned static generator over the same `clap::Command` graph
because the pinned upstream generator cannot preserve the required enum and path metadata. It
derives state only from command elements before the active cursor and escapes filesystem candidates
as literal PowerShell argument text. The supported-shell enum remains SkillMount-owned and closed;
no generated completer invokes SkillMount, an agent, or another process.

## Alternatives

- Commit complete handwritten scripts. Rejected because four copies of the command model would drift
  from parser visibility, aliases, possible values, and path metadata.
- Generate scripts in `build.rs`. Rejected because build-time code cannot naturally consume the
  product crate's private command factory and still would not install the output for an operator.
- Enable the dependency's unstable dynamic engine. Rejected because it executes callbacks or
  binaries during completion and expands the dependency and lifecycle contract.
- Use the pinned PowerShell generator unchanged. Rejected because it cannot satisfy the advertised
  enum and directory/executable-path cases.
- Let upstream generators complete wrapper options after `--`. Rejected because agent arguments are
  opaque platform-native values and SkillMount has no certified agent-specific completion model.
- Register both executable names from every generated file. Rejected because completion files are
  conventionally command-specific and an output must not silently claim a command the operator did
  not invoke.

## Consequences

- Operators and package maintainers must capture stdout and install or source the generated file for
  the same executable name. SkillMount does not edit shell profiles or choose installation paths.
- Adding a shell, changing registration-name policy, or replacing the generator boundary is a public
  CLI decision requiring synchronized parser, native acceptance, documentation, and packaging work.
- `clap_complete = 4.6.8` is a normal dependency with default and unstable features disabled. It
  adds no dependency beyond the already-used `clap` family; optimized size deltas are measured
  against the `v0.1.0` release artifacts for all three supported target builds.
- The repository owns a small amount of shell-specific compatibility code. The exact dependency pin
  and native behavior jobs make upstream drift explicit instead of silently changing output.
- Manual installation documentation, the architecture baseline, read-only tests, and native CI are
  updated in the same change. Downstream Homebrew and Chocolatey changes remain separate packaging
  decisions.

## Verification

- `src/cli.rs` parser tests enforce the fixed shell set, recognized platform-native product names,
  native path types, and opaque passthrough values.
- `tests/completions.rs` exercises both shipped binaries across all four shells, exact registration,
  deterministic bytes, missing source independence, and stable failure categories.
- `tests/read_only.rs` proves generation ignores corrupt state, broken discovery links, active locks,
  and process sentinels without changing any watched path.
- `src/app.rs` injected-writer tests enforce successful BrokenPipe handling and category 70 for other
  output failures.
- `.github/scripts/shell_completion_acceptance.py` and its self-tests exercise syntax, exact visible
  candidate sets, completed and invalid prefixes, directory and executable hints (including quoted
  prefixes, strict type filtering, traversal, no-match behavior, and literal metacharacters),
  cold-start and cursor-relative `--` handling, isolated-home containment, deterministic
  observations, owned cleanup, and unavailable-shell failure.
- `.github/workflows/ci.yml` runs Bash, Zsh, and Fish on pinned Apple Silicon macOS shell formulae,
  repeats Bash coverage with the system Bash 3.2, and runs PowerShell for both supported Windows
  target binaries, with every job required by the aggregate gate.
