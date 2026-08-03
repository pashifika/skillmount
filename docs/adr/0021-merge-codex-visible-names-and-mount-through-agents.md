# ADR 0021: Merge Codex Visible Names and Mount Through `.agents`

- **Status:** Accepted
- **Date:** 2026-08-03
- **Supersedes:** ADR 0020

## Context

ADR 0020 corrected Codex discovery from direct directory names to recursive frontmatter names, but
retained a two-entry backing-store state table. A missing `.agents/skills` could still select
`.codex/skills` and create a discovery link between them. Conflict evaluation then repeatedly
searched scope-shaped collections even though its real question was simpler: which Skills will the
child see under one logical name, and is the one physical destination path free?

Review of the implementation found three concrete failures in that model. Two sessions reaching
one nested collection through different directory links did not share a lock because only discovery
roots contributed physical identities. A late destination conflict introduced after preliminary
discovery needed a deterministic proof that the locked rebuild, rather than the preliminary
snapshot, decided the outcome. Finally, the state table made the legacy `.codex/skills` root look
like a placement choice even though current Codex already discovers `.agents/skills` directly.

The model was rechecked against the pinned Codex 0.146.0 sources:

- [`loader.rs`](https://raw.githubusercontent.com/openai/codex/rust-v0.146.0/codex-rs/core-skills/src/loader.rs)
  enumerates repository, user, deprecated user, bundled-system, and administrator roots;
- [`discovery.rs`](https://raw.githubusercontent.com/openai/codex/rust-v0.146.0/codex-rs/core-skills/src/loader/discovery.rs)
  uses recursive frontmatter discovery with depth 6, 2,000 directories, 20,000 entries, hidden
  directory pruning, and scope-dependent directory-link traversal;
- [`file-system/src/lib.rs`](https://raw.githubusercontent.com/openai/codex/rust-v0.146.0/codex-rs/file-system/src/lib.rs)
  caps the serialized walk response at 4 MiB and omits file links and other non-regular entries
  before Skill metadata is loaded; and
- [`root_loader.rs`](https://raw.githubusercontent.com/openai/codex/rust-v0.146.0/codex-rs/core-skills/src/root_loader.rs)
  removes duplicate files by `SKILL.md` path but retains different files that declare one name;
- [`namespace.rs`](https://raw.githubusercontent.com/openai/codex/rust-v0.146.0/codex-rs/core-skills/src/loader/namespace.rs)
  makes a canonical followed `SKILL.md` parent a namespace lookup root; and
- [`plugin_namespace.rs`](https://raw.githubusercontent.com/openai/codex/rust-v0.146.0/codex-rs/utils/plugins/src/plugin_namespace.rs)
  defines the three manifest spellings, their precedence, and the JSON `name` fallback.

Review of the first implementation exposed four more places where a static directory snapshot was
not equivalent to the child contract:

- Codex installs or replaces six embedded system Skills before its service loads roots, so an
  empty or stale `.system` cache is not evidence that those names will remain absent;
- the loader repairs a narrow class of unquoted YAML scalars, collapses whitespace, and falls back
  from an absent or blank `name` to the containing directory, while the shared catalog parser is
  intentionally stricter;
- Codex accepts only an existing directory from a non-empty Unicode `CODEX_HOME`, canonicalizes it
  relative to its process CWD, and ignores a non-Unicode value; and
- roots sharing one terminal can still produce different inventories when one is bundled-system
  discovery and another follows nested directory links.

The pinned [`skills/src/lib.rs`](https://raw.githubusercontent.com/openai/codex/rust-v0.146.0/codex-rs/skills/src/lib.rs)
and bundled assets establish the six installed names. The pinned
[`home-dir/src/lib.rs`](https://raw.githubusercontent.com/openai/codex/rust-v0.146.0/codex-rs/utils/home-dir/src/lib.rs)
and loader establish the remaining behavior. A local `codex-cli 0.146.0 --version` probe
establishes the executable label used to bind those facts to a mutating launch.

The source evidence also overturned one part of ADR 0020: Codex canonicalizes and traverses a linked
bundled-system root, but skips directory links encountered beneath that root. Repository, user, and
administrator discovery follows both root and nested directory links.

A later review of `DirectFileSystem` and `PathUri` disproved the first implementation's assumption
that budgeting Codex's opaque Windows URI fallback was sufficient. Codex joins every returned
directory-entry name onto the canonical root URI. An opaque URI rejects that join, while
`DirectFileSystem::read_directory` converts native entry names through `to_string_lossy` before
joining them. Native traversal can therefore succeed where Codex's walk errors or addresses a
different path. The same review found that an unbounded plugin-manifest read could allocate from an
attacker-controlled file or block after a regular-file-to-FIFO replacement on Unix.

## Decision

`DiscoverySnapshot` SHALL expose one deterministic visible-name index,
`BTreeMap<SkillNameKey, Vec<VisibleSkill>>`, retaining every declaration and its scope. It SHALL
separately expose immediate occupancy of the chosen destination namespace. Conflict policy SHALL
consume those two indexes instead of deriving logical visibility from destination paths or stopping
at the first matching scope.

The Codex adapter SHALL place every new mount through `<project-root>/.agents/skills/<name>`. A
missing `.agents/skills` SHALL become a regular directory, creating `.agents` first when needed. An
existing regular directory or directory link at `.agents/skills` SHALL be respected. The adapter
SHALL never choose `.codex/skills` as a new backing store or create a compatibility discovery link;
`.codex/skills` remains a visible legacy conflict scope only.

The adapter SHALL inspect project and ancestor `.agents/skills` and `.codex/skills`,
`$HOME/.agents/skills`, deprecated `$CODEX_HOME/skills`, bundled
`$CODEX_HOME/skills/.system`, and the platform administrator root. It SHALL retain frontmatter names
separately from direct path occupancy, use the pinned traversal bounds, skip hidden directories,
follow directory links except for links encountered beneath the bundled-system root, account conservatively for the 4 MiB
serialized response cap, and fail closed when its bounded inventory cannot be completed. A
`SKILL.md` directory entry SHALL be a regular file; file links and other special entries are not
visible Codex Skills, and a selected Codex source using one SHALL fail catalog validation before
mutation. Every traversed canonical directory SHALL contribute a physical lock identity in addition
to each scope root's logical lock.

Existing Codex metadata SHALL use the pinned loader's envelope, scalar-repair, single-line
sanitization, missing-name fallback, description requirement, and name-length rules. A malformed
file the child also rejects may remain a warning; a local size or read bound that prevents a
complete determination SHALL fail planning. Scopes sharing a canonical terminal SHALL be folded
only when their traversal policies are equivalent. In particular, bundled-system and
administrator scopes SHALL remain distinct even when their root terminals match.

A mutating launch SHALL run a shell-free `--version` probe and accept exactly
`codex-cli 0.146.0` before SkillMount state inspection, locking, or mounting. The adapter SHALL
reserve `imagegen`, `openai-docs`, `plugin-creator`, `review-agent`, `skill-creator`, and
`skill-installer` in the bundled-system scope whether or not the current cache contains them,
because that release may install or replace the cache before discovery. Read-only commands model
this pinned manifest without launching the executable. Neither an observed cache entry nor a
synthetic manifest entry SHALL authorize exact-source reuse or `--conflict=skip`; Codex may delete
or disable that cache before Skill loading, so either policy fails closed on a system collision.
The Windows administrator root SHALL be `%ProgramData%\OpenAI\Codex\skills`, using
`FOLDERID_ProgramData` and Codex's `C:\ProgramData` fallback.

Every native directory-entry name returned during discovery SHALL be Unicode. A non-Unicode entry
SHALL fail the whole inventory because Codex's lossy conversion does not preserve the path it then
joins and inspects. On Windows, each existing canonical discovery root and the canonical anchor of
the preferred root SkillMount plans to create SHALL have an ordinary file-URI representation
accepted by the pinned `PathUri` conversion. An opaque
`file:///%00/bad/path/<base64>` root SHALL fail closed because Codex cannot join a child path onto
it. A UNC `localhost` authority SHALL also fail because the pinned URL round trip removes that
authority, as SHALL a conservative set of WHATWG numeric-host spellings that normalize to a
different server path. Bracketed IPv6 SHALL be rejected rather than reimplementing and trusting URL
address canonicalization. The ordinary serialized file-URI response SHALL remain within the 4 MiB
bound.

`CODEX_HOME` resolution SHALL mirror the supported release: use only a non-empty Unicode value,
require its existing directory, canonicalize it relative to the wrapper invocation CWD, and pass
that canonical Unicode path explicitly to the child. An absent, empty, or non-Unicode value SHALL
use the user-home default without replacement so the child applies the same ignore rule. Any
successfully followed `.git` marker SHALL anchor the default project root, including a linked
marker.

Forwarded root and configuration overrides SHALL be rejected before SkillMount state inspection.
The adapter SHALL pin the child CWD and default marker model through native session arguments and
reject a higher-precedence legacy managed layer that could replace them. ADR 0023 records the source
evidence and exact launch contract added by fix-first review. An explicit wrapper `--project-root`
SHALL equal the root inferred from the launch CWD using Codex's supported default marker model; it
cannot override only the wrapper side of discovery.

Before a selected source is planned and again immediately before spawn, the adapter SHALL mirror
Codex 0.146.0's canonical-source namespace lookup. It SHALL inspect that source directory and every
ancestor for the first regular manifest spelling, in
`.codex-plugin/plugin.json`, `.claude-plugin/plugin.json`, `.cursor-plugin/plugin.json` order at
each ancestor, and use the same JSON `name` shape and malformed-first-file precedence. Any valid
manifest SHALL reject the selected source because Codex would expose `plugin:name` while the
session override enables the portable base name. Existing namespaced Skills MAY remain indexed by
their unqualified frontmatter name: `:` is outside SkillMount's portable grammar, so this can only
cause a conservative false conflict, not a missed portable-name collision. Full qualified-name
rendering remains compatibility-certification work. Each candidate manifest SHALL be reopened
without blocking on a Unix special file, verified as regular after open, and read through a 64 KiB
local bound. Read uncertainty or a crossed bound SHALL fail the plan; only a completely read,
regular JSON file may participate in malformed-first-file precedence.

Only bounded `exec`, `exec review`, and root `review` launches SHALL cross the child boundary.
Interactive TUI launches fail before state access because Codex 0.146.0 can reread a newly changed
higher-precedence managed layer after spawn; repeated pre-spawn probes do not bind that lifetime.
Command-free `inspect` remains a read-only inventory operation rather than a certified session.

## Alternatives

- Keep the two-entry backing state table. Rejected because it adds a helper link and a placement
  branch without making another Skill visible; it also obscures which namespace owns new paths.
- Keep a list of scopes and search it per selected Skill. Rejected because duplicate handling then
  depends on loop order and repeatedly reconstructs the same name relation. A merged index makes
  duplicate retention explicit while keeping direct occupancy independent.
- Trust Codex's duplicate ordering and mount despite a foreign same-name Skill. Rejected because
  loader ordering is not a SkillMount ownership contract and has changed across Codex versions.
- Mount at launch CWD to avoid project-root configuration differences. Rejected because it changes
  the project ownership boundary and would scatter transient entries through nested directories.

## Consequences

- New projects receive only `.agents/skills`; `.codex` is never created by the Codex adapter.
- An existing `.agents/skills -> .codex/skills` layout still works because placement uses the
  logical `.agents/skills` path and respects its terminal directory.
- Different files declaring one name remain a conflict even when one resolves to the selected
  source. Outside the mutable bundled cache, a regular directory reached through a linked
  collection is reusable when its canonical identity is exactly the selected source.
- Recursive inspection and nested physical locks cost more filesystem work and lock files. The
  bounded fail-closed result is intentionally stricter than Codex's warning-and-partial-load result
  because a partial conflict inventory cannot authorize mutation.
- Native non-Unicode entry names and Windows roots outside the adapter's conservative ordinary
  file-URI subset are rejected even when the operating system can enumerate them. This prevents a
  native-only success from standing in for Codex's different path model.
- Existing plugin-provided Skills are namespace-qualified with `:` and cannot collide with
  SkillMount's portable names. Their unqualified frontmatter name is conservatively retained and
  can cause a false-positive conflict. Selected sources are rejected when any of Codex's three
  manifest spellings would qualify them, preventing the injected enable rule from addressing a
  different name; full qualified-name rendering remains deferred.
- Ordinary persistent project-root marker changes are overridden for this bounded session. A
  legacy managed layer that outranks session flags is rejected rather than approximated.
- A source named like one of the pinned embedded Skills is rejected even when a local configuration
  would later disable that embedded Skill. System collisions cannot be skipped. This conservative
  false positive avoids relying on mutable cache contents or undocumented enablement ordering.
- A mutating session using another Codex release returns usage category 64 before SkillMount state
  or mounts. Supporting another release requires revalidating roots, metadata parsing, embedded
  names, and launch semantics together rather than borrowing the 0.146.0 contract.
- A contained `SKILL.md` file link can remain valid for another adapter, but it is rejected for
  Codex because the supported loader does not return file links from its discovery walk.

## Verification

- `src/agent/tests.rs` covers merged `.agents`/`.codex` duplicates, all global roots, separate
  destination occupancy, recursive frontmatter identity, depth and hidden-directory boundaries,
  missing-name and scalar-repair compatibility, linked collections, system link policy, the
  embedded-name manifest, unskippable mutable-cache conflicts, traversal-policy-safe terminal
  folding, response-budget accounting, same-source reuse outside the cache, and complete
  logical/physical lock resources.
- `tests/transaction.rs::a_conflict_introduced_after_preliminary_discovery_is_seen_under_lock`
  introduces a conflict at the unlocked checkpoint and proves the locked rebuild rejects it before
  a journal opens.
- `tests/transaction.rs::codex_sessions_reaching_one_nested_collection_through_distinct_links_serialize`
  uses distinct projects and user roots and proves their shared terminal physical identity
  serializes mutation.
- `tests/read_only.rs` proves new `.agents/skills` plans remain non-mutating, root-changing Codex
  arguments fail before state creation, plugin manifests are bounded, and native non-Unicode
  directory entries fail closed where the host filesystem permits them. Platform unit tests cover
  non-Unicode native names and Windows ordinary-versus-opaque discovery roots.
- `tests/codex_session.rs` proves exact-version rejection and re-probing, canonical `CODEX_HOME`
  propagation, injected session arguments, post-apply compatibility cleanup, and the mounted
  fake-child lifetime.
- `src/paths.rs` and `src/catalog/tests.rs` prove wrapper/child root and home agreement, linked Git
  marker handling, and Codex's regular-file-only `SKILL.md` boundary.
