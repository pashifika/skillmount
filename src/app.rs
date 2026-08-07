//! Shared application orchestration for read-only and mutating commands.

use std::ffi::OsString;
use std::io::{self, Write};
use std::path::Path;
use std::process::ExitCode;

use clap::error::ErrorKind;

use crate::agent::{AgentAdapter, DiscoverySnapshot, adapter};
use crate::catalog::{CatalogRequest, resolve_catalog};
use crate::cli::{CompletionInput, InspectAgent, ParsedCommand, parse_command_from};
use crate::domain::{AgentId, LinkMode, MountMode, RunContext, SkillCatalog};
use crate::error::{AppError, CatalogError, ExitCategory, LinkError, PlanError};
use crate::journal::store::RejectedJournal;
use crate::journal::{TransactionId, store};
use crate::lock::acquire::{HeldLocks, LockOwner, LockPolicy};
use crate::mount::MountPlan;
use crate::paths::{resolve_inspection, resolve_session};
use crate::process::{
    CleanupFailure, ProcessSupervisor, SupervisionDiagnostic, SupervisionRequest, map_exit,
};
use crate::render;
use crate::transaction::cleanup::CleanupReport;
use crate::transaction::{Transaction, recover};

/// Maximum discovery/recovery passes allowed while a mutating lock set expands.
const MAX_LOCK_SET_PASSES: usize = 8;

pub(crate) fn run_from<I, T>(args: I) -> ExitCode
where
    I: IntoIterator<Item = T>,
    T: Into<OsString> + Clone,
{
    let mut args = args.into_iter().map(Into::into).collect::<Vec<OsString>>();
    if args.len() == 1 {
        args.push(OsString::from("--help"));
    }
    let diagnostic_args = args
        .iter()
        .map(|argument| OsString::from(render::os_value(argument, true)))
        .collect::<Vec<_>>();

    let command = match parse_command_from(args) {
        Ok(command) => command,
        Err(error) => {
            let original_kind = error.kind();
            let Err(diagnostic_error) = parse_command_from(diagnostic_args) else {
                return report_clap_fallback(original_kind);
            };
            return report_clap_error(&diagnostic_error, original_kind);
        }
    };
    let command = match command {
        ParsedCommand::Completions(input) => {
            let mut stdout = io::stdout().lock();
            return match execute_completion(input, &mut stdout) {
                Ok(code) => ExitCode::from(code),
                Err(error) => report_error(&error),
            };
        }
        command => command,
    };

    let invocation_cwd = match std::env::current_dir() {
        Ok(path) => path,
        Err(error) => {
            return report_error(&AppError::Internal(format!(
                "cannot capture invocation CWD: {error}"
            )));
        }
    };
    match execute(command, &invocation_cwd) {
        Ok(code) => ExitCode::from(code),
        Err(error) => report_error(&error),
    }
}

fn execute_completion(input: CompletionInput, writer: &mut dyn Write) -> Result<u8, AppError> {
    match crate::completion::generate(input, writer) {
        Ok(()) => Ok(0),
        Err(error) if error.kind() == io::ErrorKind::BrokenPipe => Ok(0),
        Err(error) => Err(AppError::Internal(format!(
            "failed to write completion output: {error}"
        ))),
    }
}

fn execute(command: ParsedCommand, invocation_cwd: &Path) -> Result<u8, AppError> {
    match command {
        ParsedCommand::Completions(_) => Err(AppError::Internal(
            "completion request reached path-dependent command dispatch".to_owned(),
        )),
        ParsedCommand::Session(input) => {
            let dry_run = input.options.dry_run;
            let context = resolve_session(input, invocation_cwd)?;
            if !dry_run {
                return run_session(&context);
            }
            let report = plan_read_only(&context)?;
            emit(&render::render(&render::ReadOnlyReport {
                context: &context,
                catalog: &report.catalog,
                snapshot: &report.snapshot,
                plan: &report.plan,
                verbosity: context.options.verbosity,
            }))?;
            warn(&render::render_warnings(&report.catalog, &report.snapshot));
            Ok(0)
        }
        ParsedCommand::Inspect(input) => {
            let mut rendered = String::new();
            let mut warnings = Vec::new();
            // Every request is independent, exactly as `doctor` treats its Agents: one Agent's
            // refusal must not discard the reports the others already produced. Widening the
            // default selection to every Agent otherwise let a single Agent-specific environment
            // gate - OMP's `OMP_PROFILE`/`PI_PROFILE`/`PI_CONFIG_FILES` rejection - blank the
            // whole report for Codex and Claude too.
            let mut failures = Vec::new();
            for agent in inspected_agents(input.agent) {
                let report =
                    resolve_inspection(agent, &input.skills_dirs, input.validation, invocation_cwd)
                        .and_then(|context| {
                            plan_inspection(&context).map(|report| (context, report))
                        });
                let (context, report) = match report {
                    Ok(pair) => pair,
                    Err(error) => {
                        failures.push((agent, error));
                        continue;
                    }
                };
                if !rendered.is_empty() {
                    rendered.push('\n');
                }
                rendered.push_str(&render::render(&render::ReadOnlyReport {
                    context: &context,
                    catalog: &report.catalog,
                    snapshot: &report.snapshot,
                    plan: &report.plan,
                    verbosity: 0,
                }));
                warnings.extend(render::render_warnings(&report.catalog, &report.snapshot));
            }
            // Nothing was reportable, so the refusal is the whole result and keeps its own message
            // and exit category rather than being restated as a warning about a missing section.
            let Some(first) = failures.first().map(|(_, error)| error.category().code()) else {
                emit(&rendered)?;
                warn(&warnings);
                return Ok(0);
            };
            if rendered.is_empty() {
                let (_, error) = failures.remove(0);
                return Err(error);
            }
            for (agent, error) in &failures {
                warnings.push(format!(
                    "{} inspection was skipped: {error}",
                    agent.descriptor().display_name()
                ));
            }
            emit(&rendered)?;
            warn(&warnings);
            Ok(first)
        }
        ParsedCommand::Doctor(input) => {
            let outcome = crate::operator::doctor::run(&input, invocation_cwd)?;
            emit(&outcome.output)?;
            Ok(outcome.code)
        }
        ParsedCommand::Cleanup(input) => {
            let outcome = crate::operator::cleanup::run(&input, invocation_cwd)?;
            emit(&outcome.output)?;
            Ok(outcome.code)
        }
    }
}

