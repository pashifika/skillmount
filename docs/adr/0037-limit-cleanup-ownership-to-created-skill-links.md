# ADR 0037: Limit Cleanup Ownership to Created Skill Links

- **Status:** Accepted; vector-only recovery presentation superseded by ADR 0038
- **Date:** 2026-08-08
- **Supersedes:** ADR 0014's post-placement incomplete-journal claim and ADR 0015's
  journal-backed retained-candidate rule, both only for helper-directory residue; ADR 0016's
  journal-retention half of its failed-disposition clause, only for a helper directory, which the
  ADR 0025 and ADR 0027 records did not name; ADR 0025's descendant-before-shared-helper batch
  cleanup ordering and its matching consequence; ADR 0027's verification claim that the overlapping
  kept-journal test derives cleanup order from recorded ownership; and ADR 0034's decision 2
  classification of `.omp` and `skills` as transaction-owned directory actions

## Context

A successful session could become a cleanup failure without any Skill link being left behind.
`MountAction::CreateDirectory` and `MountAction::CreateDirectoryLink` shared one
`is_transaction_owned` answer, `TransactionJournal::reversible_actions` fed both kinds into one
reverse pass, and every entry that pass declined to remove reached
`CleanupReport::needs_attention`.

The reproduction is an executable-seam OMP session whose child writes one unrelated file under the
`.omp` directory the transaction created. Cleanup removed the selected Skill link, then
`remove_empty_directory` correctly refused to delete a non-empty directory. The generic report read
that refusal as unresolved mount ownership: it retained the journal, replaced child success with
filesystem status `73`, and printed the same retained path twice inside one semicolon-delimited
diagnostic whose recovery guidance was a list of raw `argv[n]` fragments. The observed pre-change
run is recorded at
`rasen/changes/human-readable-recovery-hints-and-manage-only-created-skill-links/evidence/01-pre-fix-omp-scope-residue.txt`.

Nothing about that outcome was safe-by-necessity. Once every created Skill link is gone, no
externally selected Skill is visible through the namespace, so the leftover directory exposes
nothing. The same policy could equally fail a Codex `.agents/skills` chain, a project-mode Claude
`.claude/skills` chain, or a Claude staging root on either supported platform.

## Decision

A created Skill link is the only cleanup-critical filesystem entry. Helper directories remain
journalled write-ahead, staged as transaction-unique empty siblings, placed without replacement, and
observed no-follow, but their removal is best-effort housekeeping:

1. Cleanup, rollback, automatic recovery, and `asm cleanup` prune a recorded helper directory only
   while its recorded identity still matches and it is empty, and never recursively.
2. A helper directory that is non-empty, replaced, ownership-uncertain, or whose prune fails is left
   exactly as it is, is reported as preserved scaffolding with its reason, and its action is
   durably reconciled. Preserved scaffolding alone MUST NOT retain a journal, replace a child
   status, or produce recovery guidance.
3. For a `mkdir` action the stable on-disk `rolled_back` label means that cleanup responsibility was
   reconciled; unlike a link action it does not assert physical absence. The journal schema, the
   `mkdir`/`link`/`reuse` operation labels, and the action-status labels are unchanged, so a
   pre-change journal receives this policy without migration.
4. An unremoved or ownership-uncertain created Skill link, and any journal-persistence failure,
   remain fail-closed exactly as before: the journal is retained, child-versus-cleanup exit
   precedence is unchanged, and release still requires proven managed-process-domain death or an
   explicit operator assertion.

Because a preserved directory can no longer block another transaction, batch cleanup order carries
no policy. Explicit cleanup keeps its single shared-lock claim, its reload under those locks, and
the ordinary transaction pass, and reconciles claimed journals in deterministic scan order.

Session-cleanup failure, quarantined-journal recovery, and explicit-cleanup retry guidance render
every complete structured diagnostic before one recovery footer. Recovery operations remain
executable-plus-argument native values, each external value retains the existing reversible
`render` seam, and a labelled vector remains the universal representation. ADR 0038 supersedes only
the vector-only presentation rule by permitting a proved command for one unambiguously observed
shell family; unknown or rejected cases still receive the vector.

## Alternatives

**Keep helper directories cleanup-critical.** Rejected because it makes an unrelated writer's file
decide whether an otherwise complete session failed, while removing that file is exactly what
SkillMount must never do.

**Never attempt directory pruning.** Rejected because it is the smallest implementation but leaves
one unique Claude staging tree per normal session plus avoidable empty project scaffolding. Pruning
is worth keeping as housekeeping; it is not worth making it correctness authority.

**Require the discovery directories to exist before launch.** Rejected because it turns ordinary
first use into manual setup and does not remove the residue case.

