# ADR 0038: Render Recovery for the Detected Shell

- **Status:** Accepted
- **Date:** 2026-08-08
- **Supersedes:** ADR 0025's requirement that retry arguments are never rendered as an executable
  shell command and its blanket rejection of copy-paste commands; ADR 0037's decision and
  alternative that recovery is always a labelled vector and never shell syntax. Their native-value,
  shell-free execution, process-liveness, ownership, locking, and non-Unicode safety clauses remain
  in force.

## Context

ADR 0025 made the native executable and arguments the recovery source of truth and rejected one
portable command string. That remains correct: POSIX shells, PowerShell, Command Prompt, arbitrary
Unix bytes, and Windows UTF-16 do not share one quoting contract. ADR 0037 consequently rendered
one labelled line per native value. The representation is precise and injection-safe, but an
operator must reconstruct the command manually even when the path is ordinary text and the
invoking shell is known.

Native Windows probes established two relevant facts. A child launched directly by `cmd.exe`,
Windows PowerShell, or PowerShell 7 observes the corresponding shell image in its process ancestry,
so the common direct case can select one renderer. A PowerShell invocation routed through
`cmd.exe /c` observes both shell families while control returns to PowerShell, so a nearest-parent
name is not proof of the prompt that will receive a later paste. Quote probes likewise showed that
PowerShell single quotes preserve `$` while Command Prompt double quotes group spaces, but percent
and delayed-exclamation expansion plus non-Unicode values require rejection or a distinct encoder.

The recovery operation itself does not change. For example, a labelled executable plus three
arguments and a correctly encoded shell line both describe the same native sequence. The decision
is whether SkillMount can prove that rendering and parsing the convenience line round-trips to that
sequence.

## Decision

Recovery SHALL remain an executable plus separate platform-native arguments internally. SkillMount
SHALL compare and stable-deduplicate those native sequences before rendering and SHALL never store
or execute a rendered shell command.

Near application startup, SkillMount SHALL capture a best-effort invocation-shell hint. On Windows,
a bounded Tool Help ancestry walk ends at a recognized terminal or Windows session-bootstrap
process; ancestors beyond that prompt boundary cannot regain control, and Windows commonly records
the PID of an already-exited launcher there. Before that boundary, a missing link, cycle, duplicate
PID, or overlong chain is inconsistent evidence. `powershell.exe` and `pwsh.exe` form one
PowerShell family and `cmd.exe` forms the Command Prompt family; boundaries and other wrappers do
not choose a family. Exactly one family selects it. Zero or mixed families, observation failure,
premature disappearance, PID uncertainty, or an unsupported platform yields `Unknown`. The hint is
presentation-only and MUST NOT affect cleanup authority, process-death proof, scope, mutation, or
exit status.

After every complete error or warning block, one recovery footer SHALL render each distinct native
operation once in first-seen order:

1. For a known PowerShell family and non-empty display-safe Unicode values, render one PowerShell
   command with each native value represented as a single-quoted literal and embedded apostrophes
   doubled. An empty native argument uses the vector because Windows PowerShell 5.1 drops it.
2. For a known Command Prompt family and values admitted by the native-tested encoder, render one
   Command Prompt command with exact argument grouping. Expansion-sensitive percent or exclamation
   values, controls, non-Unicode values, and unproved quoting cases are rejected to the fallback.
3. For `Unknown` or any rejected value, render the labelled executable and numbered arguments with
   the existing reversible native-value encoding. Do not print both shell variants as a guess.

The recognized invocation product chooses `asm` or `skillmount`; an unrecognized renamed binary
uses the canonical `asm` recovery identity. Session diagnostics remain on stderr, explicit cleanup
remains on stdout, and child stdout remains untouched.

## Alternatives

**Always keep the labelled vector.** Safest and fully portable, but it leaves the ordinary operator
with unnecessary transcription and was rejected as the only presentation.

**Always print both PowerShell and Command Prompt commands on Windows.** Avoids ancestry inspection,
but makes the operator choose and doubles every recovery operation. Rejected because direct
invocations identify one family and ambiguity already has a safer vector fallback.

**Choose the immediate parent executable.** Smallest detector, but the nested PowerShell →
`cmd /c` probe selects Command Prompt even though the next prompt is PowerShell. Rejected in favor
of bounded ancestry plus mixed-family fallback.

**Guess from `COMSPEC`, `PSModulePath`, `SHELL`, or terminal variables.** Rejected because inherited
environment describes installed or ancestor tooling, not necessarily the parser that launched
SkillMount or receives a later paste.

**Accept every textual path in each shell encoder.** Rejected because Command Prompt expansion,
line controls, quoting edges, and native non-Unicode values can change an argument or forge output.
A convenience renderer must be narrower than the native argument model.

**Add `--recovery-shell`.** An explicit override could remove ambiguity, but it expands the public
CLI for a diagnostic-only convenience. The fail-closed vector already handles the uncommon case,
so no flag is added.

## Consequences

- Ordinary direct Windows invocations receive one pasteable recovery command for their detected
  shell rather than a multi-line vector or two alternatives.
- Wrapped, nested, unsupported, uninspectable, expansion-sensitive, control-bearing, and
  non-Unicode cases remain correct but less convenient through the labelled vector.
- Process ancestry becomes one new best-effort observation at startup. Failure is silent and cannot
  stop `inspect`, a dry run, a session, `doctor`, or `cleanup`.
- Windows implementation adds the Tool Help feature to the existing `windows-sys` dependency and
  keeps all raw calls in the already allowed `src/process/windows_ffi.rs` boundary. No dependency,
  unsafe allowlist entry, deployment target, permission, elevation, or shell process is added.
- Public recovery text changes, so README examples, the architecture baseline, operator-recovery
  specifications, and executable-seam assertions change in the same product change.
- Reverting the renderer requires no journal or state migration because native recovery operations
  remain the sole stored and computed form.

## Verification

- Renderer unit tests pin PowerShell spaces and apostrophes, PowerShell 5.1 empty-argument fallback,
  Command Prompt spaces and trailing separators, recognized `asm`/`skillmount` identity, exact
  deduplication, and vector fallback for controls, expansion-sensitive input, and unpaired UTF-16.
- Ancestry-classifier tests pin direct PowerShell, direct Command Prompt, ignored wrappers,
  recognized prompt boundaries, mixed families, premature missing parents, bounded walks, and
  observation failures as non-authoritative data.
- Native Windows executable-seam tests parse accepted commands through real Windows PowerShell and
  `cmd.exe` into the fake agent and compare its recorded `args_os()` values with the source native
  operation.
- Session, quarantine, explicit-cleanup, transaction, and process-supervision suites pin stream and
  exit precedence, one footer after all diagnostics, deterministic deduplication, no shell launch,
  and unchanged ownership/liveness gates.
- Native PowerShell and Command Prompt smoke runs reproduce a retained-link failure and observe one
  command for the direct shell; an unknown-parent run observes the labelled fallback.
