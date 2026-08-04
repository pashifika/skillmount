//! Shared command-line parser for both executable shims.

use std::ffi::{OsStr, OsString};
use std::path::{Path, PathBuf};

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
    /// Run a Codex session with the selected Skills.
    Codex(SessionArgs),
    /// Resolve Skills for a future Claude Code session.
    Claude(SessionArgs),
    /// Generate a shell completion script on standard output.
    Completions(CompletionArgs),
    /// Inspect and validate a catalog without modifying the filesystem.
    Inspect(InspectArgs),
    /// Inspect agent, discovery, link, lock, and transaction health.
    Doctor(DoctorArgs),
    /// Reconcile transaction-owned residue from durable evidence.
    Cleanup(CleanupArgs),
}

#[derive(Debug, Args)]
struct SessionArgs {
    /// Skill directory or direct Skill; repeat for a rightmost-wins overlay.
    #[arg(
        long = "skills-dir",
        value_name = "PATH",
        value_hint = clap::ValueHint::DirPath,
        required = true
    )]
    skills_dirs: Vec<PathBuf>,

    /// Working directory for the selected agent process.
    #[arg(long, value_name = "PATH", value_hint = clap::ValueHint::DirPath)]
    cwd: Option<PathBuf>,

    /// Explicit project root.
    #[arg(long, value_name = "PATH", value_hint = clap::ValueHint::DirPath)]
    project_root: Option<PathBuf>,

    /// Explicit agent executable path.
    #[arg(
        long,
        value_name = "PATH",
        value_hint = clap::ValueHint::ExecutablePath
    )]
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

    /// Opaque arguments forwarded to the selected agent process.
    #[arg(last = true, value_name = "AGENT_ARGS", allow_hyphen_values = true)]
    passthrough_args: Vec<OsString>,
}

#[derive(Debug, Args)]
struct InspectArgs {
    /// Skill directory or direct Skill; repeat for a rightmost-wins overlay.
    #[arg(
        long = "skills-dir",
        value_name = "PATH",
        value_hint = clap::ValueHint::DirPath,
        required = true
    )]
    skills_dirs: Vec<PathBuf>,

    /// Adapter metadata policy to evaluate.
    #[arg(long, value_enum, default_value_t = InspectAgent::All)]
    agent: InspectAgent,

    /// Metadata validation policy.
    #[arg(long, value_enum, default_value_t = RawValidationLevel::Basic)]
    validation: RawValidationLevel,
}

#[derive(Debug, Args)]
struct CompletionArgs {
    /// Shell whose static completion script should be generated.
    #[arg(value_enum, value_name = "SHELL")]
    shell: CompletionShell,
}

#[derive(Debug, Args)]
struct DoctorArgs {
    /// Explicit project root.
    #[arg(long, value_name = "PATH", value_hint = clap::ValueHint::DirPath)]
    project_root: Option<PathBuf>,

    /// Explicit Codex executable path; otherwise search PATH.
    #[arg(
        long,
        value_name = "PATH",
        value_hint = clap::ValueHint::ExecutablePath
    )]
    codex_bin: Option<PathBuf>,

    /// Explicit Claude Code executable path; otherwise search PATH.
    #[arg(
        long,
        value_name = "PATH",
        value_hint = clap::ValueHint::ExecutablePath
    )]
    claude_bin: Option<PathBuf>,
}

#[derive(Debug, Args)]
struct CleanupArgs {
    /// Explicit project root.
    #[arg(long, value_name = "PATH", value_hint = clap::ValueHint::DirPath)]
    project_root: Option<PathBuf>,

    /// Include every recoverable transaction.
    #[arg(long, conflicts_with = "project_root")]
    all: bool,
}

/// Shells whose static completion behavior `SkillMount` verifies natively.
#[derive(Debug, Clone, Copy, PartialEq, Eq, ValueEnum)]
pub(crate) enum CompletionShell {
    /// Bash.
    Bash,
    /// Zsh.
    Zsh,
    /// Fish.
    Fish,
    /// PowerShell.
    #[value(name = "powershell")]
    PowerShell,
}

