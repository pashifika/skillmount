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

The 2026-08-08 access-aware concurrency coverage launches real SkillMount processes and exercises
native shared/exclusive operating-system locks, physical aliases, recovery, supervision, and
ownership-checked cleanup with the repository's fake-Agent child. That is locking and lifecycle
evidence, not a live Codex, Claude Code, or OMP discovery or link-loading run, so it promotes no row
in the matrix below.

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
| OMP | 17.2.9 | Windows / i686 | explicit junction | Rightmost overlay and non-shadowed base Skills are discoverable through a requested junction | 2026-08-06 | unverified | At SkillMount revision `5807efdc4ae0e843f4c4c79a24ba46ed088a6f06`, the historical harness omitted OMP for the i686 wrapper. No 32-bit OMP binary exists, but an i686 wrapper on an x64 host can launch the x64 agent; this row remains unverified |

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
published digest file. The historical harness omitted the x86-wrapper OMP case; that is not proof
that an x86 wrapper on an x64 host cannot launch the x64 agent. Every other native OMP cell — macOS Apple Silicon and Windows x64 junction loading
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
These historical OMP junction rows predate the v0.3.0 native-directory mechanism and do not
certify it. OMP has no 32-bit Windows binary; both wrapper architectures can nevertheless launch
the x64 OMP binary on the x64 CI host. This is not support for a 32-bit Windows operating system.
Operators who prefer fail-closed capability behavior can pass `--link-mode=symlink`; SkillMount
does not request elevation or change Windows privilege policy.

## Evidence policy

A maintainer updating this matrix records the exact Agent version, OS and architecture, link kind,
scenario, date, result, revision, and a retained log or CI run. `pass` and `fail` are reserved for a
completed scenario. `observed` records a narrower fact such as a version probe and never promotes
another Agent/platform/link scenario. `unverified` means the scenario did not run or its evidence
is incomplete.

## 2026-10-30 current-release smoke preparation

The native smoke is pinned to Codex CLI 0.166.0, Claude Code 2.1.314, and opt-in OMP 18.14.0, the latest stable upstream releases observed on this date:

- https://github.com/openai/codex/releases/tag/rust-v0.166.0
- https://github.com/anthropics/claude-code/releases/tag/v2.1.314
- https://github.com/can1357/oh-my-pi/releases/tag/v18.14.0

Exact npm platform package metadata and all four downloaded archives were checked against the committed SHA-512 SRI values. The allowlisted headless runtime members were then extracted successfully on the preparation host: Codex macOS 4 members, Codex Windows 6 members, and Claude one binary per platform. Codex 0.166.0 no longer includes the former macOS bundled-zsh member; its optional voice assets are outside this headless smoke. This is package integrity/layout evidence, not execution on macOS or Windows.

Sources:
- https://registry.npmjs.org/@openai%2Fcodex/0.166.0-darwin-arm64
- https://registry.npmjs.org/@openai%2Fcodex/0.166.0-win32-x64
- https://registry.npmjs.org/@anthropic-ai%2Fclaude-code-darwin-arm64/2.1.314
- https://registry.npmjs.org/@anthropic-ai%2Fclaude-code-win32-x64/2.1.314

OMP's macOS arm64 and Windows x64 SHA-256 values were checked in both the official release API asset metadata and the published SHA256SUMS.txt. Downloads now fail closed against those committed values before execution; the harness also checks the selected OMP binary against the matching target digest and rechecks its hash before any credential-bearing launch. Unlike 17.2.9, this release has a published checksum file. It still has no Windows x86 agent binary. Both Windows wrapper rows now use the checksum-verified x64 OMP child on an x64 runner, as they already do for Codex/Claude; this does not claim a 32-bit OMP build or 32-bit Windows OS support.

https://github.com/can1357/oh-my-pi/releases/download/v18.14.0/SHA256SUMS.txt

The new explicit probes-only workflow option runs credential-free version/help and SkillMount doctor probes under isolated homes/configuration roots. Success is recorded as `observed`, with every Skill-discovery result explicitly `unverified`; it does not certify an authenticated session, skill loading, cleanup or link behavior. The workflow does not supply provider credentials in this mode. Normal authenticated mode remains separate and missing/rejected authentication remains unverified.

