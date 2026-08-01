//! Shared read-only application boundary.

use std::ffi::OsString;
use std::fmt::Write as _;
use std::io::{self, Write};
use std::path::{Path, PathBuf};
use std::process::ExitCode;

use clap::error::ErrorKind;

use crate::catalog::{CatalogRequest, resolve_catalog};
use crate::cli::{InspectAgent, ParsedCommand, ReservedUtility, parse_command_from};
use crate::domain::{AgentId, MountMode};
use crate::error::{AppError, ExitCategory};
use crate::paths::{resolve_session, resolve_source_occurrences};

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
            let context = resolve_session(input, invocation_cwd)?;
            let destination_stores = destination_stores(&context);
            let request = CatalogRequest {
                agent: context.agent,
                validation: context.options.validation,
                destination_stores: &destination_stores,
            };
            let catalog = resolve_catalog(&context.skill_sources, &request)?;
            Err(AppError::Internal(format!(
                "catalog resolved {} Skill(s), but mount planning and agent launch are reserved for later changes",
                catalog.resolutions.len()
            )))
        }
        ParsedCommand::Inspect(input) => {
            let invocation_cwd =
                std::fs::canonicalize(invocation_cwd).map_err(|error| AppError::MissingInput {
                    path: invocation_cwd.to_path_buf(),
                    reason: error.to_string(),
                })?;
            let occurrences = resolve_source_occurrences(&input.skills_dirs, &invocation_cwd)?;
            let agent = match input.agent {
                InspectAgent::Codex | InspectAgent::All => AgentId::Codex,
                InspectAgent::Claude => AgentId::Claude,
            };
            let catalog = resolve_catalog(
                &occurrences,
                &CatalogRequest {
                    agent,
                    validation: input.validation,
                    destination_stores: &[],
                },
            )?;
            let mut rendered = String::new();
            writeln!(
                rendered,
                "Resolved {} Skill(s); {} logical override(s).",
                catalog.resolutions.len(),
                catalog.override_count()
            )
            .expect("writing to a String cannot fail");
            for resolution in catalog.resolutions {
                writeln!(
                    rendered,
                    "  {} <- --skills-dir #{} ({})",
                    resolution.selected.mount_name,
                    resolution.selected.origin.source_ordinal + 1,
                    resolution.selected.origin.source_entry.display()
                )
                .expect("writing to a String cannot fail");
            }
            if let Err(error) = io::stdout().lock().write_all(rendered.as_bytes()) {
                if error.kind() != io::ErrorKind::BrokenPipe {
                    return Err(AppError::Internal(format!(
                        "failed to write output: {error}"
                    )));
                }
            }
            for warning in catalog.warnings {
                let _ = writeln!(io::stderr().lock(), "warning: {}", warning.message);
            }
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