/// Installed product name to which one generated script is bound.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum ProductBinary {
    /// Primary `asm` executable.
    Asm,
    /// Behaviorally identical `skillmount` fallback.
    Skillmount,
}

impl ProductBinary {
    pub(crate) const fn registration_name(self) -> &'static str {
        match self {
            Self::Asm => "asm",
            Self::Skillmount => "skillmount",
        }
    }

    fn from_argv0(argv0: &OsStr) -> Option<Self> {
        match Path::new(argv0).file_name() {
            Some(name) if name == OsStr::new("asm") || name == OsStr::new("asm.exe") => {
                Some(Self::Asm)
            }
            Some(name)
                if name == OsStr::new("skillmount") || name == OsStr::new("skillmount.exe") =>
            {
                Some(Self::Skillmount)
            }
            _ => None,
        }
    }
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

/// Parsed environment-diagnostic values.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct DoctorInput {
    pub(crate) project_root: Option<PathBuf>,
    pub(crate) codex_bin: Option<PathBuf>,
    pub(crate) claude_bin: Option<PathBuf>,
}

/// Parsed explicit-recovery values.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct CleanupInput {
    pub(crate) project_root: Option<PathBuf>,
    pub(crate) all: bool,
}

/// Typed completion-generation request.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct CompletionInput {
    pub(crate) shell: CompletionShell,
    pub(crate) product: ProductBinary,
}

/// One parsed root command.
#[derive(Debug, Clone)]
pub(crate) enum ParsedCommand {
    Completions(CompletionInput),
    Session(SessionInput),
    Inspect(InspectInput),
    Doctor(DoctorInput),
    Cleanup(CleanupInput),
}

/// Returns a fresh shared command graph for completion generation.
pub(crate) fn command() -> clap::Command {
    Cli::command()
}

