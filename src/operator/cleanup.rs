//! Explicit transaction cleanup through the shared lock and ownership engine.

use std::ffi::OsString;
use std::fmt::Write as _;
use std::path::Path;

use crate::cli::CleanupInput;
use crate::error::{AppError, ExitCategory};
use crate::paths::resolve_operator_project_root;
use crate::render::{os_value, path_value};
use crate::transaction::recover::{ExplicitCleanupReport, cleanup_explicit};

use super::CommandOutcome;

/// Runs scoped or all-journal explicit cleanup and renders every decision.
pub(crate) fn run(input: &CleanupInput, invocation_cwd: &Path) -> Result<CommandOutcome, AppError> {
    let project_root = if input.all {
        None
    } else {
        Some(resolve_operator_project_root(
            input.project_root.as_deref(),
            invocation_cwd,
        )?)
    };
    let report = cleanup_explicit(project_root.as_deref())?;
    let output = render_report(project_root.as_deref(), &report);
    let code = exit_code(&report);
    Ok(CommandOutcome { output, code })
}

fn exit_code(report: &ExplicitCleanupReport) -> u8 {
    if report
        .reconciled
        .iter()
        .any(|entry| entry.report.needs_attention())
        || report
            .failures
            .iter()
            .any(|failure| failure.error.category() == ExitCategory::Filesystem)
    {
        return ExitCategory::Filesystem.code();
    }
    if !report.active.is_empty() || !report.unreadable.is_empty() {
        return ExitCategory::Temporary.code();
    }
    report
        .failures
        .first()
        .map_or(0, |failure| failure.error.category().code())
}

fn render_report(project_root: Option<&Path>, report: &ExplicitCleanupReport) -> String {
    let mut output = String::new();
    let _ = writeln!(output, "SkillMount cleanup");
    match project_root {
        Some(project_root) => {
            let _ = writeln!(
                output,
                "Scope: project {}\n",
                path_value(project_root, true)
            );
        }
        None => {
            let _ = writeln!(
                output,
                "Scope: every validated SkillMount journal (--all)\n"
            );
        }
    }

    render_cleanup_outcomes(&mut output, report);

    let attention = report
        .reconciled
        .iter()
        .any(|entry| entry.report.needs_attention())
        || !report.active.is_empty()
        || !report.unreadable.is_empty()
        || !report.failures.is_empty();
    let _ = writeln!(
        output,
        "\nSummary: {} recovered, {} active, {} corrupt, {} failed, {} completed, {} out of scope",
        report.reconciled.len(),
        report.active.len(),
        report.unreadable.len(),
        report.failures.len(),
        report.completed.len(),
        report.out_of_scope
    );
    if attention {
        let _ = writeln!(
            output,
            "No unproven entry was removed. After resolving the reported condition, retry these argv values:"
        );
        render_retry_argv(&mut output, project_root);
    }
    output
}

fn render_cleanup_outcomes(output: &mut String, report: &ExplicitCleanupReport) {
    if report.unreadable.is_empty() {
        for entry in &report.reconciled {
            let count = entry.report.removed.len();
            let noun = if count == 1 { "entry" } else { "entries" };
            let _ = writeln!(
                output,
                "[RECOVERED] transaction {} from {}: {count} {noun} removed",
                entry.transaction,
                path_value(&entry.journal, true)
            );
            for removed in &entry.report.removed {
                let _ = writeln!(output, "  removed {}", path_value(removed, true));
            }
            for retained in &entry.report.retained {
                let _ = writeln!(
                    output,
                    "  retained {}: {}",
                    path_value(&retained.path, true),
                    retained.reason
                );
            }
            for error in &entry.report.errors {
                let _ = writeln!(output, "  cleanup error: {error}");
            }
            if let Some(retention) = &entry.report.journal_retained {
                let _ = writeln!(
                    output,
                    "  journal retained at {}",
                    path_value(retention.path(), true)
                );
            }
        }
        for active in &report.active {
            let _ = writeln!(
                output,
                "[ACTIVE] transaction {} at {}: {}",
                active.transaction,
                path_value(&active.journal, true),
                active.contention.describe()
            );
        }
        for failure in &report.failures {
            let _ = writeln!(
                output,
                "[FAILED] transaction {} at {}: {}; its journal and unverified entries were retained",
                failure.transaction,
                path_value(&failure.journal, true),
                failure.error
            );
        }
        for completed in &report.completed {
            let _ = writeln!(
                output,
                "[TERMINAL] {} is completed and owns no pending cleanup; it was left unchanged",
                path_value(completed, true)
            );
        }
    } else {
        for rejected in &report.unreadable {
            let _ = writeln!(
                output,
                "[CORRUPT] {}: {}; every journal and recorded path was retained, and no valid neighbor was cleaned",
                path_value(&rejected.path, true),
                rejected.reason
            );
        }
    }
}

fn render_retry_argv(output: &mut String, project_root: Option<&Path>) {
    let mut arguments = vec![OsString::from("asm"), OsString::from("cleanup")];
    match project_root {
        Some(project_root) => {
            arguments.push(OsString::from("--project-root"));
            arguments.push(project_root.as_os_str().to_os_string());
        }
        None => arguments.push(OsString::from("--all")),
    }
    for (index, argument) in arguments.iter().enumerate() {
        let _ = writeln!(output, "  argv[{index}] = {}", os_value(argument, true));
    }
}
