# SkillMount

Mount external Agent Skills into one Codex CLI, Claude Code, or Oh My Pi session, then clean up
automatically.

Codex CLI, Claude Code, and Oh My Pi discover Skills only in their own directories. When your
Skills live in a shared or external folder, you end up copying them into every project and
removing them by hand.
SkillMount removes that chore. It links the Skills you select into the place the agent searches,
launches the agent for you, and deletes exactly the links it created once the session ends.

One package installs two identical commands: `asm`, the primary name used in every example below,
and `skillmount`, a descriptive fallback that behaves the same way.

## Contents

- [Features](#features)
- [Install](#install)
- [Quick start](#quick-start)
- [Oh My Pi sessions](#oh-my-pi-sessions)
- [Concurrent sessions](#concurrent-sessions)
- [Commands and options](#commands-and-options)
- [Shell completion](#shell-completion)
- [Health checks and cleanup](#health-checks-and-cleanup)
- [Requirements](#requirements)
- [Documentation](#documentation)
- [Contributing](#contributing)
- [License](#license)

## Features

- **Session-scoped mounts:** Skills appear when the agent starts and disappear when it exits, so
  your project directory stays clean.
- **Layered catalogs (Codex/Claude):** repeat `--skills-dir` to stack Skill collections; the rightmost source
  wins each Skill name.
- **Conflict safety:** an existing Skill is never replaced, and a name clash stops the session
  before anything changes.
- **Crash recovery:** every change is journaled, so `asm doctor` can diagnose an interrupted
  session and `asm cleanup` can reconcile what it left behind.
- **Clean launches:** the agent starts directly, without a shell in between, and its output
  streams pass through untouched, so JSON pipelines keep working.

## Install

### Homebrew on Apple Silicon

Homebrew is the fastest installation path on supported Apple Silicon Macs. Install the primary
`asm` command:

```bash
brew trust --formula pashifika/tap/skillmount-asm
brew install pashifika/tap/skillmount-asm
asm --version   # prints: SkillMount 0.2.0
```

Or install the descriptive `skillmount` command:

```bash
brew trust --formula pashifika/tap/skillmount
brew install pashifika/tap/skillmount
skillmount --version   # prints: SkillMount 0.2.0
```

Each Formula installs only the command you selected and generates its Bash, Zsh, and Fish
completions. Installing both Formulae together is supported. Homebrew requires the explicit trust
step for this third-party tap; uninstalling a Formula removes its trust entry, so repeat that step
before reinstalling it.

### Download a release

Each release archive contains both commands plus the license and version files, and the release
publishes `SHA256SUMS` digests next to the archives. On macOS (Apple Silicon):

```bash
curl -LO https://github.com/pashifika/skillmount/releases/download/v0.2.0/skillmount-v0.2.0-aarch64-apple-darwin.tar.gz
tar -xzf skillmount-v0.2.0-aarch64-apple-darwin.tar.gz
skillmount-v0.2.0-aarch64-apple-darwin/asm --version   # prints: SkillMount 0.2.0
```

On Windows x64 (use the `i686` archive for x86):

```powershell
Invoke-WebRequest https://github.com/pashifika/skillmount/releases/download/v0.2.0/skillmount-v0.2.0-x86_64-pc-windows-msvc.zip -OutFile skillmount.zip
Expand-Archive skillmount.zip .
.\skillmount-v0.2.0-x86_64-pc-windows-msvc\asm.exe --version   # prints: SkillMount 0.2.0
```

Move `asm` — and `skillmount`, if you want the fallback name — into a directory on your `PATH`.
Keep each executable under its own name; SkillMount rejects a renamed copy.

### Chocolatey (not yet available)

The Chocolatey packages have passed native package CI but are not public yet. These commands remain
unavailable until their individual Community Repository approvals and clean Windows installations
pass:

| Command | Would install | Status |
|---|---|---|
| `choco install skillmount` | `skillmount` on Windows x64/x86 | Not yet available |
| `choco install skillmount-asm` | `asm` on Windows x64/x86 | Not yet available |

Each package will own only its selected executable and ordinary Chocolatey shim. Upgrade,
uninstall, and completion details for every channel live in
[docs/packaging.md](docs/packaging.md).

### Build from source

Requires Rust 1.85.0 or newer:

```bash
git clone https://github.com/pashifika/skillmount.git
cd skillmount
cargo build --locked --release --bins
target/release/asm --version
```

## Quick start

A `--skills-dir` value is one Skill directory or a folder whose immediate children are Skills.
Start by checking what the agents would see, without touching anything:

```bash
asm inspect --skills-dir ~/agent-skills
```

Preview a Codex session plan, still read-only:

```bash
asm codex --skills-dir ~/agent-skills --dry-run -- exec "Summarize this project's build steps"
```

The preview prints the adapter's dated last-tested banner but does not query the installed Agent.

Run the session for real. SkillMount mounts the Skills, launches Codex, and cleans up after the
Agent exits. Normal sessions neither query the installed Agent banner nor emit a version
compatibility warning; use `asm doctor` when you need local version evidence:

```bash
asm codex --skills-dir ~/agent-skills -- exec "Summarize this project's build steps"
```

Claude Code works the same way. SkillMount stages the Skills in a private directory and hands it
to Claude through `--add-dir`, so the project itself is not modified:

```bash
asm claude --skills-dir ~/agent-skills -- -p "Review the staged diff with the team review Skill"
```

Oh My Pi receives native `skills.customDirectories` through a temporary session configuration;
no Skill links or project settings are written:

```bash
asm omp --skills-dir ~/agent-skills -- -p "Summarize this project's build steps"
```

Everything after the standalone `--` is passed to the Agent unchanged. Repeat `--skills-dir` to
overlay collections, and add `--agent-bin path/to/codex` to select a specific executable instead of
searching `PATH`.

Treat mounted Skills as code you chose to run: review their contents and provenance before making
them visible to an agent.

## Oh My Pi sessions

`asm omp` and `skillmount omp` launch a new supervised foreground OMP session. Unlike
Codex/Claude, OMP owns Skill discovery, selection, filters, aliases and duplicate resolution.
SkillMount adds collection roots through OMP's native `skills.customDirectories`, using a private
session-only `--config` file. It never edits your `.omp/config.yml` or creates `.omp/skills` links.

```bash
asm omp --skills-dir ~/agent-skills -- -p "Summarize this project's build steps"
asm omp --skills-dir ~/rust-skills --skills-dir ~/fe-skills -- --skills 'rust-*'
asm omp --skills-dir ~/agent-skills --dry-run
asm inspect --agent omp --skills-dir ~/agent-skills
asm doctor --project-root . --omp-bin /opt/homebrew/bin/omp
```

Already configured directories are preserved first; CLI roots follow in their declared order.
OMP scans immediate `<root>/<entry>/SKILL.md` children, not individual `SKILL.md` roots or nested
collections. Pass the containing collection, not an individual Skill. OMP gives the first custom
occurrence its bare name; differing duplicates can remain under namespaced aliases, while identical
content can collapse. **OMP does not use SkillMount's Codex/Claude rightmost-wins or conflict policy.**
Default `inspect` covers all agents: an individual Skill input still reports Codex/Claude, but
returns a partial failure naming OMP's collection-only requirement. Use a collection for all-agent
success, or `inspect --agent codex` / `--agent claude` for a direct Skill.

Configured filters and `--skills`/`--no-skills` remain OMP policy; SkillMount does not promise that
every directory entry will load or that only one duplicate remains visible.

For example, existing project configuration needs no links and is not rewritten:

```yaml
skills:
  customDirectories:
    - ~/Work/omp-skills/rust-skills
    - ~/Work/omp-skills/fe-skills
```

Because OMP replaces arrays across settings layers, SkillMount snapshots the effective initial
custom-directory array before appending CLI roots. Changes to that array during the session take
effect in a **new** wrapped session; other settings retain OMP's own behavior. Projection is bounded:
plain YAML/JSON/TOML settings only, at most 1 MiB per file and 1024 combined directories. Unsupported
OpenCode JSONC syntax/substitutions, malformed settings, and unmigrated legacy settings fail with an
explanation rather than silently discarding configuration. Start OMP directly for unsupported input.

OMP uses private staging only. Non-default `--link-mode`, `--conflict=skip`, `--validation`, and
`--keep-mounts` are rejected rather than ignored. Root/profile/config relocation, extension-changing
CLI flags, session resume/fork/restore/import, server modes and operator subcommands remain rejected.
A home-directory launch requires your own `--allow-home`. Model, prompt, filters and approval flags
are forwarded unchanged; SkillMount does not add permissions, authentication or auto-approval.

The owned config stays available until the entire managed process domain is dead, including
reloading and background children. Cleanup checks the exact recorded file identity and content;
changed or replaced files are retained with recovery evidence, never removed as user-owned data.
Old link-based journals remain readable for cleanup, but older binaries cannot clean the new config
operation: finish or recover active sessions with this release before downgrading.
See [the native-directory decision](docs/adr/0040-delegate-omp-discovery-through-owned-config.md)
and [compatibility evidence](docs/compatibility.md).

## Concurrent sessions

Independent project sessions can overlap, even when they inspect the same user, administrator,
settings, plugin, compatibility, or custom Skill roots. Each invocation stabilizes its own current
namespace before planning, so a later session applies `--conflict=error` or `--conflict=skip` to
what exists at its launch. An already-running Agent is not guaranteed to receive a live update;
start a new session when it must see newly added Skills.

Logical or physical destination overlap is still exclusive. A contender waits for the active
session to release the destination and, if contention remains, exits with status 75 before applying
anything. Physical aliases such as directory links do not bypass that protection, and one session
never uses shared discovery access as authority to clean another session's entries.

## Commands and options

```
codex         Run a Codex session with the selected Skills
claude        Resolve Skills for a future Claude Code session
omp           Run an Oh My Pi session with the selected Skills
completions   Generate a shell completion script on standard output
inspect       Inspect and validate a catalog without modifying the filesystem
doctor        Inspect agent, discovery, link, lock, and transaction health
cleanup       Reconcile residue from durable transaction evidence
```

The session commands `codex`, `claude`, and `omp` share these options:

```
--skills-dir <PATH>    Skill directory or direct Skill; repeat for a rightmost-wins overlay
--cwd <PATH>           Working directory for the selected agent process
--project-root <PATH>  Explicit project root
--agent-bin <PATH>     Explicit agent executable path
--link-mode <MODE>     Link implementation: auto, symlink, junction [default: auto]
--mount-mode <MODE>    Mount location strategy: auto, project, staging [default: auto]
--conflict <POLICY>    Existing-destination policy: error, skip [default: error]
--validation <LEVEL>   Metadata validation policy: basic, strict, none [default: basic]
--dry-run              Keep later planning read-only
--keep-mounts          Retain later session mounts for diagnostics
--no-recover           Disable later stale-transaction recovery
-v, --verbose          Increase diagnostic verbosity
```

`--conflict skip` keeps an existing Skill and quietly drops the colliding mount instead of
failing. `--validation none` relaxes metadata checks only; structural and safety checks always
run. `omp` mounts only into the project scope, so `--mount-mode=staging` is rejected as a usage
error.

## Shell completion

`asm completions <shell>` writes one static completion script to standard output and changes
nothing else. Supported shells are `bash`, `zsh`, `fish`, and `powershell`. Place the script where
your shell loads completions from:

```bash
mkdir -p ~/.local/share/skillmount ~/.zfunc ~/.config/fish/completions
asm completions bash > ~/.local/share/skillmount/asm.bash   # then source it from ~/.bashrc
asm completions zsh > ~/.zfunc/_asm                         # keep ~/.zfunc on fpath before compinit
asm completions fish > ~/.config/fish/completions/asm.fish  # Fish loads it automatically
```

On PowerShell, save the script and dot-source it from your profile:

```powershell
asm completions powershell | Set-Content -LiteralPath "$HOME\asm-completion.ps1"
Add-Content -LiteralPath $PROFILE -Value '. "$HOME\asm-completion.ps1"'
```

Each script is bound to the command that generated it: run `skillmount completions` for the
fallback name. Regenerate the files after upgrading SkillMount. Completion is static — pressing
Tab never runs SkillMount or an agent, and arguments after the session `--` are left to the
agent's own completer.

## Health checks and cleanup

```bash
asm doctor --project-root ~/projects/webapp
asm cleanup --project-root ~/projects/webapp
```

`doctor` reports Agent version evidence, discovery layout, link capability, locks, and leftover
journals without changing project or SkillMount state. The last-tested banner is a `pass`; a
different or unavailable banner is `unverified` and does not fail `doctor` unless another check
fails. `cleanup` releases mounts left behind by an interrupted session; run it only after confirming
no Agent process still uses them, or sweep every recorded transaction with
`asm cleanup --all`. The recovery rules and their guarantees are documented in
[docs/architecture.md](docs/architecture.md).

When one or more Skill links genuinely cannot be released, the session first reports every
condition once on stderr, then emits one recovery footer. A direct Windows invocation whose
ancestry identifies PowerShell receives a command whose path is a single-quoted literal:

```text
error: session cleanup failed
  reason: a regular directory replaced the entry, so it cannot be proved to belong to this session and was left untouched
  retained path: C:\projects\O'Brien webapp\.omp\skills\team-review
  retained journal: C:\Users\me\AppData\Local\skillmount\transactions\<id>.journal

Recovery — run only after confirming that every related Agent process has exited:
  command 1 (PowerShell):
    asm cleanup --project-root 'C:\projects\O''Brien webapp'
```

PowerShell emits only the narrow ASCII safe-word set `[A-Za-z0-9_.-]+` unquoted and uses `'...'`
for every other accepted value; an apostrophe inside the value becomes `''`. A direct Command Prompt
invocation uses its independently verified double-quoted form instead:

```text
  command 1 (Command Prompt):
    asm cleanup --project-root "C:\projects\web app"
```

The executable is `skillmount` when that is the recognized invoked product name and `asm`
otherwise. If startup ancestry is absent or ambiguous, or a native value cannot be represented
losslessly in the selected shell, SkillMount prints the authoritative native values instead of
guessing:

```text
  command 1 (native argument vector):
    executable: asm
    argument 1: cleanup
    argument 2: --project-root
    argument 3: /projects/webapp
```

SkillMount never stores or executes the displayed shell line. Recovery authority still comes from
the separate native values and the operator's confirmation that every related Agent process has
exited.

## Requirements

- Supported hosts: Windows 10 version 1709 or later (x64 and x86) and macOS on Apple Silicon.
- The adapters' dated last-tested banners are `codex-cli 0.146.0`, `2.1.220 (Claude Code)`, and
  `omp/18.14.0`. These are evidence baselines, not an exact-version allowlist. Normal sessions do
  not query or warn about the installed banner; `asm doctor` classifies a different or unavailable
  banner as `unverified` until the live-agent workflow is recorded in
  [docs/compatibility.md](docs/compatibility.md).
- Release-independent discovery, configuration, and foreground-lifecycle controls still fail
  closed independently of banner evidence.
- Building from source requires Rust 1.85.0 or newer.

## Documentation

| Document | What it covers |
|---|---|
| [docs/architecture.md](docs/architecture.md) | Module boundaries, safety invariants, implementation status |
| [docs/compatibility.md](docs/compatibility.md) | Last-tested Agent evidence and dated observations |
| [docs/packaging.md](docs/packaging.md) | Package channels, availability, maintainer runbook |
| [docs/releasing.md](docs/releasing.md) | Building and publishing a stable release |

## Contributing

Read [CONTRIBUTING.md](CONTRIBUTING.md) for the branch and review workflow. Before opening a pull
request, run the required checks:

```bash
SKILLMOUNT_REQUIRE_LINKS=1 cargo test --locked --all-targets
cargo test --locked --all-features
cargo clippy --locked --all-targets --all-features -- -D warnings
cargo fmt --all -- --check
env RUSTDOCFLAGS="-D warnings" cargo doc --locked --no-deps --all-features
cargo deny --locked check
```

## License

Licensed under either of the [Apache License 2.0](LICENSE-APACHE) or the [MIT License](LICENSE-MIT),
at your option.
