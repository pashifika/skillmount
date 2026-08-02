# Contributing to SkillMount

Read [docs/architecture.md](docs/architecture.md) before changing cross-module behavior. It is the
tracked current-state baseline for responsibilities, dependency and mutation boundaries, safety
invariants, supported targets, and implementation status. Update affected baseline material in the
same product change; when replacing a normative decision, also add or update a focused ADR from
[docs/adr/0000-template.md](docs/adr/0000-template.md).

SkillMount uses a release-line workflow. Normal changes move from a topic branch
to the active development line and are then promoted to `main`:

```text
main <- dev/<major>.<minor>.x <- <topic-prefix>/<slug>
```

`main` is the default, release-ready branch. The initial development line is
`dev/0.1.x`. Do not push ordinary changes directly to either branch.

## Create a branch

Create a topic branch from the development line that will receive the change.
For the initial line:

```bash
git fetch origin
git switch --create feat/short-description origin/dev/0.1.x
```

Use exactly one of these prefixes:

- `feat/` for new behavior
- `fix/` for bug fixes
- `perf/` for performance work
- `refactor/` for internal restructuring
- `docs/` for documentation
- `test/` for test-only changes
- `build/` for build-system or dependency work
- `ci/` for continuous-integration changes
- `chore/` for repository maintenance
- `revert/` for reverting an earlier change

The slug after the prefix must be non-empty and the whole branch name must be a
valid Git ref. Use a short, descriptive, lowercase kebab-case slug such as
`fix/windows-path-resolution`; never begin a branch name with `/`.

Open the topic pull request against its matching `dev/<major>.<minor>.x` branch.
Only development-line branches may target `main` during normal promotion. A
narrow CI exception permits authenticated, same-repository Dependabot updates;
similarly named user or fork branches do not receive that exception.

When starting a later release line, create `dev/<major>.<minor>.x` from the
current `main`, then create all topics for that release from the new development
line. The branch-policy check accepts one numeric major component, one numeric
minor component, and the literal `.x`, for example `dev/1.2.x`.

## Merge and promote changes

Repository pull requests use a regular merge commit by default so the commits
reviewed in the pull request remain part of the branch history. Squash merge is
also available when the author intentionally wants to replace a noisy topic
history with one commit. Rebase merge is disabled. Before merging a topic pull
request:

1. Bring the topic branch up to date with its development-line base.
2. Wait for the required `gate` check in the `CI` workflow to succeed.
3. Resolve every review conversation.
4. Merge the pull request into the development line, normally with a merge
   commit.

GitHub automatically deletes merged, unprotected topic branches when safe. The
protected development line remains available for the rest of the release.

When a release line is ready, open a pull request from that exact
`dev/<major>.<minor>.x` branch to `main`, update it with `main`, wait for
the `gate` check in the `CI` workflow, resolve all conversations, and merge the
promotion. Use a regular merge commit unless the pull request deliberately
documents a reason to squash its history.

The repository initially requires zero approving reviews because it has one
maintainer, and an author cannot approve their own pull request. Pull requests,
strict CI, and resolved conversations are still mandatory. Once
a second regular reviewer has write access, update both branch rulesets together
to require at least one approving review, document the decision, and verify the
effective rules before treating the higher count as policy.

## Emergency recovery

There is no standing bypass for protected branches. If a ruleset itself prevents
an urgent repair, an administrator may temporarily edit or disable only the
affected named ruleset under **Settings > Rules > Rulesets**.

Before the emergency change, record the reason, actor, time, ruleset ID, and
exported payload. Make the smallest necessary repair through a pull request when
possible. Immediately afterward, restore the reviewed payload, reactivate the
ruleset, query the effective rules for the affected branch, and append the
results to the incident or change audit. A temporary ruleset edit is a recovery
procedure, not a normal merge path or a permanent bypass.