**Delete known generated files, use an allowlist, or remove recursively.** Rejected because a name
proves no ownership, an allowlist would drift across Agents and operating systems, and recursive
removal is the one operation whose mistake is unrecoverable.

**Serialize a cleanup-policy field in the journal.** Rejected because the already serialized
operation kind determines the policy. A separate field would admit invalid combinations and force a
schema migration for no additional behavior.

**Move directory creation outside the journal.** Rejected because a crash would then leave an
unrecorded planned mutation, weakening the write-ahead boundary this crate rests on.

**Render a portable or unconditional copy-paste shell command.** Rejected for the reason ADR 0025
already records: POSIX shells, PowerShell, and native non-Unicode values share no portable quoting
contract. ADR 0038 later accepts only detected-shell commands whose narrow encoder proves an exact
native round trip, with the labelled vector as fallback.

## Consequences

- A successful session no longer guarantees that empty discovery scaffolding such as
  `.agents/skills`, project-mode `.claude/skills`, or `.omp/skills` disappeared. Only the links it
  created are guaranteed absent. Normal best-effort pruning still produces the previous empty-layout
  result when nothing interferes.
- A directory the pass could not prune is preserved without a journal. This is intentional: it
  exposes no selected external Skill, and the alternative is deleting content SkillMount did not
  create.
- Directory creation failures and journal-persistence failures still fail normally. Only
  post-link-removal housekeeping is non-critical, and verbose session output and the `asm cleanup`
  report still name the preserved path and its reason.
- The `rolled_back` label now carries a kind-dependent meaning. Link semantics are unchanged, so the
  weaker directory meaning cannot leak into link recovery, but a reader of the journal format must
  consult the operation to interpret it.
- Reverting this decision needs no data conversion. An older binary reading a journal written by
  this one sees ordinary scaffolding plus already terminal actions; its recovered behavior is more
  conservative, never more destructive.
- ADR 0014's cooperating-session lock scope and last-boundary identity check, ADR 0015's
  first-observation evidence boundary and prohibition on unchecked pathname rollback, ADR 0016's
  POSIX-disposition mechanism and its rule that a failed removal is never reported as removed, ADR
  0019's proven-empty process domain before any cleanup callback, ADR 0022's `supervising`
  quarantine, ADR 0025's shared-lock batch, ADR 0027's reload-under-lock and drift failure, and ADR
  0034's OMP destination, link ownership, locking, discovery, and launch contract all remain in
  force. ADR 0038 alone replaces the vector-only presentation rule. No claim here is stronger than
  those records support: the guarantee for a preserved directory is that SkillMount did not touch
  it, not that no other actor will.
- No recursive remover, junk-file allowlist, shell invocation, dependency, CLI flag, exit category,
  journal schema version, pinned Agent version, unsafe allowlist entry, or supported target changes.
- `docs/architecture.md`, `README.md`, the six modified capability specifications, and the
  transaction, journal, mount, adapter, and session tests are updated in the same product change.

## Verification

- `tests/omp_session.rs::unrelated_content_under_the_omp_scope_survives_without_failing_the_session`
  reproduces the reported failure and requires child success, a surviving unrelated file, an absent
  Skill link, no journal, and no recovery diagnostic.
- `tests/codex_session.rs` and `tests/claude_session.rs` cover the same contract for a created
  `.agents/skills` chain, Claude default staging, and `--mount-mode=project`, and separately require
  a genuine link mismatch to keep status `73`, its journal, and one structured recovery block.
- `tests/transaction.rs` covers apply rollback, ordinary cleanup, automatic recovery, and explicit
  cleanup for directory-only residue, a pre-change `mkdir` journal, the shared-lock claim for
  overlapping kept journals, and termination at the debug-only `scaffolding-reconciled` checkpoint
  followed by a real second invocation that completes.
- `src/transaction/tests.rs` and `src/journal/tests.rs` pin the exhaustive creation/disposition
  classification, the stable label round trips, and the preserved-scaffolding report channel.
- `src/app.rs` unit tests pin one block per condition, multiple retained paths, multiple quarantined
  journals, macOS control bytes, Windows unpaired UTF-16, forged-line input, and one final,
  stable-deduplicated detected-shell command or native-vector fallback; raw `recovery[n] argv[n]`
  fragments remain forbidden.
- `tests/read_only.rs` keeps `inspect` and `--dry-run` free of any directory, link, lock, journal,
  recovery, or child side effect.
- Native Apple Silicon macOS and native Windows x64/x86 CI run the guarded transaction and Agent
  suites; a cross-compile is type evidence only and proves no native junction, disposition, or
  UTF-16 behavior. The observed local runs are recorded under
  `rasen/changes/human-readable-recovery-hints-and-manage-only-created-skill-links/evidence/`.