/// Everything the read-only pipeline produced for one agent.
pub(crate) struct ReadOnlyOutcome {
    pub(crate) catalog: SkillCatalog,
    pub(crate) snapshot: DiscoverySnapshot,
    pub(crate) plan: MountPlan,
}

/// Runs the mutating half of a session: lock, recover, plan, apply, supervise, and clean up.
///
/// The order is not negotiable, and no complete plan is built before the lock. After the journal
/// preflight and any staging-control setup, discovery yields the lock set on its own. A
/// complete plan built earlier would be a mistake: a crashed session can leave a mount pointing at
/// a source this run did not select, the conflict table would refuse it, and the run would exit
/// before recovery ever got the chance to remove the very entry it was refusing.
///
/// The supervisor owns the single orderly cleanup call. It invokes cleanup only after proving no
/// child was spawned or the managed process domain is empty; uncertain liveness leaves the active
/// journal untouched for recovery. `--keep-mounts` reaches the transaction's terminal kept state
/// through that same callback.
fn run_session(context: &RunContext) -> Result<u8, AppError> {
    // Root-changing arguments and release-independent Agent controls are rejected before reading
    // or creating SkillMount state. No Agent process runs here: mount visibility and removal are
    // established by the journal, the locks, proven process-domain death, and ownership-verified
    // removal, none of which depend on the installed release. `doctor` owns version evidence.
    // See ADR 0036.
    let adapter = adapter(context.agent_id());
    adapter.validate_passthrough_args(&context.passthrough_args)?;
    adapter.validate_launch_invariants(context)?;

    // Unknown ownership state is checked before creating even SkillMount's own staging or lock
    // directories. Recovery scans again after locks are held, so a journal appearing between this
    // read-only preflight and acquisition still fails closed rather than entering a new plan.
    let scan = store::scan()?;
    if !scan.rejected.is_empty() {
        return Err(unreadable_journals_error(&scan.rejected));
    }

    // SkillMount's own storage is created here rather than planned as a transaction action. Every
    // session shares it, so an action that created it would make two concurrent runs contend on a
    // directory neither of them owns. Only a staging session needs it, and creating it anyway
    // would leave a Codex-only state root looking as though it had once staged something.
    if context.options.mount_mode == MountMode::Staging {
        crate::state::ensure_private_directory(&crate::state::session_root_base()?)?;
    }

    // The identifier is minted before discovery is inspected and used for both the staging root and
    // the journal name. Minting it this early is what keeps two concurrent Claude sessions apart:
    // the placeholder a preliminary plan uses is one shared path, so locking it would serialize two
    // sessions that in reality never touch the same directory.
    let transaction_id = TransactionId::mint();
    let context = RunContext {
        session_id: Some(transaction_id.clone()),
        ..context.clone()
    };
    let owner = LockOwner::for_transaction(&transaction_id);
    let policy = LockPolicy::from_env();

    let preliminary = adapter.inspect_discovery(&context)?;
    crate::checkpoint::reached(crate::checkpoint::Checkpoint::DiscoveryInspected, 1);
    let mut required_resources = preliminary.lock_resources;
    let mut locks = HeldLocks::acquire(&required_resources, policy, &owner)?;

    // Recovery or an external filesystem change can make the rebuilt snapshot add a physical key.
    // A new key that sorts after the held set can be appended safely. A key that sorts before it
    // requires dropping the set and taking the complete union in one order. That unlocked gap is
    // never hidden from the rest of the pipeline: recovery and filesystem inspection run again
    // after every restart and after every monotonic expansion.
    let mut stable = None;
    for _ in 0..MAX_LOCK_SET_PASSES {
        reconcile_incomplete_transactions(&context, &mut locks)?;

        // Built under the lock and after recovery, because recovery may have removed mounts
        // discovery saw a moment ago. A plan that disagrees with the filesystem it is about to
        // change is exactly what every precondition check exists to catch.
        let rebuilt = plan_read_only(&context)?;
        if locks.holds_all(&rebuilt.snapshot.lock_resources) {
            stable = Some(rebuilt);
            break;
        }

        extend_resources(&mut required_resources, &rebuilt.snapshot.lock_resources);
        if locks.requires_reacquire(&rebuilt.snapshot.lock_resources) {
            drop(locks);
            locks = HeldLocks::acquire(&required_resources, policy, &owner)?;
            continue;
        }

        locks.acquire_more(&rebuilt.snapshot.lock_resources, policy, &owner)?;
    }
    let rebuilt = stable.ok_or_else(|| {
        AppError::Temporary(format!(
            "the resource lock set did not stabilize after {MAX_LOCK_SET_PASSES} inspections; \
             nothing was mounted"
        ))
    })?;

    // Lock acquisition may wait behind a long-running session while managed configuration or
    // another hard launch control changes, so repeat those release-independent checks after the
    // lock set stabilizes.
    adapter.validate_launch_invariants(&context)?;

    warn(&render::render_warnings(
        &rebuilt.catalog,
        &rebuilt.snapshot,
    ));

    let session_output = render_session_output(&context, &rebuilt);

    let mut transaction = Transaction::open_with(
        &context,
        &rebuilt.catalog,
        &rebuilt.plan,
        &rebuilt.snapshot,
        &locks,
        transaction_id,
    )?;
    transaction
        .apply()
        .map_err(|failure| failure.into_error())?;
    if let Some(message) = automatic_junction_warning(&context, transaction.journal()) {
        warn(&[message]);
    }

    // Apply can itself take time. Repeat only the hard launch invariants at the child boundary. If
    // one changed, no child is spawned and the active transaction is released through the normal
    // evidence-checked cleanup path.
    verify_spawn_boundary(adapter, &context, &rebuilt, &mut transaction)?;

    if let Err(error) = transaction.begin_supervision() {
        match transaction.cleanup_required() {
            Ok(report) => warn(&report.describe()),
            Err(cleanup_error) => warn(&[format!(
                "recording agent supervision intent failed and cleanup also failed: {cleanup_error}"
            )]),
        }
        return Err(error);
    }
    if let Err(error) = emit_diagnostic(&session_output) {
        match transaction.cleanup_required() {
            Ok(report) => warn(&report.describe()),
            Err(cleanup_error) => warn(&[format!(
                "writing the session diagnostics failed and cleanup also failed: {cleanup_error}"
            )]),
        }
        return Err(error);
    }
    let journal_path = transaction.journal_path().to_path_buf();
    let request = SupervisionRequest::new(rebuilt.plan.launch.clone());
    let verbose_cleanup = context.options.verbosity > 0;
    let outcome = ProcessSupervisor::new().supervise(request, move || {
        cleanup_for_supervisor(&mut transaction, &journal_path, verbose_cleanup)
    });
    // Lock ownership covers the whole child lifetime and the cleanup callback. Keeping this
    // explicit prevents a future refactor from shortening the guard to the last planning use.
    drop(locks);

    let decision = map_exit(&outcome);
    report_supervision_diagnostics(&decision);
    Ok(decision.code)
}

