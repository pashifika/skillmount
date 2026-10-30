# ADR 0040: Delegate OMP discovery through an owned session configuration

- Status: Accepted
- Date: 2026-10-30
- Supersedes: ADR 0034/0035 OMP namespace emulation, placement and selected-Skill policy only

## Context

OMP 18.14.0 natively loads `skills.customDirectories`. Recreating its provider inventory and
linking a SkillMount rightmost winner into `.omp/skills` imposes a different policy and duplicates
upstream discovery logic. The operator explicitly chose native OMP selection/duplicate semantics.
Codex and Claude continue to use the existing selected-catalog/link contract.

## Decision

The OMP adapter delegates Skill parsing, filtering, names, aliases and duplicate decisions to OMP.
It validates collection-root accessibility, not Skill metadata. An individual Skill root is rejected:
native custom directories scan only immediate `<root>/<child>/SKILL.md`, so substituting the parent
would expose unrequested siblings. CLI roots are canonical absolute Unicode paths. Existing effective
configured roots retain their literal values and appear first; CLI roots append in input order. First
custom occurrence receives the bare name; differing duplicates can remain namespaced. No rightmost
winner or conflict-skip guarantee is made for OMP.

The adapter reads only enough declarative settings to preserve the effective `customDirectories`
array: global `config.yml` or fallback `config.yaml`; current-CWD native `.omp/settings.json` then
`.omp/config.yml` (native skipped at home), Claude, Codex TOML, Gemini, all four OpenCode project
files in root-json/root-jsonc/nested-json/nested-jsonc order, then Cursor. Later arrays replace earlier
arrays. Project non-object `skills` shadows are ignored. No ancestor settings walk or Skill inventory
is performed. User environment/CLI config overlays and profile relocation remain unsupported. Absolute `PI_CONFIG_DIR` is rejected
because Node and Rust join it to the home directory differently; relative values remain supported.
Unmigrated legacy `settings.json` or `agent.db`, invalid/oversized settings, non-string directory arrays and
unsupported OpenCode JSONC/substitutions fail closed. This is deliberately stricter than upstream's
warnings/skipping; run OMP directly when a configuration cannot be projected. Inputs are capped at
1 MiB each and the combined array at 1024 entries. No OMP config command is executed or migration
performed.

A JSON-serialized YAML-subset document containing only `skills.customDirectories` is planned under
private session state and supplied with `--config <file>` before unchanged validated passthrough.
Arrays are snapshotted at launch: subsequent base custom-directory edits need a new wrapped session.
Other OMP settings remain OMP-managed. `--skills` and `--no-skills` are native filter controls, not
path overrides. Root/profile relocation, extension-changing CLI controls, lifecycle reuse/restore,
operator commands and server modes retain their conservative gates. OMP uses staging only;
non-default link mode, conflict skip, metadata validation and keep-mounts are rejected clearly.

Shared orchestration owns mutations and supervision. `CreateConfigFile` is a cleanup-critical
journal action (`config`, kind `file`), with exact bounded contents, identity, and a create-new
no-overwrite file operation. The enclosing owned directory is scaffolding. The file remains until
the full managed process domain is proven dead, because OMP can reload configuration and rediscover
Skills. Pre-spawn checks repeat effective inputs and exact owned-file identity/content verification.
Cleanup removes only the recorded unchanged regular file; replacements, symlinks and modified content
remain untouched with recovery evidence. Native file handles/no-follow primitives enforce this in
the backend; the Unix final pathname race remains the explicitly accepted ADR 0014 boundary,
while Windows retains its verified handle through removal.

No journal schema number changes: current binaries still read schema 1/2 legacy link journals.
Older binaries fail closed on the new operation/kind, so active sessions must be finished or cleaned
by a supporting binary before downgrade. Existing `.omp/skills`, project settings and Skill files
are never owned by the new OMP session.

## Consequences and validation

OMP differs visibly from Codex/Claude; inspect and dry-run report delegated selection, config actions
and ordered sources, not fabricated chosen Skills. Independent sessions have independent private
files and do not serialize on a nonexistent project mount destination. Settings/source observations
remain shared locks; OMP decides runtime source behavior. No permission, authentication or approval
setting is injected.

Focused tests cover config projection, read-only behavior, native filter forwarding, no project
links/config edits, journal recovery, content/identity mismatch and lifetime cleanup. Official OMP
18.14.0 credential-free native-loader probes separately verify duplicate aliases, root order,
tilde/relative roots and array replacement. Such probes do not claim authenticated wrapper-session
certification. Native target CI and the release checklist remain mandatory.

Sources: [OMP skills](https://github.com/can1357/oh-my-pi/blob/v18.14.0/packages/coding-agent/src/extensibility/skills.ts),
[settings](https://github.com/can1357/oh-my-pi/blob/v18.14.0/packages/coding-agent/src/config/settings.ts),
[CLI](https://github.com/can1357/oh-my-pi/blob/v18.14.0/packages/coding-agent/src/cli/flag-tables.ts).