The OMP authenticated scenario follows native first-custom-root bare-name semantics instead of imposing SkillMount's Codex/Claude rightmost overlay oracle. It passes source roots in their original order, expects the base plus first-root bare-name result, does not pass a link mode to the OMP adapter, and records `skills.customDirectories`/no link. Additional OMP aliases are not treated as a failure merely because they exist. Codex/Claude retain their existing rightmost overlay scenario.

Native execution and authenticated discovery for these new pins are still unverified until actual runs are recorded. Do not promote compatibility matrix rows from these preparation checks or deterministic harness tests alone.

Pull requests into main or dev/** now run the same native jobs in forced credential-free mode, including OMP on macOS arm64 and both Windows wrapper architectures (x64 child on an x64 host). Provider-secret expressions require a manual workflow_dispatch and probes_only=false; authenticated discovery remains manual-only. Summary schema is now 2 because the former global link-kind and winner-3 fields became agent-specific mechanism/expected-response evidence. The supply-chain manifest remains schema 1.

Credential-free PR runs also execute a separate `omp skill list --json` native-loader probe using a temporary PI_CONFIG_FILES overlay (the subcommand does not forward root --config). It checks first-root bare-name identity, retained duplicate source aliases, a non-shadowed base, and tilde/relative directories containing spaces. It creates no project .omp/skills destination. A pass records raw OMP discovery only; authenticated SkillMount wrapper-session integration remains explicitly unverified. The same harness probe genuinely passed on Linux x86_64 using official OMP 18.14.0, SHA-256 e7dd2b606c4ebe9d1a7bcfad1b8820db968cd6b05409fed1c6bc8db0b6a95a8f, on 2026-10-30. Its private fixture was removed after the run. Native macOS/Windows results still require their actual CI runs.

### Current adapter contract review

Codex rust-v0.166.0 source and current Claude CLI/Skills docs were reviewed on 2026-10-30.
Regressions cover exec fork/worktree rejection, selected Codex product metadata restrictions,
Claude operator/restricted controls, reserved names and linked-worktree fallback. Previous exact
banner baselines for Codex/Claude remain historical evidence; source review is not authenticated
latest-runtime certification. OMP's older link-based rows above are historical and do not certify
the v0.3.0 native-directory mechanism (ADR 0040).

The OMP diagnostic binary is always provisioned for the all-agent doctor probe. Manual
`include_omp` selects the authenticated OMP scenario only; leaving it false no longer removes
OMP from mandatory diagnostics. `--omp-probe-bin` identifies this diagnostics-only role.

### 2026-10-30 native credential-free observations

[Live agent smoke run 41050170503](https://github.com/pashifika/skillmount/actions/runs/41050170503),
attempt 1, exercised PR head `1f5c75b3cb99b77430203736d94f80659d83f4fc` through tested merge
`8167a28a90406bad3e73135d0132065b62e6d7a2`. All three downloaded evidence ZIPs matched their
GitHub API SHA-256 digests. Their summaries and doctor logs record:

| Native host / wrapper | Observed versions | Help / doctor | Raw OMP loader |
|---|---|---|---|
| macOS ARM64 / ARM64 | Codex 0.166.0, Claude 2.1.314, OMP 18.14.0 | All help exits 0; doctor 0 (12 pass, 1 warning, 0 failure, 6 unverified) | Pass: six Skills, no links |
| Windows x64 / x64 | Codex 0.166.0, Claude 2.1.314, OMP 18.14.0 | All help exits 0; doctor 0 (13 pass, 1 warning, 0 failure, 5 unverified) | Pass: six Skills, no links |
| Windows x64 / i686 wrapper, x64 agents | Codex 0.166.0, Claude 2.1.314, OMP 18.14.0 | All help exits 0; doctor 0 (13 pass, 1 warning, 0 failure, 5 unverified) | Pass: six Skills, no links |

Evidence artifact IDs are respectively `12974621328`, `12974132697`, and `12974486960`.
These rows establish **observed** executable/doctor/raw-loader behavior only. Every authenticated
scenario and `wrapper_session_integration` remains explicitly **unverified**. They do not certify
full product CI, authenticated Skill loading, session lifetime, or cleanup. The separate product
suite at this revision still exposed a stale transaction diagnostic assertion; its correction and
fresh exact-head release gates are required independently of this probe evidence.
