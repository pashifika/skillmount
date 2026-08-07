# Compatibility evidence

SkillMount records a dated last-tested banner for each launch adapter. The banner is compile-time
adapter metadata, never an exact-version launch allowlist, and a mutating session neither observes
it nor warns about it: `asm doctor` is the only surface that executes an Agent with `--version`
([ADR 0036](adr/0036-confine-agent-version-observation-to-doctor.md)). This matrix records
observations, not evergreen compatibility claims. A row is `observed` only when the named command
or scenario ran on the stated platform and date. A banner observation alone does not certify
mounted Skill discovery, lifecycle, link loading, or cleanup, and documentation review without a
live Agent run remains `unverified` for runtime discovery.

The deterministic fake-agent and native filesystem suites are the release gate. Authenticated
real-agent checks are additional compatibility evidence and are never inferred from a missing or
green CI job.

## Current matrix

| Agent | Version | OS / architecture | Link kind | Scenario | Date | Outcome | Evidence |
|---|---|---|---|---|---|---|---|
| Codex CLI | 0.146.0 | macOS / Apple Silicon | n/a | `codex --version` reports the adapter's last-tested banner | 2026-08-05 | observed | Local command output at base revision `977dfd21f0d597348e36b004b796cdbb2c3140bc`: `codex-cli 0.146.0` |
| Claude Code | 2.1.220 | macOS / Apple Silicon | n/a | `claude --version` reports the adapter's last-tested banner | 2026-08-03 | observed | Local command output: `2.1.220 (Claude Code)` |
| Claude Code | 2.1.222 | macOS / Apple Silicon | n/a | `claude --version` reports a banner newer than the last-tested evidence | 2026-08-05 | observed | Local command output at base revision `977dfd21f0d597348e36b004b796cdbb2c3140bc`: `2.1.222 (Claude Code)`; banner only |
| Codex CLI | 0.146.0 | macOS / Apple Silicon | directory symlink | Rightmost overlay and non-shadowed base Skills are discoverable in a real authenticated session | 2026-08-05 | unverified | Live smoke has not run |
| Claude Code | 2.1.220 | macOS / Apple Silicon | directory symlink | Rightmost overlay and non-shadowed base Skills are discoverable in a real authenticated session | 2026-08-03 | unverified | Live smoke has not run |
| Claude Code | 2.1.222 | macOS / Apple Silicon | directory symlink | Rightmost overlay and non-shadowed base Skills are discoverable in a real authenticated session | 2026-08-05 | unverified | Version/help/documentation review only; authenticated mounted-session smoke did not run |
| Codex CLI | 0.146.0 | Windows / x86_64 | explicit junction | Rightmost overlay and non-shadowed base Skills are discoverable through a requested junction | 2026-08-03 | unverified | No native live-agent run recorded |
| Codex CLI | 0.146.0 | Windows / i686 | explicit junction | Rightmost overlay and non-shadowed base Skills are discoverable through a requested junction | 2026-08-03 | unverified | No native live-agent run recorded |
| Claude Code | 2.1.220 | Windows / x86_64 | explicit junction | Rightmost overlay and non-shadowed base Skills are discoverable through the injected staging root | 2026-08-03 | unverified | No native live-agent run recorded |
| Claude Code | 2.1.220 | Windows / i686 | explicit junction | Rightmost overlay and non-shadowed base Skills are discoverable through the injected staging root | 2026-08-03 | unverified | No native live-agent run recorded |
| OMP | 17.2.9 | macOS / Apple Silicon | n/a | `omp --version` reports the adapter's last-tested banner | 2026-08-06 | observed | Local command output at SkillMount revision `5807efdc4ae0e843f4c4c79a24ba46ed088a6f06` from the Homebrew-installed binary: `omp/17.2.9`; contract read from source tag `v17.2.9`, commit `f7f8e040ee04710414fbd775431091fa301b9786` |
| OMP | 17.2.9 | macOS / Apple Silicon | directory symlink | Rightmost overlay and non-shadowed base Skills are discoverable in a real authenticated session | 2026-08-06 | unverified | Live smoke has not run at SkillMount revision `5807efdc4ae0e843f4c4c79a24ba46ed088a6f06` |
| OMP | 17.2.9 | Windows / x86_64 | explicit junction | Rightmost overlay and non-shadowed base Skills are discoverable through a requested junction | 2026-08-06 | unverified | No native live-agent run recorded at SkillMount revision `5807efdc4ae0e843f4c4c79a24ba46ed088a6f06`; the opt-in smoke has not run against the published `omp-windows-x64.exe` |
| OMP | 17.2.9 | Windows / i686 | explicit junction | Rightmost overlay and non-shadowed base Skills are discoverable through a requested junction | 2026-08-06 | unverified | At SkillMount revision `5807efdc4ae0e843f4c4c79a24ba46ed088a6f06`, OMP publishes no 32-bit Windows asset for 17.2.9, so no native run is possible for this version; a missing asset, not a failed scenario |

## Documentation review

The 2026-08-03 review used current official documentation and kept runtime claims separate from
documented behavior:

- Codex documents repository, user, administrator, and bundled Skill scopes, repository ancestor
  discovery, and symlinked Skill folders in [Build skills](https://learn.chatgpt.com/docs/build-skills.md).
- Codex documents `codex --version` and keeps Skill discovery separate from `--add-dir` workspace
  access in the [CLI command reference](https://learn.chatgpt.com/docs/developer-commands.md?surface=cli).
- Claude Code documents project-ancestor, user, and `--add-dir` Skill discovery plus live change
  detection in [Extend Claude with skills](https://code.claude.com/docs/en/skills).
- Claude Code documents `CLAUDE_CONFIG_DIR` in
  [Environment variables](https://code.claude.com/docs/en/env-vars) and managed settings as the
  highest-precedence tier in [Settings](https://code.claude.com/docs/en/settings).

The 2026-08-05 implementation review observed `codex-cli 0.146.0` and
`2.1.222 (Claude Code)` on macOS / Apple Silicon at base revision
`977dfd21f0d597348e36b004b796cdbb2c3140bc`:

- Current Codex [Skills](https://developers.openai.com/codex/skills) and
  [configuration](https://developers.openai.com/codex/config-basic) documentation retained the
  reviewed repository/user/system discovery and higher-precedence configuration model. The
  installed banner matched the last-tested evidence; published `0.147.0-alpha` material was treated
  as prerelease source evidence, not compatibility.
- Current Claude [Skills](https://code.claude.com/docs/en/skills),
  [settings](https://code.claude.com/docs/en/settings), and
  [changelog](https://github.com/anthropics/claude-code/blob/main/CHANGELOG.md) material retained the
  reviewed discovery and managed-policy model but documented lifecycle changes after `2.1.220`.
  Local `claude --help` additionally exposed the non-session `import` command and value-taking
  `--autocompact` option. Those two observed parser shapes are enforced, while `2.1.222` mounted
  Skill discovery remains `unverified`.

No authenticated mounted-session smoke ran during that review. Deterministic fake-agent tests are
the release gate and no live compatibility row was promoted.

The 2026-08-06 OMP review read the tagged source rather than documentation: `oh-my-pi` tag
`v17.2.9`, commit `f7f8e040ee04710414fbd775431091fa301b9786`, with the installed Homebrew binary
reporting `omp/17.2.9`.
[ADR 0034](adr/0034-pin-the-omp-session-discovery-contract.md) pins the recorded discovery and
launch contract. The GitHub release for the tag publishes `omp-darwin-arm64`, `omp-darwin-x64`,
`omp-linux-arm64`, `omp-linux-x64`, `omp-linux-musl-arm64`, `omp-linux-musl-x64`, and
`omp-windows-x64.exe`; there is no 32-bit Windows asset, and 17.2.9 publishes no `SHA256SUMS.txt`
— that file first appears at 17.2.10 — so this exact release cannot be integrity-locked from a
published digest file. Windows x86 OMP evidence is therefore permanently unavailable for this
version, while every other native OMP cell — macOS Apple Silicon and Windows x64 junction loading
included — is `unverified` only because the opt-in native smoke has not run, not because the
platform is unsupported. No authenticated OMP session ran during this review and no live OMP row
was promoted. The deterministic fake-agent read-only, session, and transaction suites at SkillMount
revision `5807efdc4ae0e843f4c4c79a24ba46ed088a6f06` reproduce the recorded 17.2.9 contract over real
directory symlinks and remain the release gate; like the Codex and Claude suites they are a gate
rather than a matrix row, because they exercise no OMP process.

The documentation does not establish real Windows junction discovery for any listed
Agent/platform combination. Those rows remain `unverified` until the manual smoke workflow records
a native run.

The manual workflow fetches exact native npm platform packages with lifecycle scripts disabled,
checks each archive against a committed SHA-512 SRI value, extracts only allowlisted regular-file
runtime members, and binds the primary binary's SHA-256 digest into the evidence. On Windows it
passes the native x64 `codex.exe` and `claude.exe` directly; npm `.cmd` shims are rejected because
they would require implicit `cmd.exe` execution. The i686 row runs the 32-bit SkillMount wrapper
against those native agents on the x64 Windows runner; it does not claim a 32-bit agent build.

The harness removes credentials from version and doctor probes, gives Codex and Claude only their
respective provider credential, and maps the repository's `OPENAI_API_KEY` secret to the
non-interactive Codex `CODEX_API_KEY` child variable. It redacts every retained stream before
writing it and scans the artifact for inherited secret values. It disables agent auto-updates and
rechecks each native binary hash immediately before its credential-bearing launch. A timeout
terminates the wrapper process tree through a Unix session boundary or a Windows kill-on-close Job
Object, including when the wrapper parent has already exited. Missing or rejected authentication
is `unverified`, not evidence that Skill discovery failed. The summary records the workflow run,
repository revision, runner, binary hashes, exact versions, requested link kind, machine-output
validity, winner/base observations, displaced-token absence, and journal residue count.

## Automatic junction policy

On Windows, `--link-mode=auto` may fall back from a directory symlink to a junction when the host
denies symlink creation. SkillMount continues only after the native backend creates and records an
ownership-verifiable junction, but emits a warning whenever this Agent/platform/link combination
lacks passing live junction evidence in the matrix. The adapter's last-tested banner is diagnostic
context, not proof that the junction was exercised. The warning is a compatibility boundary, not a
cleanup warning: transaction-owned junctions still use the same journal, lock, and verified-removal
path.
For OMP, 17.2.9 publishes a 64-bit Windows binary but no 32-bit one: the x64 junction combination
stays `unverified` until the opt-in native smoke records it, and no 17.2.9 junction evidence can
ever exist on Windows x86.
Operators who prefer fail-closed capability behavior can pass `--link-mode=symlink`; SkillMount
does not request elevation or change Windows privilege policy.

## Evidence policy

A maintainer updating this matrix records the exact Agent version, OS and architecture, link kind,
scenario, date, result, revision, and a retained log or CI run. `pass` and `fail` are reserved for a
completed scenario. `observed` records a narrower fact such as a version probe and never promotes
another Agent/platform/link scenario. `unverified` means the scenario did not run or its evidence
is incomplete.
