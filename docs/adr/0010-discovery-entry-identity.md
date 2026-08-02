# ADR 0010: Identify Pre-Existing Discovery Entries by Raw Name and Comparison Key

- **Status:** Accepted
- **Date:** 2026-08-02
- **Supersedes:** the `ExistingSkill` shape in V2 design section 25

## Context

Read-only mount planning has to enumerate every Skill an agent can already see, because V2 design
section 15.7 requires each adapter to preflight the complete discovery namespace rather than only
the directory SkillMount intends to modify.

The V2 proposed data model types such an entry as:

```rust
pub struct ExistingSkill {
    pub mount_name: SkillName,
    pub comparison_key: SkillNameKey,
    pub source_canonical: Option<PathBuf>,
    pub kind: PathEntry,
}
```

`SkillName` is SkillMount's *portable mount name*, and `SkillName::parse` in `src/domain.rs`
enforces it: 1–64 bytes, valid UTF-8, lowercase ASCII letters, digits, and single interior hyphens.
That grammar is a constraint SkillMount imposes on names it creates.

Entries already present in `<project>/.agents/skills`, `~/.claude/skills`, or a passthrough
`--add-dir` scope are not created by SkillMount. Users, the agents themselves, and other tooling
write them, and nothing obliges them to satisfy that grammar. Directory names that exist in
practice and fail `SkillName::parse`: `My_Skill` (uppercase and underscore), `rust--review`
(consecutive hyphens), `-draft` (boundary hyphen), and any name that is not valid UTF-8 on the host
encoding.

## Decision

`ExistingSkill` in `src/agent/mod.rs` stores the platform-native name plus the comparison key, and
does not store a `SkillName`. `SkillNameKey` in `src/domain.rs` is widened from `String` to
`OsString` so a single comparison-key type serves both source overlay and discovery inspection.

```rust
pub struct ExistingSkill {
    pub comparison_key: SkillNameKey,
    pub raw_name: OsString,
    pub entry: PathBuf,
    pub kind: PathKind,
    pub source_canonical: Option<PathBuf>,
}
```

The portable-name grammar continues to apply to every name SkillMount *creates*: a selected Skill
still resolves through `SkillName`, so no unsafe name is written to a destination.

## Alternatives

**Drop entries that fail to parse.** Rejected because it makes conflict detection unsound. Section
15.6 states that "an exact destination path being absent is insufficient if `A` or another case
variant already occupies logical key `a`". An entry named `My_Skill` occupies logical key
`my_skill`. Dropping it means planning reports a clear destination, the transaction creates a link,
and the child sees two entries under one logical name, resolved by precedence rules the agent does
not document. Section 15.7 exists to prevent exactly that.

**Fail the run when an entry fails to parse.** Rejected because it makes SkillMount reject project
state it has no authority over. An unrelated directory in the user's own `~/.claude/skills` would
block every mount, including mounts whose names do not collide with it. This also contradicts
ADR-005: failing closed applies to conflicts with the requested operation, not to the mere presence
of unrelated files.

**Keep both `SkillName` and a separate raw name.** Rejected as a redundant field that is `None` or
synthetic for exactly the entries that matter, while still requiring every consumer to handle the
absent case. It adds a field without removing a branch.

## Consequences

- Conflict detection covers every entry the child can see, not the subset SkillMount could have
  authored.
- `SkillNameKey` holds an `OsString`, so non-UTF-8 entry names participate in comparison. ASCII-only
  case folding is retained deliberately: full Unicode folding is locale-sensitive and would make the
  comparison key depend on the host.
- The public `SkillNameKey::as_str` accessor is replaced by `as_os_str`, and `Display` renders
  through `Path::display` rather than `OsStr::display`, which is newer than the crate MSRV of
  1.85.0. This is a breaking change to an unreleased public type; no consumer exists outside the two
  bundled binaries.
- The previous `SkillNameKey(String)` and the private `NativeNameKey(OsString)` in
  `src/catalog/discover.rs` collapse into one type, removing the risk that overlay folding and
  discovery inspection disagree about what counts as the same logical name.
- Diagnostics quote the raw name and path, which may contain characters the destination grammar
  forbids. Verbose output uses the reversible platform representation required by the
  `read-only-inspection` specification.

## Verification

- `src/agent/tests.rs` asserts that a scope containing `My_Skill` reports an occupant under key
  `my_skill`, and that a selected Skill named `my-skill` therefore conflicts with it. The test fails
  if the entry is dropped.
- `src/agent/tests.rs` asserts that a scope containing a name that fails `SkillName::parse` does not
  by itself fail planning when no selected Skill collides with it.
- `src/catalog/tests.rs` continues to assert case-variant overlay folding, which now exercises the
  same `SkillNameKey` type as discovery inspection.
