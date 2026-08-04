# Compatibility evidence

SkillMount pins each launch adapter to a tested agent release. This matrix records observations,
not evergreen compatibility claims. A row is `observed` only when the named command or scenario
ran on the stated platform and date. Documentation review without a live agent run remains
`unverified` for runtime discovery.

The deterministic fake-agent and native filesystem suites are the release gate. Authenticated
real-agent checks are additional compatibility evidence and are never inferred from a missing or
green CI job.

## Current matrix

| Agent | Version | OS / architecture | Link kind | Scenario | Date | Outcome | Evidence |
|---|---|---|---|---|---|---|---|
| Codex CLI | 0.146.0 | macOS / Apple Silicon | n/a | `codex --version` reports the adapter-pinned release | 2026-08-03 | observed | Local command output: `codex-cli 0.146.0` |
| Claude Code | 2.1.220 | macOS / Apple Silicon | n/a | `claude --version` reports the adapter-pinned release | 2026-08-03 | observed | Local command output: `2.1.220 (Claude Code)` |
| Codex CLI | 0.146.0 | macOS / Apple Silicon | directory symlink | Rightmost overlay and non-shadowed base Skills are discoverable in a real authenticated session | 2026-08-03 | unverified | Live smoke has not run |
| Claude Code | 2.1.220 | macOS / Apple Silicon | directory symlink | Rightmost overlay and non-shadowed base Skills are discoverable in a real authenticated session | 2026-08-03 | unverified | Live smoke has not run |
| Codex CLI | 0.146.0 | Windows / x86_64 | explicit junction | Rightmost overlay and non-shadowed base Skills are discoverable through a requested junction | 2026-08-03 | unverified | No native live-agent run recorded |
| Codex CLI | 0.146.0 | Windows / i686 | explicit junction | Rightmost overlay and non-shadowed base Skills are discoverable through a requested junction | 2026-08-03 | unverified | No native live-agent run recorded |
| Claude Code | 2.1.220 | Windows / x86_64 | explicit junction | Rightmost overlay and non-shadowed base Skills are discoverable through the injected staging root | 2026-08-03 | unverified | No native live-agent run recorded |
| Claude Code | 2.1.220 | Windows / i686 | explicit junction | Rightmost overlay and non-shadowed base Skills are discoverable through the injected staging root | 2026-08-03 | unverified | No native live-agent run recorded |

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

The documentation does not establish real Windows junction discovery for either pinned agent.
Those rows remain `unverified` until the manual smoke workflow records a native run.

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
ownership-verifiable junction, but emits a warning whenever the pinned agent/version lacks passing
live junction evidence in this matrix. The warning is a compatibility boundary, not a cleanup
warning: transaction-owned junctions still use the same journal, lock, and verified-removal path.
Operators who prefer fail-closed capability behavior can pass `--link-mode=symlink`; SkillMount
does not request elevation or change Windows privilege policy.

## Evidence policy

A maintainer updating this matrix records the exact agent version, OS and architecture, link kind,
scenario, date, result, and a retained log or CI run. `pass` and `fail` are reserved for a completed
scenario. `observed` records a narrower fact such as a version probe. `unverified` means the
scenario did not run or its evidence is incomplete.