fn render_session_output(context: &RunContext, outcome: &ReadOnlyOutcome) -> String {
    let report = render::ReadOnlyReport {
        context,
        catalog: &outcome.catalog,
        snapshot: &outcome.snapshot,
        plan: &outcome.plan,
        verbosity: context.options.verbosity,
    };
    let mut output = render::render_session_start(&report);
    if context.options.verbosity > 0 {
        output.push('\n');
        output.push_str(&render::render(&report));
    }
    output
}

fn automatic_junction_warning(
    context: &RunContext,
    journal: &crate::journal::TransactionJournal,
) -> Option<String> {
    let used_junction_fallback = context.options.link_mode == LinkMode::Auto
        && journal.actions.iter().any(|action| {
            action.operation == crate::journal::ActionOperation::CreateDirectoryLink
                && action.kind == crate::journal::RecordedKind::Junction
        });
    junction_policy_warning(
        context.agent_id(),
        context.options.link_mode,
        used_junction_fallback,
    )
}

fn junction_policy_warning(
    agent: AgentId,
    requested: LinkMode,
    used_junction: bool,
) -> Option<String> {
    (requested == LinkMode::Auto && used_junction).then(|| {
        let last_tested = adapter(agent).version_spec().last_tested_banner();
        format!(
            "automatic symlink fallback selected a Windows junction, but live {} junction compatibility is unverified: docs/compatibility.md has no passing evidence for this Agent/platform/link combination. The adapter's dated last-tested banner is {last_tested:?}, not evidence that this junction was exercised. This session will continue with the ownership-verified junction. Run the opt-in native smoke before claiming compatibility, or request --link-mode=symlink to fail instead of falling back",
            agent.label()
        )
    })
}

/// Repeats only the hard launch invariants at the child boundary.
///
/// The locked pre-apply snapshot and plan let an adapter ignore exactly the transaction-owned
/// actions it asked for. If one invariant changed, no child is spawned and the active transaction is
/// released through the normal evidence-checked cleanup path.
fn verify_spawn_boundary(
    adapter: &'static dyn AgentAdapter,
    context: &RunContext,
    outcome: &ReadOnlyOutcome,
    transaction: &mut Transaction,
) -> Result<(), AppError> {
    // Named so a test can stall a real session in exactly the window an external writer would use:
    // the mounts are applied and the recheck has not yet re-read the namespace.
    crate::checkpoint::reached(crate::checkpoint::Checkpoint::SpawnBoundary, 1);
    let compatibility = adapter.validate_spawn_boundary(
        context,
        &outcome.catalog,
        &outcome.snapshot,
        &outcome.plan,
    );
    if let Err(error) = compatibility {
        match transaction.cleanup_required() {
            Ok(report) => warn(&report.describe()),
            Err(cleanup_error) => warn(&[format!(
                "an Agent hard launch invariant changed before spawn and cleanup also failed: {cleanup_error}"
            )]),
        }
        return Err(error);
    }
    Ok(())
}

