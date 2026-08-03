# ADR 0020: Model Codex Discovery by Observed Roots and Frontmatter

- **Status:** Superseded by ADR 0021
- **Date:** 2026-08-03
- **Supersedes:** ADR 0010 for Codex discovery identity only

## Context

The architecture baseline modeled every ancestor `.agents/skills` directory but treated only the
project root's `.codex/skills` as a backing-store compatibility candidate. It also identified an
existing Skill by the immediate child directory name. Those rules were deliberately provisional
until the first child-launch integration revalidated the Codex contract.

The current public Codex documentation says that repository Skills are scanned recursively under
`.agents/skills` from the launch CWD through the repository root and that symlinked Skill folders
are supported. The current open-source loader also retains project `.codex/skills` roots and
deduplicates loaded Skills by `SKILL.md` path rather than by frontmatter name.

On 2026-08-03, an isolated fixture was inspected with `codex-cli 0.146.0` and
`codex debug prompt-input`. The model-visible list contained synthetic Skills from both
`.agents/skills` and `.codex/skills` at the repository root and a nested ancestor. It also showed:

- a recursively nested `SKILL.md`;
- a Skill reached through a symlinked collection;
- the frontmatter `name` when it differed from its directory name;
- both entries when two different `SKILL.md` files declared the same name; and
- no entry for a directory that contained no `SKILL.md`.

This evidence conflicts with the backing-only and directory-name assumptions. Keeping either
assumption would let a child see a same-name Skill that planning never inspected.

## Decision

The Codex adapter inspects both `.agents/skills` and `.codex/skills` at every directory from launch
CWD through project root. The project `.agents/skills` state table still chooses where new mounts
are written, including the existing `.agents/skills -> .codex/skills` compatibility layout, but it
does not make any other root invisible.

Within each Codex root, the adapter recursively discovers `SKILL.md` files, follows resolvable
directory links without revisiting a terminal directory, and identifies visible Skills by their
frontmatter `name`. It keeps direct destination-entry occupancy as separate evidence: a path can
block creation even when it is not a valid Skill, while a differently named or deeply nested path
can still conflict by frontmatter name. Multiple entries under one logical name are all retained
for conflict evaluation.

## Alternatives

**Follow only the public `.agents/skills` table.** Rejected because the supported installed CLI and
current open-source loader both expose ancestor `.codex/skills` roots. Documentation alone cannot
make those roots invisible to the child SkillMount launches.

**Keep directory names as conservative logical identities.** Rejected because this both misses a
real conflict when frontmatter differs and invents a conflict for a directory with no `SKILL.md`.
Exact destination occupancy remains conservative without conflating the two facts.

**Reject any duplicate name or invalid existing `SKILL.md` globally.** Rejected because Codex can
list duplicate names and reports malformed Skills independently. SkillMount fails or skips only
when a requested logical name or destination is affected, while retaining warnings for malformed
entries it could not model as Skills.

## Consequences

- Codex discovery walks more paths than the earlier model and may reject a mount that would have
  collided with a nested or legacy Skill.
- `.agents/skills` remains the preferred project mount entry; `.codex/skills` remains useful as a
  compatibility backing store but is also acknowledged as a live discovery root.
- Recursive traversal is iterative, bounded, and terminal-directory deduplicated so a symlink
  cycle or an unexpectedly large tree fails before mutation rather than hanging or overflowing.
- Claude discovery keeps ADR 0010's raw direct-entry identity because this evidence changes only
  the Codex loader contract.
- A later Codex version may remove legacy roots. Removing them from the adapter requires new
  official and executable evidence plus another baseline update; inspecting an extra existing root
  is preferred while the supported compatibility range remains unsettled.

## Verification

- `src/agent/tests.rs` covers ancestor `.agents` and `.codex` roots, recursive and symlinked
  discovery, frontmatter/directory-name divergence, duplicate logical names, exact non-Skill
  destination occupancy, terminal deduplication, and lock resources.
- Feature-gated Codex session integration tests launch the fake agent only after apply and assert
  the mounted and pre-existing Skills remain visible until child exit.
- The external evidence can be repeated with a fixture containing unique frontmatter names and
  `codex debug prompt-input`; the version and observation date above bound the claim when a live
  Codex binary is unavailable in CI.
