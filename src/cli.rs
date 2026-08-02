//! Shared command-line parser for both executable shims.

use std::ffi::OsString;
use std::path::PathBuf;

use clap::{Args, CommandFactory, Parser, Subcommand, ValueEnum};

use crate::domain::{AgentId, ConflictPolicy, LinkMode, MountMode, RunOptions, ValidationLevel};
use crate::error::AppError;

#[derive(Debug, Parser)]
#[command(
    name = "SkillMount",
    bin_name = "<asm|skillmount>",
    version,
    about = "SkillMount — session-scoped Agent Skills catalog resolver and CLI launcher",
    arg_required_else_help = true,
    disable_help_subcommand = true
)]
struct Cli {
    #[command(subcommand)]
    command: CliCommand,
}

#[derive(Debug, Subcommand)]
enum CliCommand {
    /// Resolve Skills for a future Codex session.
    Codex(SessionArgs),
    /// Resolve Skills for a future Claude Code session.
    Claude(SessionArgs),
    /// Inspect and validate a catalog without modifying the filesystem.
    Inspect(InspectArgs),
    /// Reserved for the later environment-diagnostic change.
    Doctor(DoctorArgs),
    /// Reserved for a later operator-facing recovery command.
    Cleanup(CleanupArgs),
}

#[derive(Debug, Args)]
struct SessionArgs {
    /// Skill directory or direct Skill; repeat for a rightmost-wins overlay.
    #[arg(long = "skills-dir", value_name = "PATH", required = true)]
    skills_dirs: Vec<PathBuf>,

    /// Working directory for the future agent process.
    #[arg(long, value_name = "PATH")]
    cwd: Option<PathBuf>,

    /// Explicit project root.
    #[arg(long, value_name = "PATH")]
    project_root: Option<PathBuf>,

    /// Explicit agent executable path.
    #[arg(long, value_name = "PATH")]
    agent_bin: Option<PathBuf>,

    /// Link implementation.
    #[arg(long, value_enum, default_value_t = RawLinkMode::Auto)]
    link_mode: RawLinkMode,

    /// Mount location strategy.
    #[arg(long, value_enum, default_value_t = RawMountMode::Auto)]
    mount_mode: RawMountMode,

    /// Existing-destination policy.
    #[arg(long, value_enum, default_value_t = RawConflictPolicy::Error)]
    conflict: RawConflictPolicy,

    /// Metadata validation policy.
    #[arg(long, value_enum, default_value_t = RawValidationLevel::Basic)]
    validation: RawValidationLevel,

    /// Keep later planning read-only.
    #[arg(long)]
    dry_run: bool,

    /// Retain later transaction-owned mounts for diagnostics.
    #[arg(long)]
    keep_mounts: bool,

    /// Disable later stale-transaction recovery.
    #[arg(long)]
    no_recover: bool,

    /// Increase diagnostic verbosity.
    #[arg(short = 'v', long = "verbose", action = clap::ArgAction::Count)]
    verbosity: u8,

    /// Opaque arguments forwarded to the future agent process.
    #[arg(last = true, value_name = "AGENT_ARGS", allow_hyphen_values = true)]
    passthrough_args: Vec<OsString>,
}

#[derive(Debug, Args)]
struct InspectArgs {
    /// Skill directory or direct Skill; repeat for a rightmost-wins overlay.
    #[arg(long = "skills-dir", value_name = "PATH", required = true)]
    skills_dirs: Vec<PathBuf>,

    /// Adapter metadata policy to evaluate.
    #[arg(long, value_enum, default_value_t = InspectAgent::All)]
    agent: InspectAgent,

    /// Metadata validation policy.
    #[arg(long, value_enum, default_value_t = RawValidationLevel::Basic)]
    validation: RawValidationLevel,
}

#[derive(Debug, Args)]
struct DoctorArgs {
    /// Explicit project root.
    #[arg(long, value_name = "PATH")]
    project_root: Option<PathBuf>,
}

#[derive(Debug, Args)]
struct CleanupArgs {
    /// Explicit project root.
    #[arg(long, value_name = "PATH")]
    project_root: Option<PathBuf>,

    /// Include every recoverable transaction.
    #[arg(long)]
    all: bool,
}

#[derive(Debug, Clone, Copy, ValueEnum)]
enum RawLinkMode {
    Auto,
    Symlink,
    Junction,
}

#[derive(Debug, Clone, Copy, ValueEnum)]
enum RawMountMode {
    Auto,
    Project,
    Staging,
}