fn cleanup_for_supervisor(
    transaction: &mut Transaction,
    journal_path: &Path,
    verbose: bool,
) -> Result<(), CleanupFailure> {
    let recovery_command = cleanup_recovery_arguments(&transaction.journal().project_root);
    match transaction.cleanup() {
        Ok(report) => {
            if verbose {
                if report.removed.is_empty() {
                    inform(&["cleanup removed 0 created entries".to_owned()]);
                } else {
                    inform(
                        &report
                            .removed
                            .iter()
                            .map(|path| {
                                format!("cleanup removed {}", render::path_value(path, true))
                            })
                            .collect::<Vec<_>>(),
                    );
                }
                // Scaffolding a pass preserved is normal housekeeping, not a finding. It appears
                // only where an operator asked for detail.
                inform(&report.describe_preserved());
            }
            if report.needs_attention() {
                // Deliberately no warning first: the same facts are about to be reported once, as
                // one structured block, by the supervision diagnostic.
                return Err(cleanup_failure_from_report(
                    &report,
                    journal_path,
                    recovery_command,
                ));
            }
            warn(&report.describe());
            Ok(())
        }
        Err(error) => Err(CleanupFailure {
            reason: error.to_string(),
            failed_paths: Vec::new(),
            retained_journal: Some(journal_path.to_path_buf()),
            recovery_command,
        }),
    }
}

/// Summarizes why cleanup could not finish, without repeating the paths the block already lists.
///
/// Identical reasons collapse: two links refused for the same cause are one fact, and each of their
/// paths still gets its own `retained path` line.
fn cleanup_failure_from_report(
    report: &CleanupReport,
    fallback_journal: &Path,
    recovery_command: Vec<OsString>,
) -> CleanupFailure {
    let mut reasons: Vec<String> = Vec::new();
    for reason in report
        .retained
        .iter()
        .map(|entry| entry.reason.clone())
        .chain(report.errors.iter().cloned())
    {
        if !reasons.contains(&reason) {
            reasons.push(reason);
        }
    }
    CleanupFailure {
        reason: reasons.join("; "),
        failed_paths: report
            .retained
            .iter()
            .map(|entry| entry.path.clone())
            .collect(),
        retained_journal: report
            .journal_retained
            .as_ref()
            .map(|retention| retention.path().to_path_buf())
            .or_else(|| Some(fallback_journal.to_path_buf())),
        recovery_command,
    }
}

fn cleanup_recovery_arguments(project_root: &Path) -> Vec<OsString> {
    vec![
        OsString::from("asm"),
        OsString::from("cleanup"),
        OsString::from("--project-root"),
        project_root.as_os_str().to_os_string(),
    ]
}

/// The precondition an operator must satisfy before any recovery vector is safe to invoke.
const RECOVERY_PRECONDITION: &str =
    "first confirm that every related Agent process has exited, then invoke this argument vector";

fn report_supervision_diagnostics(decision: &crate::process::ExitDecision) {
    if let Some(primary) = &decision.primary {
        emit_supervision_diagnostic("error", primary);
    }
    for secondary in &decision.secondary {
        emit_supervision_diagnostic("warning", secondary);
    }
}

fn emit_supervision_diagnostic(severity: &str, diagnostic: &SupervisionDiagnostic) {
    let mut stderr = io::stderr().lock();
    for line in supervision_diagnostic_lines(severity, diagnostic) {
        let _ = writeln!(stderr, "{line}");
    }
}

/// Renders one diagnostic as already-escaped lines, severity headline first.
fn supervision_diagnostic_lines(severity: &str, diagnostic: &SupervisionDiagnostic) -> Vec<String> {
    match diagnostic {
        SupervisionDiagnostic::Cleanup(failure) => cleanup_failure_lines(severity, failure),
        other => vec![format!(
            "{severity}: {}",
            render::text_value(&describe_supervision_diagnostic(other))
        )],
    }
}

/// Renders a cleanup failure as one deliberately multiline block.
///
/// Only the fixed labels and separators here introduce a newline; every external value is escaped on
/// its own, so a reason or path carrying `\n[PASS] forged` cannot manufacture a line that looks like
/// one of ours. The severity is the whole difference between replacing child success and annotating a
/// child failure, and each fact appears exactly once.
fn cleanup_failure_lines(severity: &str, failure: &CleanupFailure) -> Vec<String> {
    let mut lines = vec![format!("{severity}: session cleanup failed")];
    if !failure.reason.is_empty() {
        lines.push(format!("  reason: {}", render::text_value(&failure.reason)));
    }
    for path in &failure.failed_paths {
        lines.push(format!(
            "  retained path: {}",
            render::path_value(path, true)
        ));
    }
    match &failure.retained_journal {
        Some(path) => lines.push(format!(
            "  retained journal: {}",
            render::path_value(path, true)
        )),
        None => lines.push("  retained journal: none was available".to_owned()),
    }
    if !failure.recovery_command.is_empty() {
        lines.push(format!("  recovery: {RECOVERY_PRECONDITION}"));
        lines.extend(render::argument_vector("    ", &failure.recovery_command));
    }
    lines
}

fn describe_supervision_diagnostic(diagnostic: &SupervisionDiagnostic) -> String {
    match diagnostic {
        SupervisionDiagnostic::Process(failure) => {
            let cwd = failure
                .cwd()
                .map(|path| format!(" in {}", path.display()))
                .unwrap_or_default();
            format!(
                "agent process {:?} failed for {}{cwd}: {}",
                failure.stage(),
                failure.executable().display(),
                failure.reason()
            )
        }
        SupervisionDiagnostic::LivenessUncertain => {
            "agent process liveness could not be proved; cleanup was deferred".to_owned()
        }
        SupervisionDiagnostic::UnexpectedCleanupDeferral => {
            "cleanup was unexpectedly deferred after a terminal child outcome".to_owned()
        }
        SupervisionDiagnostic::ExceptionalWindowsStatus { raw_status } => {
            format!("agent returned exceptional Windows status 0x{raw_status:08x}")
        }
        SupervisionDiagnostic::ExceptionalUnixSignal { signal } => {
            format!("agent terminated with unrepresentable Unix signal {signal}")
        }
        SupervisionDiagnostic::ExceptionalUnixStatus { raw_status } => {
            format!("agent returned unrepresentable Unix wait status {raw_status}")
        }
        // Rendered as a structured block by `cleanup_failure_lines`, which is the only shape that
        // can carry several paths and an argument vector without repeating anything.
        SupervisionDiagnostic::Cleanup(_) => "session cleanup failed".to_owned(),
    }
}

