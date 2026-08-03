# ADR 0023: Pin the Codex Session Discovery Contract

- **Status:** Accepted
- **Date:** 2026-08-03
- **Supersedes:** _none_

## Context

Fix-first review disproved ADR 0021's assumption that `current_dir` and rejected forwarded `-C`
were enough to keep one Codex process inside the inspected repository interval. Codex 0.146.0's
fresh TUI rebuilds configuration during `/resume`, `/fork`, `/new`, and side-conversation lifecycle
transitions. The rebuild retains CLI overrides but rereads legacy managed files and macOS managed
preferences. Those layers outrank session flags, so a value appearing after spawn can replace
`project_root_markers` and move the TUI into a discovery interval SkillMount never inspected.
Persistent user `skills.config` rules can also disable a selected Skill after its link is mounted.
Remote and service subcommands can move discovery to another process or host.

The pinned sources provide a bounded alternative. Root `review`, `exec review`, and plain `exec`
all enter `codex_exec::run_main`, build configuration at startup, and do not expose the TUI's
runtime configuration-reload path. A CLI CWD override fixes their launch CWD. Session flags outrank
normal system, cloud-managed, user, profile, and project layers, and a final name-enable rule
removes both earlier name and path disables for the loaded Skill. Legacy managed files and the
macOS `com.openai.codex:config_toml_base64` preference still outrank session flags, so their
presence cannot be silently approximated. Version and configuration pathnames can also change
while a session waits for locks or applies mounts.

## Decision

Every supported Codex launch SHALL inject separate native `-C <canonical-launch-cwd>`,
`-c project_root_markers=[".git"]`, and one `-c skills.config=[...]` argument containing an enabled
name rule for every selected, non-skipped Skill. Codex adapter-required metadata name and
description validation SHALL remain active even when optional validation is `none`. A selected
source SHALL also pass ADR 0021's exact plugin-manifest ancestry gate when planning and immediately
before spawn, so every injected base-name rule addresses the name Codex will actually load. A
candidate manifest that cannot be completely reopened as a regular file within the 64 KiB local
bound SHALL fail closed rather than borrowing malformed-manifest precedence from uncertain bytes.

Forwarded discovery-changing configuration, CWD, remote, interactive TUI, resume/fork, service, and
operator modes SHALL fail before SkillMount state access. Only bounded `exec`, `exec review`, and
root `review` launches remain supported, with command positions parsed separately from prompt and
option values. Bare variadic `-i`/`--image` SHALL be rejected because a later option can terminate
its values and expose a nested command; attached `-iVALUE`/`--image=VALUE` remains supported.
`inspect` remains a command-free inventory operation and does not certify or launch a Codex
session. A legacy managed file or macOS managed configuration preference SHALL fail closed before
state access and be rechecked after lock stabilization and immediately before spawn. The exact
Codex version SHALL be checked at the same three boundaries.

Only `src/agent/codex/macos_ffi.rs` and `src/paths/windows_ffi.rs` MAY opt in to unsafe code for this
decision. The macOS boundary SHALL synchronize the application domain before using the same Core
Foundation application-value lookup as Codex, fail closed when synchronization fails, expose only
a safe boolean/error result, release every Create/Copy object, and keep all raw types inside the
module. The Windows boundary SHALL resolve only `FOLDERID_Profile` and `FOLDERID_ProgramData`, copy
the returned UTF-16 path before releasing its COM task allocation, and expose only safe `PathBuf`
values. This extends the audited unsafe allowlist from four modules to six.

## Alternatives

- Support the interactive TUI by injecting only the pinned CWD and session flags. Rejected because
  runtime rebuilds reread higher-precedence managed layers; the CWD override does not bind the
  project-root marker model.
- Watch managed files and preferences while the TUI runs. Rejected because observing a change and
  interrupting the child cannot prevent a concurrent configuration rebuild from using it first,
  and the macOS preference is not a lockable filesystem object.
