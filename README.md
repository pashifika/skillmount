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
- **Layered catalogs:** repeat `--skills-dir` to stack Skill collections; the rightmost source
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

Oh My Pi mounts into the project's own `.omp/skills`, the first place OMP searches, and removes
the links it created when the session ends:

```bash
asm omp --skills-dir ~/agent-skills -- -p "Summarize this project's build steps"
```

Everything after the standalone `--` is passed to the Agent unchanged. Repeat `--skills-dir` to
overlay collections, and add `--agent-bin path/to/codex` to select a specific executable instead of
searching `PATH`.

Treat mounted Skills as code you chose to run: review their contents and provenance before making
them visible to an agent.

## Oh My Pi sessions

`asm omp` and `skillmount omp` support exactly one scope: a new foreground Oh My Pi (OMP) session
that SkillMount launches itself. The selected Skills are linked into `<launch CWD>/.omp/skills` —
the first place OMP searches — and SkillMount creates the `.omp` and `.omp/skills` directories
when they are missing, then removes every link it created after OMP exits:

```bash
asm omp --skills-dir ~/agent-skills -- -p "Summarize this project's build steps"
skillmount omp --skills-dir ~/agent-skills -- --mode json "Summarize this project's build steps"
```

Preview and health checks stay read-only, like the other agents:

```bash
asm inspect --agent omp --skills-dir ~/agent-skills
asm omp --skills-dir ~/agent-skills --dry-run
asm doctor --project-root . --omp-bin /opt/homebrew/bin/omp
```

Because the mount destination outranks every other Skill source in OMP's own precedence,
SkillMount checks the complete OMP namespace before touching anything: all nine of OMP's Skill
providers — including `.claude/skills`, marketplace plugin caches, `.agents/skills`, and
`.codex/skills` — plus your `skills.customDirectories`. A provider is a Skill source, not a single
directory: most contribute both a user root and a project root, so even a minimal project renders
fourteen scanned roots under `asm omp --dry-run --verbose`, and project ancestors or installed
plugins add more. SkillMount never replaces an existing Skill: `--conflict=error` stops the
session and `--conflict=skip` keeps the existing winner. A Skill that your own OMP
configuration hides (`skills.enabled: false`, a source toggle, `ignoredSkills`, `includeSkills`,
or `disabledExtensions`) fails the plan instead of mounting something OMP would silently ignore.

Arguments that would move or reshape that namespace after planning are rejected before anything is
created, and each rejection names the token and the safe new-session alternative:

- launching from your home directory without your own `--allow-home`;
- `--cwd`, `--profile`, `--alias`, `--config`, and the `OMP_PROFILE`, `PI_PROFILE`, and
  `PI_CONFIG_FILES` environment variables;
- Skill- and extension-set changes: `--no-skills`, `--skills`, `-e`/`--extension`, `--hook`,
  `--no-extensions`, `--plugin-dir`;
- session reuse: `-c`/`--continue`, `-r`/`--resume`, `--session`, `--fork`, `--from-claude`,
  `--from-codex`, `--export`;
- protocol-server modes (`--mode rpc`, `--mode rpc-ui`, `--mode acp`) and every OMP subcommand,
  such as `omp config` or `omp plugin`.

Everything else — prompt text, `--print`, `--mode text`, `--mode json`, model and thinking
selection, and approval flags you supplied yourself — passes through unchanged. SkillMount injects
no argument and no environment variable into an OMP launch, does not manage OMP authentication,
and never weakens OMP's permission or approval model: it forwards `--auto-approve`, `--yolo`, and
`--approval-mode` only when you supplied them.

Cleanup uses the same journaled, crash-recoverable path as the other agents. Every Skill link
SkillMount created remains cleanup-critical once the OMP process domain is dead. The `.omp` and
`.omp/skills` directories it had to create are pruned too — but only while they are still empty and
directories it recorded. If OMP, the operating system, or another program left something in one,
SkillMount preserves that directory and its contents untouched, reports it, and still completes
when no created link remains. Automatic and session cleanup reconcile a created link only after an
identity-verified unlink; pathname absence cannot prove that the same object was not moved
elsewhere, so an unverified link and its created enclosing helpers remain journal-backed. After
checking the reported paths and establishing that the process domain is dead, run `asm cleanup`.
That explicit command treats an all-absent set of recorded link paths as the operator's decision to
release the stale ownership record, reports that no filesystem entry was removed, and completes the
journal. It still leaves every existing mismatch untouched. This repairs a crash after unlink
without adding a second recovery option; if the link was instead moved elsewhere, the command
deliberately stops tracking that moved link. `asm doctor` helps inspect the interrupted session.
Attaching to or hot-reloading Skills into an OMP process SkillMount did not launch is not supported
— OMP loads Skills once at
startup, so start a new session instead. The recorded OMP evidence, including the platforms it
does and does not cover, lives in [docs/compatibility.md](docs/compatibility.md).

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
  `omp/17.2.9`. These are evidence baselines, not an exact-version allowlist. Normal sessions do
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