/// Recovers or refuses, according to `--no-recover`.
///
/// A journal this build cannot interpret blocks every mutating run. Its unknown ownership record
/// may describe a path discovery cannot see or a future state this build does not understand, so
/// planning around the visible filesystem is not a safe substitute for recovery evidence.
fn reconcile_incomplete_transactions(
    context: &RunContext,
    locks: &mut HeldLocks,
) -> Result<(), AppError> {
    if context.options.no_recover {
        let blocking = recover::blocking_state(locks)?;
        if blocking.is_empty() {
            return Ok(());
        }
        return Err(AppError::TemporaryReport {
            summary: "--no-recover forbids reconciling incomplete transaction state, and nothing \
                      was changed"
                .to_owned(),
            detail: blocking,
        });
    }

    let report = recover::recover_stale(locks)?;
    if !report.unreadable.is_empty() {
        return Err(unreadable_journals_error(&report.unreadable));
    }
    if !report.quarantined.is_empty() {
        // One block per quarantined journal: the journal it belongs to, the precondition, and the
        // argument vector that releases it. Grouping keeps the pairing obvious when several journals
        // are quarantined at once, which indexed fragments could only imply.
        let mut detail = report.describe();
        detail.push(
            "state: recovered entries listed above were changed by ownership-checked recovery; the \
             quarantined mounts were not changed and remain journal-backed"
                .to_owned(),
        );
        detail.push(
            "safe next action: release each quarantined journal below, or account for every \
             recorded path manually"
                .to_owned(),
        );
        for quarantined in &report.quarantined {
            detail.push(format!(
                "  quarantined journal: {}",
                render::path_value(&quarantined.journal, true)
            ));
            detail.push(format!("  recovery: {RECOVERY_PRECONDITION}"));
            detail.extend(render::argument_vector(
                "    ",
                &cleanup_recovery_arguments(&quarantined.project_root),
            ));
        }
        return Err(AppError::TemporaryReport {
            summary:
                "cannot start a mutating session because process-domain death was never proved"
                    .to_owned(),
            detail,
        });
    }
    warn(&report.describe());
    Ok(())
}

/// Builds the fail-closed operator diagnostic for journals this build cannot account for.
fn unreadable_journals_error(rejected: &[RejectedJournal]) -> AppError {
    let mut detail = rejected
        .iter()
        .map(|rejected| {
            format!(
                "transaction journal {} cannot be interpreted: {}",
                render::path_value(&rejected.path, true),
                render::text_value(&rejected.reason)
            )
        })
        .collect::<Vec<_>>();
    detail.push(
        "account for every recorded path that may belong to each reported journal before moving or \
         removing any remaining state, then retry"
            .to_owned(),
    );
    AppError::TemporaryReport {
        summary:
            "cannot start a mutating session while transaction state is unreadable or uses an \
                  unsupported schema; no cleanup was attempted and no new plan was applied"
                .to_owned(),
        detail,
    }
}

/// Extends an accumulated lock-resource description without duplicating identical observations.
fn extend_resources(
    resources: &mut Vec<crate::lock::LockResource>,
    additional: &[crate::lock::LockResource],
) {
    for resource in additional {
        if !resources.contains(resource) {
            resources.push(resource.clone());
        }
    }
}

/// Runs the complete read-only pipeline: catalog, discovery inspection, preliminary plan.
///
/// Nothing in this function creates a directory, link, lock, journal, or child process. Both the
/// `inspect` command and `--dry-run` stop here. A normal session calls the same pure pipeline under
/// its locks after recovery and before applying any planned destination mutation.
pub(crate) fn plan_read_only(context: &RunContext) -> Result<ReadOnlyOutcome, AppError> {
    build_read_only(context, true)
}

/// Builds an inspection report without certifying a child command that will never be launched.
fn plan_inspection(context: &RunContext) -> Result<ReadOnlyOutcome, AppError> {
    build_read_only(context, false)
}

fn build_read_only(
    context: &RunContext,
    validate_launch_command: bool,
) -> Result<ReadOnlyOutcome, AppError> {
    let adapter = adapter(context.agent_id());
    if validate_launch_command {
        adapter.validate_passthrough_args(&context.passthrough_args)?;
        // A dry run describes the session the mutating run would start. An invariant that would
        // refuse that session - a launch CWD the Agent relocates away from, configuration whose
        // effective values are in no file this release can read - therefore has to refuse the
        // description too, or `--dry-run` prints a plan for a namespace no child would ever load.
        adapter.validate_launch_invariants(context)?;
    }

    let destination_stores = adapter.destination_stores(context);
    let catalog = resolve_catalog(
        &context.skill_sources,
        &CatalogRequest {
            agent: context.agent_id(),
            policy: adapter.catalog_policy(),
            validation: context.options.validation,
            destination_stores: &destination_stores,
        },
    )?;
    let mut snapshot = adapter.inspect_discovery(context)?;
    let plan = adapter.build_mount_plan(context, &catalog, &snapshot)?;
    snapshot
        .warnings
        .extend(adapter.catalog_diagnostics(context, &catalog, &plan));
    Ok(ReadOnlyOutcome {
        catalog,
        snapshot,
        plan,
    })
}