#[derive(Debug, Clone, Copy, ValueEnum)]
enum RawConflictPolicy {
    Error,
    Skip,
}

#[derive(Debug, Clone, Copy, ValueEnum)]
enum RawValidationLevel {
    Basic,
    Strict,
    None,
}

#[derive(Debug, Clone, Copy, ValueEnum)]
pub(crate) enum InspectAgent {
    Codex,
    Claude,
    All,
}

/// Parsed session values before invocation-relative path resolution.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct SessionInput {
    pub(crate) agent: AgentId,
    pub(crate) skills_dirs: Vec<PathBuf>,
    pub(crate) cwd: Option<PathBuf>,
    pub(crate) project_root: Option<PathBuf>,
    pub(crate) agent_bin: Option<PathBuf>,
    pub(crate) passthrough_args: Vec<OsString>,
    pub(crate) options: RunOptions,
}

/// Parsed read-only inspection values.
#[derive(Debug, Clone)]
pub(crate) struct InspectInput {
    pub(crate) skills_dirs: Vec<PathBuf>,
    pub(crate) agent: InspectAgent,
    pub(crate) validation: ValidationLevel,
}

/// Utility command reserved for a later change.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum ReservedUtility {
    Doctor,
    Cleanup,
}

/// One parsed root command.
#[derive(Debug, Clone)]
pub(crate) enum ParsedCommand {
    Session(SessionInput),
    Inspect(InspectInput),
    Reserved(ReservedUtility),
}

pub(crate) fn parse_command_from<I, T>(args: I) -> Result<ParsedCommand, clap::Error>
where
    I: IntoIterator<Item = T>,
    T: Into<OsString> + Clone,
{
    let cli = Cli::try_parse_from(args)?;
    match cli.command {
        CliCommand::Codex(args) => session_input(AgentId::Codex, args),
        CliCommand::Claude(args) => session_input(AgentId::Claude, args),
        CliCommand::Inspect(args) => Ok(ParsedCommand::Inspect(InspectInput {
            skills_dirs: args.skills_dirs,
            agent: args.agent,
            validation: args.validation.into(),
        })),
        CliCommand::Doctor(args) => {
            let _ = args.project_root;
            Ok(ParsedCommand::Reserved(ReservedUtility::Doctor))
        }
        CliCommand::Cleanup(args) => {
            let _ = (args.project_root, args.all);
            Ok(ParsedCommand::Reserved(ReservedUtility::Cleanup))
        }
    }
}

fn session_input(agent: AgentId, args: SessionArgs) -> Result<ParsedCommand, clap::Error> {
    let input = normalize_session(agent, args).map_err(|error| {
        Cli::command().error(clap::error::ErrorKind::InvalidValue, error.to_string())
    })?;
    Ok(ParsedCommand::Session(input))
}

fn normalize_session(agent: AgentId, args: SessionArgs) -> Result<SessionInput, AppError> {
    let link_mode = match args.link_mode {
        RawLinkMode::Auto => LinkMode::Auto,
        RawLinkMode::Symlink => LinkMode::Symlink,
        RawLinkMode::Junction if cfg!(windows) => LinkMode::Junction,
        RawLinkMode::Junction => {
            return Err(AppError::Usage(
                "--link-mode=junction is supported only on Windows".to_owned(),
            ));
        }
    };

    let mount_mode = match (agent, args.mount_mode) {
        (AgentId::Codex, RawMountMode::Auto | RawMountMode::Project)
        | (AgentId::Claude, RawMountMode::Project) => MountMode::Project,
        (AgentId::Claude, RawMountMode::Auto | RawMountMode::Staging) => MountMode::Staging,
        (AgentId::Codex, RawMountMode::Staging) => {
            return Err(AppError::Usage(
                "--mount-mode=staging is incompatible with Codex".to_owned(),
            ));
        }
    };

    Ok(SessionInput {
        agent,
        skills_dirs: args.skills_dirs,
        cwd: args.cwd,
        project_root: args.project_root,
        agent_bin: args.agent_bin,
        passthrough_args: args.passthrough_args,
        options: RunOptions {
            link_mode,
            mount_mode,
            conflict: args.conflict.into(),
            validation: args.validation.into(),
            dry_run: args.dry_run,
            keep_mounts: args.keep_mounts,
            no_recover: args.no_recover,
            verbosity: args.verbosity,
        },
    })
}

