//! Shared read-only application boundary.

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
use crate::mount::MountPlan;
use crate::paths::{resolve_inspection, resolve_session};
use crate::render;

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
        Ok(()) => ExitCode::SUCCESS,
        Err(error) => report_error(&error),
    }
}

fn execute(command: ParsedCommand, invocation_cwd: &Path) -> Result<(), AppError> {
    match command {
        ParsedCommand::Session(input) => {
            let dry_run = input.options.dry_run;
            let context = resolve_session(input, invocation_cwd)?;
            let report = plan_read_only(&context)?;

            if !dry_run {
                return Err(AppError::Internal(format!(
                    "planned {} action(s) for {} Skill(s), but applying a plan, acquiring locks, and launching the agent are reserved for later changes",
                    report.plan.actions.len(),
                    report.catalog.resolutions.len()
                )));
            }
            emit(&render::render(&render::ReadOnlyReport {
                context: &context,
                catalog: &report.catalog,
                snapshot: &report.snapshot,
                plan: &report.plan,
                verbosity: context.options.verbosity,
            }))?;
            warn(&render::render_warnings(&report.catalog, &report.snapshot));
            Ok(())
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
                let report = plan_read_only(&context)?;
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
            Ok(())
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
struct ReadOnlyOutcome {
    catalog: SkillCatalog,
    snapshot: DiscoverySnapshot,
    plan: MountPlan,
}

/// Runs the complete read-only pipeline: catalog, discovery inspection, preliminary plan.
///
/// Nothing in this function creates a directory, link, lock, journal, or child process. Both the
/// `inspect` command and `--dry-run` stop here, and a normal session reuses the same result before
/// it reaches the mutation boundary.
fn plan_read_only(context: &RunContext) -> Result<ReadOnlyOutcome, AppError> {
    let adapter = adapter_for(context.agent);
    adapter.validate_passthrough_args(&context.passthrough_args)?;

    let destination_stores = destination_stores(context);
    let catalog = resolve_catalog(
        &context.skill_sources,
        &CatalogRequest {
            agent: context.agent,
            validation: context.options.validation,
            destination_stores: &destination_stores,
        },
    )?;
    let snapshot = adapter.inspect_discovery(context)?;
    let plan = adapter.build_mount_plan(context, &catalog, &snapshot)?;
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
        (AgentId::Codex, _) => vec![
            context.project_root.join(".agents/skills"),
            context.project_root.join(".codex/skills"),
        ],
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