- Trust `Command::current_dir`. Rejected because it does not set the TUI's resume CWD override.
- Inspect only the current user config. Rejected because configuration reload and higher layers can
  change the result after inspection.
- Query `config/read` through app-server. Rejected because starting app-server initializes Codex
  state before SkillMount's read-only boundary.
- Use the `defaults` utility for macOS MDM. Rejected because its process output is not the same API
  contract as Codex's Core Foundation lookup.
- Depend on `dirs`/`dirs-sys` for Windows Known Folders. Rejected because their `option-ext`
  dependency is MPL-2.0, which the repository license policy does not allow; the narrow FFI keeps
  the already pinned `windows-sys` dependency and exact Codex API contract.
- Force bundled Skills enabled. Rejected because that can recreate a cache the operator explicitly
  disabled. Every system-cache collision instead blocks both conflict policies as a documented
  safe false positive.

## Consequences

- Forwarded arguments are preserved byte-for-byte after the injected prefix, but they are no
  longer the complete child argv. Verbose read-only output renders both layers separately.
- Selected Skills override user name/path disables for this child only. No persistent file or
  permission profile is edited.
- A selected source beneath a valid Codex, Claude, or Cursor plugin manifest is rejected. Existing
  plugin-qualified Skills may still produce conservative base-name conflicts until the adapter
  models qualified display names end to end.
- Any legacy managed configuration is conservatively unsupported, even when it does not contain a
  custom marker. Later support requires bounded parsing plus stable evidence for the relevant key.
- Interactive TUI sessions are unavailable until a design can prevent runtime managed-layer
  changes from escaping the inspected interval. This removes the unbounded post-spawn reload
  window instead of presenting repeated probes as lifetime evidence.
- For bounded `exec` and `review`, three probes materially narrow executable/configuration update
  races but cannot atomically bind a pathname or external preference to `Command::spawn`.
  Replacing that residual pre-spawn window requires a platform object-capability launch design and
  another ADR.
- The audited macOS FFI surface grows by four Core Foundation calls and carries a native framework
  link obligation already supplied by macOS. Synchronization makes repeated probes refresh
  externally changed preferences instead of trusting the process cache.
- The Windows path boundary adds three raw calls behind two safe Known Folder resolvers and avoids
  relaxing the dependency-license policy.

## Verification

- `src/agent/tests.rs::codex_launch_pins_the_inspected_cwd_and_discovery_configuration` proves the
  exact injected argument vector.
- `tests/read_only.rs` covers discovery-changing flags, remote and non-session modes, command-value
  disambiguation, bare/attached image boundaries, interactive-TUI rejection, command-free
  `inspect`, pre-state managed-layer rejection, all three selected-source plugin-manifest
  spellings, malformed-manifest precedence, and the bounded manifest read.
- `tests/codex_session.rs` records three version probes, rejects a third-probe release change,
  injects a plugin manifest at the third probe, proves the child sees the pinned arguments, and
  verifies required cleanup before either rejected spawn even when `--keep-mounts` was requested.
- `src/catalog/tests.rs::validation_levels_follow_adapter_metadata_rules` keeps the Codex logical
  name available to the injected enable rule under `validation=none`.
- Native macOS and Windows compilation plus raw-diff review verify the fifth and sixth unsafe
  boundaries; every unsafe block carries a local `SAFETY` explanation and no Core Foundation or
  Windows raw type crosses either module.
- The official `rust-v0.146.0` source at commit
  `e363b08c9175ac1cbe5893615dd2cb9ddf95043b` provides the behavior boundary:
  `tui/src/app/config_persistence.rs` rebuilds TUI configuration and
  `tui/src/app/session_lifecycle.rs` invokes it, while root `review` and `exec` route through
  `cli/src/main.rs` to the one-shot builder in `exec/src/lib.rs`; `core-skills/src/loader/namespace.rs`
  and `utils/plugins/src/plugin_namespace.rs` define the selected source's canonical plugin-name
  lookup.