/// Expands one `--agent` selection into registered Agents, in the single deterministic order.
fn inspected_agents(selection: InspectAgent) -> Vec<AgentId> {
    match selection {
        InspectAgent::Codex => vec![AgentId::Codex],
        InspectAgent::Claude => vec![AgentId::Claude],
        InspectAgent::Omp => vec![AgentId::Omp],
        InspectAgent::All => AgentId::ALL.to_vec(),
    }
}

/// Writes report text to stdout, treating a closed pipe as an ordinary end of output.
fn emit(text: &str) -> Result<(), AppError> {
    match io::stdout().lock().write_all(text.as_bytes()) {
        Ok(()) => Ok(()),
        Err(error) if error.kind() == io::ErrorKind::BrokenPipe => Ok(()),
        Err(error) => Err(AppError::Internal(format!(
            "failed to write output: {error}"
        ))),
    }
}

/// Writes wrapper-owned session diagnostics to stderr, preserving child stdout as a data stream.
fn emit_diagnostic(text: &str) -> Result<(), AppError> {
    match io::stderr().lock().write_all(text.as_bytes()) {
        Ok(()) => Ok(()),
        Err(error) if error.kind() == io::ErrorKind::BrokenPipe => Ok(()),
        Err(error) => Err(AppError::Internal(format!(
            "cannot write session diagnostics to standard error: {error}"
        ))),
    }
}

fn warn(messages: &[String]) {
    for message in messages {
        let _ = writeln!(
            io::stderr().lock(),
            "warning: {}",
            render::text_value(message)
        );
    }
}

fn inform(messages: &[String]) {
    for message in messages {
        let _ = writeln!(io::stderr().lock(), "info: {}", render::text_value(message));
    }
}

fn report_clap_error(error: &clap::Error, original_kind: ErrorKind) -> ExitCode {
    let success = matches!(
        original_kind,
        ErrorKind::DisplayHelp
            | ErrorKind::DisplayVersion
            | ErrorKind::DisplayHelpOnMissingArgumentOrSubcommand
    );
    if let Err(write_error) = error.print() {
        if success && write_error.kind() == io::ErrorKind::BrokenPipe {
            return ExitCode::SUCCESS;
        }
        return ExitCode::from(ExitCategory::Internal.code());
    }
    if success {
        ExitCode::SUCCESS
    } else {
        ExitCode::from(ExitCategory::Usage.code())
    }
}

fn report_clap_fallback(original_kind: ErrorKind) -> ExitCode {
    let success = matches!(
        original_kind,
        ErrorKind::DisplayHelp
            | ErrorKind::DisplayVersion
            | ErrorKind::DisplayHelpOnMissingArgumentOrSubcommand
    );
    let message = if success {
        "command help could not be rendered safely"
    } else {
        "invalid command-line arguments could not be rendered safely"
    };
    let stream = if success {
        &mut io::stdout().lock() as &mut dyn Write
    } else {
        &mut io::stderr().lock() as &mut dyn Write
    };
    if writeln!(stream, "{message}").is_err() {
        return ExitCode::from(ExitCategory::Internal.code());
    }
    if success {
        ExitCode::SUCCESS
    } else {
        ExitCode::from(ExitCategory::Usage.code())
    }
}

fn report_error(error: &AppError) -> ExitCode {
    let mut stderr = io::stderr().lock();
    let _ = writeln!(stderr, "error: {}", render::text_value(&error.to_string()));
    if let AppError::TemporaryReport { detail, .. } = error {
        // Written verbatim: each line was built here from independently escaped values, so escaping
        // the block again would only hide the structure it exists to provide.
        for line in detail {
            let _ = writeln!(stderr, "{line}");
        }
    }
    for line in error_guidance(error) {
        let _ = writeln!(stderr, "{line}");
    }
    ExitCode::from(error.category().code())
}

/// Adds stable state and recovery guidance without weakening the typed primary error.
fn error_guidance(error: &AppError) -> Vec<&'static str> {
    match error {
        AppError::Catalog(CatalogError::InvalidSelectedSkill { .. }) => vec![
            "state: no selected Skill was mounted and no existing destination was replaced; any earlier stale recovery was reported separately",
            "safe next action: fix or remove the rightmost selected winner, or change the --skills-dir order; SkillMount does not fall back to a shadowed candidate",
        ],
        AppError::Catalog(_) => vec![
            "state: no selected Skill was mounted and no existing destination was replaced; any earlier stale recovery was reported separately",
            "safe next action: correct the named catalog entry or source ordering, then retry",
        ],
        AppError::Plan(PlanError::DestinationConflict { .. }) => vec![
            "state: the conflicting destination was not replaced; any earlier stale recovery was reported separately",
            "safe next action: --conflict=skip may preserve an ordinary project directory or different-source directory link while omitting this selected Skill; otherwise account for and repair the existing entry before retrying",
        ],
        AppError::Plan(_) => vec![
            "state: the reported destination was not replaced; an ownership-checked rollback completed if mutation had already begun",
            "safe next action: inspect and account for the reported discovery entry before repairing it and retrying",
        ],
        AppError::Link(
            LinkError::Create { .. } | LinkError::SymlinkPrivilegeUnavailable { .. },
        ) => vec![
            "state: the final destination was not replaced; the attempt may have written private transaction state or retained an unverified staged path for recovery",
            "safe next action: run asm doctor; on Windows, use --link-mode=junction only with passing compatibility evidence for this agent/version, or make symbolic links available without elevation",
        ],
        AppError::Link(_) => vec![
            "state: no unowned entry was intentionally removed or replaced; any uncertain path remains retained for ownership-safe recovery",
            "safe next action: run asm doctor, inspect every named retained path, and use asm cleanup only after proving no related process is active",
        ],
        AppError::Journal(_) => vec![
            "state: the journal and every path whose ownership is uncertain were retained",
            "safe next action: run asm doctor and account for every recorded path; do not edit or delete journal state merely to bypass the failure",
        ],
        AppError::MissingInput { .. } => vec![
            "state: the agent was not launched; if an earlier diagnostic names a retained path or journal, that mount may still exist",
            "safe next action: restore or correct the named input path, then retry",
        ],
        AppError::Filesystem(_) => vec![
            "safe next action: inspect every retained path and journal named above with asm doctor; use asm cleanup only after proving no related process is active",
        ],
        AppError::Temporary(_) | AppError::TemporaryReport { .. } => vec![
            "safe next action: run asm doctor and wait for any active session to exit; never delete a lock file or trust holder text as liveness proof, and use asm cleanup only after proving process-domain death",
        ],
        AppError::Usage(_) | AppError::Internal(_) | AppError::Interrupted => Vec::new(),
    }
}