impl From<RawConflictPolicy> for ConflictPolicy {
    fn from(value: RawConflictPolicy) -> Self {
        match value {
            RawConflictPolicy::Error => Self::Error,
            RawConflictPolicy::Skip => Self::Skip,
        }
    }
}

impl From<RawValidationLevel> for ValidationLevel {
    fn from(value: RawValidationLevel) -> Self {
        match value {
            RawValidationLevel::Basic => Self::Basic,
            RawValidationLevel::Strict => Self::Strict,
            RawValidationLevel::None => Self::None,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::{ParsedCommand, parse_command_from};
    use crate::domain::{AgentId, LinkMode, MountMode, ValidationLevel};
    use clap::error::ErrorKind;
    use std::ffi::OsString;
    use std::path::PathBuf;

    fn session(arguments: Vec<OsString>) -> super::SessionInput {
        let command = parse_command_from(arguments).expect("session should parse");
        let ParsedCommand::Session(session) = command else {
            panic!("expected session command");
        };
        session
    }

    #[test]
    fn ordered_sources_defaults_and_passthrough_are_preserved() {
        let args = [
            "asm",
            "claude",
            "--skills-dir",
            "first",
            "--skills-dir=second",
            "--skills-dir",
            "third",
            "--",
            "-p",
            "日本語 \"$HOME\" \\ quoted; & |",
        ]
        .into_iter()
        .map(OsString::from)
        .collect();

        let input = session(args);

        assert_eq!(input.agent, AgentId::Claude);
        assert_eq!(
            input.skills_dirs,
            [
                PathBuf::from("first"),
                PathBuf::from("second"),
                PathBuf::from("third")
            ]
        );
        assert_eq!(input.options.mount_mode, MountMode::Staging);
        assert_eq!(input.options.link_mode, LinkMode::Auto);
        assert_eq!(input.options.validation, ValidationLevel::Basic);
        assert_eq!(
            input.passthrough_args,
            [
                OsString::from("-p"),
                OsString::from("日本語 \"$HOME\" \\ quoted; & |")
            ]
        );
    }

    #[test]
    fn sources_are_required() {
        let error = parse_command_from(["asm", "codex"]).expect_err("source is required");
        assert_eq!(error.kind(), ErrorKind::MissingRequiredArgument);
    }

    #[test]
    fn codex_staging_is_rejected() {
        let error = parse_command_from([
            "asm",
            "codex",
            "--skills-dir",
            "skills",
            "--mount-mode=staging",
        ])
        .expect_err("staging should be rejected");
        assert_eq!(error.kind(), ErrorKind::InvalidValue);
    }

    #[test]
    fn copy_mode_is_hidden_and_rejected() {
        let help = parse_command_from(["asm", "codex", "--help"]).expect_err("help exits");
        let rendered = help.to_string();
        assert!(!rendered.contains("copy"));

        let error =
            parse_command_from(["asm", "codex", "--skills-dir", "skills", "--link-mode=copy"])
                .expect_err("copy should be rejected");
        assert_eq!(error.kind(), ErrorKind::InvalidValue);
    }

    #[cfg(unix)]
    #[test]
    fn non_utf8_sources_and_passthrough_are_preserved() {
        use std::os::unix::ffi::OsStringExt;

        let source = OsString::from_vec(vec![b's', 0xff]);
        let passthrough = OsString::from_vec(vec![b'p', 0xfe]);
        let input = session(vec![
            OsString::from("asm"),
            OsString::from("codex"),
            OsString::from("--skills-dir"),
            source.clone(),
            OsString::from("--"),
            passthrough.clone(),
        ]);
        assert_eq!(input.skills_dirs, [PathBuf::from(source)]);
        assert_eq!(input.passthrough_args, [passthrough]);
    }

    #[cfg(windows)]
    #[test]
    fn non_unicode_sources_and_passthrough_are_preserved() {
        use std::os::windows::ffi::OsStringExt;

        let source = OsString::from_wide(&[u16::from(b's'), 0xd800]);
        let passthrough = OsString::from_wide(&[u16::from(b'p'), 0xd801]);
        let input = session(vec![
            OsString::from("asm"),
            OsString::from("codex"),
            OsString::from("--skills-dir"),
            source.clone(),
            OsString::from("--"),
            passthrough.clone(),
        ]);
        assert_eq!(input.skills_dirs, [PathBuf::from(source)]);
        assert_eq!(input.passthrough_args, [passthrough]);
    }
}