pub(crate) fn parse_command_from<I, T>(args: I) -> Result<ParsedCommand, clap::Error>
where
    I: IntoIterator<Item = T>,
    T: Into<OsString> + Clone,
{
    let args = args.into_iter().map(Into::into).collect::<Vec<_>>();
    let product = args
        .first()
        .and_then(|argv0| ProductBinary::from_argv0(argv0));
    let cli = Cli::try_parse_from(args)?;
    match cli.command {
        CliCommand::Codex(args) => session_input(AgentId::Codex, args),
        CliCommand::Claude(args) => session_input(AgentId::Claude, args),
        CliCommand::Completions(args) => {
            let product = product.ok_or_else(|| {
                command().error(
                    clap::error::ErrorKind::InvalidValue,
                    "completion generation requires the installed executable name `asm` or \
                     `skillmount` (with the normal `.exe` suffix on Windows)",
                )
            })?;
            Ok(ParsedCommand::Completions(CompletionInput {
                shell: args.shell,
                product,
            }))
        }
        CliCommand::Inspect(args) => Ok(ParsedCommand::Inspect(InspectInput {
            skills_dirs: args.skills_dirs,
            agent: args.agent,
            validation: args.validation.into(),
        })),
        CliCommand::Doctor(args) => Ok(ParsedCommand::Doctor(DoctorInput {
            project_root: args.project_root,
            codex_bin: args.codex_bin,
            claude_bin: args.claude_bin,
        })),
        CliCommand::Cleanup(args) => Ok(ParsedCommand::Cleanup(CleanupInput {
            project_root: args.project_root,
            all: args.all,
        })),
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
    use super::{
        CompletionInput, CompletionShell, ParsedCommand, ProductBinary, command, parse_command_from,
    };
    use crate::domain::{AgentId, LinkMode, MountMode, ValidationLevel};
    use clap::ValueHint;
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

    fn completion(argv0: &str, shell: &str) -> CompletionInput {
        let command = parse_command_from([argv0, "completions", shell])
            .expect("completion request should parse");
        let ParsedCommand::Completions(input) = command else {
            panic!("expected completion command");
        };
        input
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
    #[test]
    fn completion_shell_values_parse() {
        for (value, expected) in [
            ("bash", CompletionShell::Bash),
            ("zsh", CompletionShell::Zsh),
            ("fish", CompletionShell::Fish),
            ("powershell", CompletionShell::PowerShell),
        ] {
            assert_eq!(completion("asm", value).shell, expected);
        }
    }

    #[test]
    fn completion_shell_is_required_with_usage_category() {
        let error =
            parse_command_from(["asm", "completions"]).expect_err("shell should be required");
        assert_eq!(error.kind(), ErrorKind::MissingRequiredArgument);
    }

    #[test]
    fn only_advertised_completion_shells_parse() {
        for value in ["elvish", "nushell", "power-shell"] {
            let error = parse_command_from(["asm", "completions", value])
                .expect_err("unadvertised shell should be rejected");
            assert_eq!(error.kind(), ErrorKind::InvalidValue);
            let rendered = error.to_string();
            for supported in ["bash", "zsh", "fish", "powershell"] {
                assert!(rendered.contains(supported), "{rendered}");
            }
        }
    }

    #[test]
    fn completion_product_identity_recognizes_installed_names() {
        assert_eq!(
            completion("/installed/asm", "bash").product,
            ProductBinary::Asm
        );
        assert_eq!(
            completion("/installed/skillmount", "bash").product,
            ProductBinary::Skillmount
        );
    }

    #[test]
    fn completion_product_identity_recognizes_windows_suffixes() {
        assert_eq!(completion("asm.exe", "bash").product, ProductBinary::Asm);
        assert_eq!(
            completion("skillmount.exe", "bash").product,
            ProductBinary::Skillmount
        );
    }

    #[test]
    fn completion_product_identity_rejects_renamed_alias() {
        let error = parse_command_from(["renamed", "completions", "bash"])
            .expect_err("renamed alias should be rejected");
        assert_eq!(error.kind(), ErrorKind::InvalidValue);
        let rendered = error.to_string();
        assert!(rendered.contains("`asm`"), "{rendered}");
        assert!(rendered.contains("`skillmount`"), "{rendered}");
    }

    #[test]
    fn completion_metadata_preserves_native_paths_and_opaque_passthrough() {
        let input = session(
            [
                "asm",
                "codex",
                "--skills-dir",
                "relative/skills",
                "--cwd",
                "relative/work",
                "--project-root",
                "relative/project",
                "--agent-bin",
                "relative/bin/codex",
                "--",
                "--search",
                "literal value",
            ]
            .into_iter()
            .map(OsString::from)
            .collect(),
        );

        assert_eq!(input.skills_dirs, [PathBuf::from("relative/skills")]);
        assert_eq!(input.cwd, Some(PathBuf::from("relative/work")));
        assert_eq!(input.project_root, Some(PathBuf::from("relative/project")));
        assert_eq!(input.agent_bin, Some(PathBuf::from("relative/bin/codex")));
        assert_eq!(
            input.passthrough_args,
            [OsString::from("--search"), OsString::from("literal value")]
        );
    }

    #[test]
    fn completion_command_graph_carries_path_value_hints() {
        let mut root = command();
        root.build();
        let cases = [
            ("codex", "skills_dirs", ValueHint::DirPath),
            ("codex", "cwd", ValueHint::DirPath),
            ("codex", "project_root", ValueHint::DirPath),
            ("codex", "agent_bin", ValueHint::ExecutablePath),
            ("claude", "skills_dirs", ValueHint::DirPath),
            ("claude", "cwd", ValueHint::DirPath),
            ("claude", "project_root", ValueHint::DirPath),
            ("claude", "agent_bin", ValueHint::ExecutablePath),
            ("inspect", "skills_dirs", ValueHint::DirPath),
            ("doctor", "project_root", ValueHint::DirPath),
            ("doctor", "codex_bin", ValueHint::ExecutablePath),
            ("doctor", "claude_bin", ValueHint::ExecutablePath),
            ("cleanup", "project_root", ValueHint::DirPath),
        ];

        for (subcommand, argument, expected) in cases {
            let command = root
                .find_subcommand(subcommand)
                .expect("subcommand should exist");
            let argument = command
                .get_arguments()
                .find(|candidate| candidate.get_id().as_str() == argument)
                .expect("argument should exist");
            assert_eq!(
                argument.get_value_hint(),
                expected,
                "{subcommand} --{argument:?}"
            );
        }
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