#[cfg(test)]
mod tests {
    use std::ffi::OsString;
    use std::io::{self, Write};
    use std::path::PathBuf;

    use super::{
        error_guidance, execute_completion, junction_policy_warning, supervision_diagnostic_lines,
    };
    use crate::cli::{CompletionInput, CompletionShell, ProductBinary};
    use crate::domain::{AgentId, LinkMode};
    use crate::error::{AppError, ExitCategory, LinkError};
    use crate::process::{CleanupFailure, SupervisionDiagnostic};

    struct ErrorWriter {
        kind: io::ErrorKind,
    }

    impl Write for ErrorWriter {
        fn write(&mut self, _buffer: &[u8]) -> io::Result<usize> {
            Err(io::Error::new(self.kind, "injected completion failure"))
        }

        fn flush(&mut self) -> io::Result<()> {
            Ok(())
        }
    }

    const fn completion_input() -> CompletionInput {
        CompletionInput {
            shell: CompletionShell::Bash,
            product: ProductBinary::Asm,
        }
    }

    #[test]
    fn broken_pipe_during_completion_is_success() {
        let mut writer = ErrorWriter {
            kind: io::ErrorKind::BrokenPipe,
        };

        let code =
            execute_completion(completion_input(), &mut writer).expect("broken pipe is successful");

        assert_eq!(code, 0);
    }

    #[test]
    fn non_broken_pipe_completion_failure_uses_internal_category() {
        let mut writer = ErrorWriter {
            kind: io::ErrorKind::PermissionDenied,
        };

        let error = execute_completion(completion_input(), &mut writer)
            .expect_err("other write failures should fail");

        assert_eq!(error.category(), ExitCategory::Internal);
        assert!(
            error
                .to_string()
                .contains("failed to write completion output")
        );
    }

    #[test]
    fn only_automatic_junction_fallback_emits_the_unverified_compatibility_warning() {
        let warning = junction_policy_warning(AgentId::Codex, LinkMode::Auto, true)
            .expect("automatic junction fallback must be visible");
        assert!(warning.contains("0.146.0"));
        assert!(warning.contains("unverified"));
        assert!(warning.contains("--link-mode=symlink"));

        assert!(junction_policy_warning(AgentId::Codex, LinkMode::Auto, false).is_none());
        assert!(junction_policy_warning(AgentId::Claude, LinkMode::Junction, true).is_none());
        assert!(junction_policy_warning(AgentId::Claude, LinkMode::Symlink, false).is_none());
    }

    #[test]
    fn link_creation_guidance_names_diagnosis_and_never_suggests_elevation() {
        let error = AppError::Link(LinkError::Create {
            destination: PathBuf::from("destination"),
            source: PathBuf::from("source"),
            reason: "symbolic-link privilege is unavailable".to_owned(),
        });

        let guidance = error_guidance(&error).join("\n");

        assert!(guidance.contains("asm doctor"));
        assert!(guidance.contains("--link-mode=junction"));
        assert!(guidance.contains("without elevation"));
        assert!(!guidance.contains("sudo"));
        assert!(!guidance.contains("runas"));
    }

    #[test]
    fn missing_input_guidance_never_denies_a_preceding_cleanup_failure() {
        let error = AppError::MissingInput {
            path: PathBuf::from("agent"),
            reason: "disappeared before spawn".to_owned(),
        };

        let guidance = error_guidance(&error).join("\n");

        assert!(guidance.contains("the agent was not launched"));
        assert!(guidance.contains("retained path or journal"));
        assert!(!guidance.contains("no selected Skill was mounted"));
    }

    /// One structured block per condition, with every fact exactly once and no shell syntax.
    #[test]
    fn a_cleanup_failure_renders_one_labelled_block_per_condition() {
        let diagnostic = SupervisionDiagnostic::Cleanup(CleanupFailure {
            reason: "the entry could not be proved to belong to this session".to_owned(),
            failed_paths: vec![
                PathBuf::from("/project/.agents/skills/alpha"),
                PathBuf::from("/project/.agents/skills/beta"),
            ],
            retained_journal: Some(PathBuf::from("/state/transactions/one.journal")),
            recovery_command: vec![
                OsString::from("asm"),
                OsString::from("cleanup"),
                OsString::from("--project-root"),
                OsString::from("/project"),
            ],
        });

        let lines = supervision_diagnostic_lines("error", &diagnostic);
        let rendered = lines.join("\n");

        assert_eq!(lines[0], "error: session cleanup failed", "{rendered}");
        assert_eq!(
            lines
                .iter()
                .filter(|line| line.starts_with("  retained path: "))
                .count(),
            2,
            "each retained path gets its own line: {rendered}"
        );
        assert_eq!(
            lines
                .iter()
                .filter(|line| line.starts_with("  reason: "))
                .count(),
            1,
            "the reason is stated once: {rendered}"
        );
        assert!(
            lines.contains(&"  retained journal: /state/transactions/one.journal".to_owned()),
            "{rendered}"
        );
        assert!(
            lines.contains(&"    executable: asm".to_owned())
                && lines.contains(&"    argument 1: cleanup".to_owned())
                && lines.contains(&"    argument 2: --project-root".to_owned())
                && lines.contains(&"    argument 3: /project".to_owned()),
            "{rendered}"
        );
        assert!(
            !rendered.contains("argv["),
            "raw argv fragments are gone: {rendered}"
        );
        assert!(
            !rendered.contains("asm cleanup --project-root"),
            "recovery must never look like a pasteable command: {rendered}"
        );
    }

