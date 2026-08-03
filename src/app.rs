//! Shared application orchestration for read-only and mutating commands.

use std::ffi::OsString;
use std::io::{self, Write};
use std::path::{Path, PathBuf};
use std::process::ExitCode;

use clap::error::ErrorKind;

use crate::agent::claude::ClaudeAdapter;
use crate::agent::codex::CodexAdapter;
use crate::agent::{AgentAdapter, DiscoverySnapshot};
use crate::catalog::{CatalogRequest, resolve_catalog};
use crate::cli::{InspectAgent, ParsedCommand, ReservedUtility, parse_command_from};
use crate::domain::{AgentId, MountMode, RunContext, SkillCatalog};
use crate::error::{AppError, ExitCategory};
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
    let invocation_cwd = match std::env::current_dir() {
        Ok(path) => path,
        Err(error) => {
            return report_error(&AppError::Internal(format!(
                "cannot capture invocation CWD: {error}"
            )));
        }
    };

    let command = match parse_command_from(args) {
        Ok(command) => command,
        Err(error) => return report_clap_error(&error),
    };
    match execute(command, &invocation_cwd) {
        Ok(code) => ExitCode::from(code),
        Err(error) => report_error(&error),
    }
}

fn execute(command: ParsedCommand, invocation_cwd: &Path) -> Result<u8, AppError> {
    match command {
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
            for agent in inspected_agents(input.agent) {
                let context = resolve_inspection(
                    agent,
                    &input.skills_dirs,
                    input.validation,
                    invocation_cwd,
                )?;
                let report = plan_inspection(&context)?;
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
            emit(&rendered)?;
            warn(&warnings);
            Ok(0)
        }
        ParsedCommand::Reserved(utility) => {
            let name = match utility {
                ReservedUtility::Doctor => "doctor",
                ReservedUtility::Cleanup => "cleanup",
            };
            Err(AppError::Internal(format!(
                "{name} is reserved for a later change and is not implemented"
            )))
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
    // Root-changing arguments and an incompatible Codex binary are rejected before reading or
    // creating SkillMount state. The later locked replan repeats the pure argument check, but it
    // must not be the first time a mutating invocation learns that its launch contract is unsafe.
    adapter_for(context.agent).validate_passthrough_args(&context.passthrough_args)?;
    match context.agent {
        AgentId::Codex => crate::agent::codex::verify_supported_launch(context)?,
        AgentId::Claude => crate::agent::claude::verify_supported_launch(context)?,
    }

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

    let preliminary = adapter_for(context.agent).inspect_discovery(&context)?;
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

    // Lock acquisition may wait behind a long-running session while the installed agent is
    // upgraded. Re-probe after the lock set stabilizes so a plan is never persisted or applied
    // using only compatibility evidence captured before that wait.
    match context.agent {
        AgentId::Codex => crate::agent::codex::verify_supported_launch(&context)?,
        AgentId::Claude => crate::agent::claude::verify_supported_launch(&context)?,
    }

    warn(&render::render_warnings(
        &rebuilt.catalog,
        &rebuilt.snapshot,
    ));

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

    // Apply can itself take time and runs after the last version probe. Check once more at the
    // child boundary. If an updater replaced the agent, no child is spawned and the active
    // transaction is released through the normal evidence-checked cleanup path.
    verify_spawn_boundary(&context, &rebuilt.catalog, &mut transaction)?;

    if let Err(error) = transaction.begin_supervision() {
        match transaction.cleanup_required() {
            Ok(report) => warn(&report.describe()),
            Err(cleanup_error) => warn(&[format!(
                "recording agent supervision intent failed and cleanup also failed: {cleanup_error}"
            )]),
        }
        return Err(error);
    }
    let journal_path = transaction.journal_path().to_path_buf();
    let request = SupervisionRequest::new(rebuilt.plan.launch.clone());
    let outcome = ProcessSupervisor::new().supervise(request, move || {
        cleanup_for_supervisor(&mut transaction, &journal_path)
    });
    // Lock ownership covers the whole child lifetime and the cleanup callback. Keeping this
    // explicit prevents a future refactor from shortening the guard to the last planning use.
    drop(locks);

    let decision = map_exit(&outcome);
    report_supervision_diagnostics(&decision);
    Ok(decision.code)
}

fn verify_spawn_boundary(
    context: &RunContext,
    catalog: &SkillCatalog,
    transaction: &mut Transaction,
) -> Result<(), AppError> {
    let compatibility = match context.agent {
        AgentId::Codex => crate::agent::codex::verify_supported_launch(context)
            .and_then(|()| crate::agent::codex::verify_selected_plugin_namespaces(catalog)),
        AgentId::Claude => crate::agent::claude::verify_supported_launch(context),
    };
    if let Err(error) = compatibility {
        match transaction.cleanup_required() {
            Ok(report) => warn(&report.describe()),
            Err(cleanup_error) => warn(&[format!(
                "agent compatibility changed before launch and cleanup also failed: {cleanup_error}"
            )]),
        }
        return Err(error);
    }
    Ok(())
}

fn cleanup_for_supervisor(
    transaction: &mut Transaction,
    journal_path: &Path,
) -> Result<(), CleanupFailure> {
    match transaction.cleanup() {
        Ok(report) => {
            let messages = report.describe();
            warn(&messages);
            if report.needs_attention() {
                Err(cleanup_failure_from_report(
                    &report,
                    &messages,
                    journal_path,
                ))
            } else {
                Ok(())
            }
        }
        Err(error) => Err(CleanupFailure {
            reason: error.to_string(),
            failed_paths: Vec::new(),
            retained_journal: Some(journal_path.to_path_buf()),
            recovery_command: Vec::new(),
        }),
    }
}

fn cleanup_failure_from_report(
    report: &CleanupReport,
    messages: &[String],
    fallback_journal: &Path,
) -> CleanupFailure {
    CleanupFailure {
        reason: messages.join("; "),
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
        recovery_command: Vec::new(),
    }
}

fn report_supervision_diagnostics(decision: &crate::process::ExitDecision) {
    if let Some(primary) = &decision.primary {
        let _ = writeln!(
            io::stderr().lock(),
            "error: {}",
            describe_supervision_diagnostic(primary)
        );
    }
    for secondary in &decision.secondary {
        let _ = writeln!(
            io::stderr().lock(),
            "warning: {}",
            describe_supervision_diagnostic(secondary)
        );
    }
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
        SupervisionDiagnostic::Cleanup(failure) => {
            let journal = failure.retained_journal.as_ref().map_or_else(
                || "no journal path was available".to_owned(),
                |path| format!("journal retained at {}", path.display()),
            );
            format!("session cleanup failed: {}; {journal}", failure.reason)
        }
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
        return Err(AppError::Temporary(format!(
            "--no-recover forbids reconciling incomplete transaction state, and nothing was \
             changed:\n{}",
            blocking.join("\n")
        )));
    }

    let report = recover::recover_stale(locks)?;
    if !report.unreadable.is_empty() {
        return Err(unreadable_journals_error(&report.unreadable));
    }
    if !report.quarantined.is_empty() {
        return Err(AppError::Temporary(format!(
            "cannot start a mutating session because process-domain death was never proved for:\n{}\nthese journals and their mounts were retained; verify that every related process has exited, then use the future explicit cleanup command or account for the recorded paths manually",
            report
                .quarantined
                .iter()
                .map(|path| format!("transaction journal {}", path.display()))
                .collect::<Vec<_>>()
                .join("\n")
        )));
    }
    warn(&report.describe());
    Ok(())
}

/// Builds the fail-closed operator diagnostic for journals this build cannot account for.
fn unreadable_journals_error(rejected: &[RejectedJournal]) -> AppError {
    AppError::Temporary(format!(
        "cannot start a mutating session while transaction state is unreadable or uses an \
         unsupported schema; every journal was retained and no new plan was applied:\n{}\n\
         inspect these files and account for every recorded path before moving or removing them, \
         then retry",
        rejected
            .iter()
            .map(|rejected| {
                format!(
                    "transaction journal {} cannot be interpreted: {}",
                    rejected.path.display(),
                    rejected.reason
                )
            })
            .collect::<Vec<_>>()
            .join("\n")
    ))
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
    let adapter = adapter_for(context.agent);
    if validate_launch_command {
        adapter.validate_passthrough_args(&context.passthrough_args)?;
    }

    let destination_stores = destination_stores(context);
    let catalog = resolve_catalog(
        &context.skill_sources,
        &CatalogRequest {
            agent: context.agent,
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

fn adapter_for(agent: AgentId) -> Box<dyn AgentAdapter> {
    match agent {
        AgentId::Codex => Box::new(CodexAdapter),
        AgentId::Claude => Box::new(ClaudeAdapter),
    }
}

fn inspected_agents(selection: InspectAgent) -> Vec<AgentId> {
    match selection {
        InspectAgent::Codex => vec![AgentId::Codex],
        InspectAgent::Claude => vec![AgentId::Claude],
        InspectAgent::All => vec![AgentId::Codex, AgentId::Claude],
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

fn warn(messages: &[String]) {
    for message in messages {
        let _ = writeln!(io::stderr().lock(), "warning: {message}");
    }
}

fn destination_stores(context: &crate::domain::RunContext) -> Vec<PathBuf> {
    match (context.agent, context.options.mount_mode) {
        (AgentId::Codex, _) => vec![context.project_root.join(".agents/skills")],
        (AgentId::Claude, MountMode::Project) => {
            vec![context.project_root.join(".claude/skills")]
        }
        (AgentId::Claude, MountMode::Staging) => Vec::new(),
    }
}

fn report_clap_error(error: &clap::Error) -> ExitCode {
    let success = matches!(
        error.kind(),
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

fn report_error(error: &AppError) -> ExitCode {
    let _ = writeln!(io::stderr().lock(), "error: {error}");
    ExitCode::from(error.category().code())
}