    /// A child failure keeps its own code; only the severity word changes.
    #[test]
    fn a_secondary_cleanup_failure_differs_only_in_its_severity_word() {
        let failure = CleanupFailure {
            reason: "removal was refused".to_owned(),
            failed_paths: vec![PathBuf::from("/project/.agents/skills/alpha")],
            retained_journal: Some(PathBuf::from("/state/transactions/one.journal")),
            recovery_command: vec![OsString::from("asm"), OsString::from("cleanup")],
        };
        let diagnostic = SupervisionDiagnostic::Cleanup(failure);

        let primary = supervision_diagnostic_lines("error", &diagnostic);
        let secondary = supervision_diagnostic_lines("warning", &diagnostic);

        assert_eq!(primary[0], "error: session cleanup failed");
        assert_eq!(secondary[0], "warning: session cleanup failed");
        assert_eq!(primary[1..], secondary[1..]);
    }

    #[test]
    fn a_cleanup_failure_without_a_journal_path_says_so_once() {
        let diagnostic = SupervisionDiagnostic::Cleanup(CleanupFailure {
            reason: "the journal transition could not be persisted".to_owned(),
            failed_paths: Vec::new(),
            retained_journal: None,
            recovery_command: Vec::new(),
        });

        let lines = supervision_diagnostic_lines("error", &diagnostic);

        assert_eq!(
            lines,
            [
                "error: session cleanup failed".to_owned(),
                "  reason: the journal transition could not be persisted".to_owned(),
                "  retained journal: none was available".to_owned(),
            ]
        );
    }

    #[test]
    fn supervision_diagnostics_escape_line_and_terminal_controls() {
        let diagnostic = SupervisionDiagnostic::Cleanup(CleanupFailure {
            reason: "cleanup failed\n[PASS] forged\u{1B}]52;clipboard\u{7}".to_owned(),
            failed_paths: vec![PathBuf::from("mount\n[PASS] path")],
            retained_journal: Some(PathBuf::from("journal\u{202E}txt")),
            recovery_command: vec![
                OsString::from("asm"),
                OsString::from("cleanup\n[PASS] argv"),
            ],
        });

        let lines = supervision_diagnostic_lines("error", &diagnostic);
        let rendered = lines.join("\n");

        assert!(rendered.contains("\\u{A}"), "{rendered}");
        assert!(rendered.contains("\\u{1B}"), "{rendered}");
        assert!(rendered.contains("\\u{7}"), "{rendered}");
        assert!(rendered.contains("\\u{202E}"), "{rendered}");
        assert!(!rendered.contains('\u{1B}'), "{rendered}");
        assert!(!rendered.contains('\u{202E}'), "{rendered}");
        // Every line the block emits is one this code wrote. A forged value can no longer contribute
        // a line of its own, even though the block itself is intentionally multiline.
        assert_eq!(
            lines.len(),
            7,
            "only fixed separators may create lines: {rendered}"
        );
        for line in &lines {
            assert!(!line.contains('\n'), "{line}");
        }
        assert!(
            !lines.iter().any(|line| line.starts_with("[PASS]")),
            "{rendered}"
        );
    }

    /// A non-Unicode recovery argument stays reversible instead of lossy.
    #[cfg(unix)]
    #[test]
    fn a_non_unicode_recovery_argument_is_escaped_reversibly() {
        use std::os::unix::ffi::OsStringExt as _;

        let diagnostic = SupervisionDiagnostic::Cleanup(CleanupFailure {
            reason: "removal was refused".to_owned(),
            failed_paths: Vec::new(),
            retained_journal: None,
            recovery_command: vec![
                OsString::from("asm"),
                OsString::from_vec(b"/pro\xffject".to_vec()),
            ],
        });

        let rendered = supervision_diagnostic_lines("error", &diagnostic).join("\n");

        assert!(
            rendered.contains("argument 1: escaped:/pro\\xFFject"),
            "{rendered}"
        );
    }

    /// A Windows argument holding an unpaired surrogate stays reversible instead of lossy.
    #[cfg(windows)]
    #[test]
    fn an_unpaired_utf16_recovery_argument_is_escaped_reversibly() {
        use std::os::windows::ffi::OsStringExt as _;

        let diagnostic = SupervisionDiagnostic::Cleanup(CleanupFailure {
            reason: "removal was refused".to_owned(),
            failed_paths: Vec::new(),
            retained_journal: None,
            recovery_command: vec![
                OsString::from("asm"),
                OsString::from_wide(&[0x0043, 0x003A, 0x005C, 0xD800, 0x0070]),
            ],
        });

        let rendered = supervision_diagnostic_lines("error", &diagnostic).join("\n");

        assert!(rendered.contains("argument 1: escaped:"), "{rendered}");
        assert!(rendered.contains("\\uD800"), "{rendered}");
    }
}
