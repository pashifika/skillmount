//! Bounded classification of OMP 17.2.9 root-command arguments.
//!
//! Every table here is transcribed from the tagged OMP source recorded in ADR 0034, not from OMP's
//! help output or its shipped completion scripts: several functional flags are absent from both.
//! Values stay platform-native `OsStr` throughout, because a forwarded path or prompt may not be
//! UTF-8 and must reach the child byte for byte.

use std::ffi::{OsStr, OsString};

use crate::error::AppError;

/// Flags whose value is the next token, consumed even when that token looks like a flag.
///
/// OMP's tokenizer consumes unconditionally here, so `--system-prompt --resume` makes the literal
/// string `--resume` the prompt rather than resuming a session. Classification must agree, or
/// `SkillMount` would reject an argument OMP never treats as a control.
const REQUIRED_VALUE_FLAGS: &[&str] = &[
    "--add-dir",
    "--alias",
    "--api-key",
    "--append-system-prompt",
    "--approval-mode",
    "--config",
    "--cwd",
    "--export",
    "--extension",
    "--fork",
    "--hook",
    "--max-time",
    "--mode",
    "--model",
    "--models",
    "--plan",
    "--plan-yolo-into",
    "--plugin-dir",
    "--prewalk-into",
    "--profile",
    "--prompt-cache-key",
    "--provider",
    "--provider-session-id",
    "--service-tier",
    "--session-dir",
    "--skills",
    "--slow",
    "--smol",
    "--system-prompt",
    "--thinking",
    "--tools",
    "-e",
];

/// Flags that consume the next token only when it exists, is non-empty, and is not flag-shaped.
const OPTIONAL_VALUE_FLAGS: &[&str] = &["--resume", "--session", "-r"];

/// Flags that never consume a value.
const NO_VALUE_FLAGS: &[&str] = &[
    "--advisor",
    "--allow-home",
    "--auto-approve",
    "--continue",
    "--from-claude",
    "--from-codex",
    "--help",
    "--hide-thinking",
    "--no-extensions",
    "--no-lsp",
    "--no-prewalk",
    "--no-pty",
    "--no-rules",
    "--no-session",
    "--no-skills",
    "--no-title",
    "--no-tools",
    "--prewalk",
    "--plan-yolo",
    "--print",
    "--print-thoughts",
    "--version",
    "--yolo",
    "-c",
    "-h",
    "-p",
    "-v",
];

/// OMP's internal profile-bootstrap sentinel, which its tokenizer skips without a value.
const PROFILE_BOUNDARY_FLAG: &str = "--omp-profile-boundary";

/// Options that relocate the inspected root, profile, or settings layers.
const ROOT_CHANGING_FLAGS: &[&str] = &["--alias", "--config", "--cwd", "--profile"];

/// Options that change the discovered or selected Skill set outside persistent configuration.
const DISCOVERY_CHANGING_FLAGS: &[&str] = &[
    "--extension",
    "--hook",
    "--no-extensions",
    "--no-skills",
    "--plugin-dir",
    "--skills",
    "-e",
];

/// Options that resume, fork, import, relocate, or replace the new-session contract.
const NON_SESSION_FLAGS: &[&str] = &[
    "--continue",
    "--export",
    "--fork",
    "--from-claude",
    "--from-codex",
    "--resume",
    "--session",
    "-c",
    "-r",
];

/// `--mode` values that are not a supervised foreground session.
const REJECTED_MODES: &[&str] = &["acp", "rpc", "rpc-ui"];

/// Every command OMP dispatches instead of starting a session, including its hidden entries.
const NON_SESSION_COMMANDS: &[&str] = &[
    "__complete",
    "acp",
    "agents",
    "auth-broker",
    "auth-gateway",
    "bench",
    "browser-relay",
    "cleanse",
    "commit",
    "completions",
    "config",
    "dry-balance",
    "gallery",
    "gc",
    "grep",
    "grievances",
    "install",
    "join",
    "launch",
    "models",
    "plugin",
    "q",
    "read",
    "say",
    "search",
    "setup",
    "shell",
    "ssh",
    "stats",
    "tiny-models",
    "token",
    "ttsr",
    "update",
    "usage",
    "worktree",
    "wt",
];

/// The token that lets an operator keep the inspected root when the launch CWD is the user home.
pub(super) const ALLOW_HOME_FLAG: &str = "--allow-home";

/// How OMP would treat one token in a flag position.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Arity {
    Required,
    Optional,
    None,
}

/// One classified OMP token.
#[derive(Debug, Clone, PartialEq, Eq)]
enum Token {
    /// A flag with no attached value, plus whether it consumed the following token.
    Flag {
        name: &'static str,
        value: Option<OsString>,
    },
    /// A flag written as `--name=value`.
    AttachedFlag { name: &'static str, value: OsString },
    /// A token OMP does not recognize as a flag in this position.
    Unknown(OsString),
    /// A positional token before OMP's own `--`.
    Positional(OsString),
    /// A token after OMP's own `--`, which is always literal message text.
    Terminated(OsString),
}

/// Everything the adapter needs to know about one OMP passthrough argument list.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub(super) struct Passthrough {
    /// Whether the operator supplied `--allow-home`.
    pub(super) allows_home: bool,
}

/// Classifies and validates OMP passthrough arguments.
///
/// # Errors
///
/// Returns [`AppError::Usage`] naming the conflicting token whenever an argument would change the
/// inspected discovery root, settings profile, provider set, or foreground ownership contract after
/// planning.
pub(super) fn validate(args: &[OsString]) -> Result<Passthrough, AppError> {
    let tokens = classify(args);
    let mut passthrough = Passthrough::default();
    let mut command_position = true;

    for token in &tokens {
        match token {
            Token::Flag { name, value } => {
                reject_flag(name)?;
                if *name == "--mode" {
                    reject_mode(value.as_deref())?;
                }
                if *name == ALLOW_HOME_FLAG {
                    passthrough.allows_home = true;
                }
            }
            Token::AttachedFlag { name, value } => {
                reject_flag(name)?;
                if *name == "--mode" {
                    reject_mode(Some(value))?;
                }
            }
            Token::Positional(value) => {
                if command_position {
                    reject_command(value)?;
                }
                // OMP takes only the first non-flag token as a command candidate; a later
                // command-shaped word is ordinary prompt text.
                command_position = false;
            }
            // An unknown flag is OMP's own hard usage error, and everything after OMP's `--` is
            // literal message text. Neither can name a control this adapter must gate.
            Token::Unknown(_) | Token::Terminated(_) => {}
        }
    }

    Ok(passthrough)
}

/// Splits the argument list the way OMP's tokenizer splits it.
fn classify(args: &[OsString]) -> Vec<Token> {
    let mut tokens = Vec::with_capacity(args.len());
    let mut index = 0;
    let mut terminated = false;

    while index < args.len() {
        let argument = args[index].as_os_str();
        index += 1;

        if terminated {
            tokens.push(Token::Terminated(argument.to_os_string()));
            continue;
        }
        if argument == OsStr::new("--") {
            terminated = true;
            continue;
        }
        if argument == OsStr::new(PROFILE_BOUNDARY_FLAG) {
            continue;
        }

        if let Some((name, value)) = split_attached(argument) {
            tokens.push(Token::AttachedFlag { name, value });
            continue;
        }
        let Some((name, arity)) = known_flag(argument) else {
            if is_flag_shaped(argument) {
                tokens.push(Token::Unknown(argument.to_os_string()));
            } else {
                tokens.push(Token::Positional(argument.to_os_string()));
            }
            continue;
        };

        let value = match arity {
            Arity::Required => {
                let value = args.get(index).cloned();
                if value.is_some() {
                    index += 1;
                }
                value
            }
            Arity::Optional => match args.get(index) {
                Some(next) if !is_flag_shaped(next) && !next.is_empty() => {
                    index += 1;
                    Some(next.clone())
                }
                _ => None,
            },
            Arity::None => None,
        };
        tokens.push(Token::Flag { name, value });
    }

    tokens
}

/// Splits a long `--name=value` token, which is the only attached form OMP accepts.
fn split_attached(argument: &OsStr) -> Option<(&'static str, OsString)> {
    if !argument.as_encoded_bytes().starts_with(b"--") {
        return None;
    }
    let (name, value) = split_at_equals(argument)?;
    let (name, _) = known_flag(&name)?;
    Some((name, value))
}

/// Splits one token at its first ASCII `=` without any UTF-8 round trip.
///
/// The split happens on the platform's own storage unit — bytes on Unix, UTF-16 code units on
/// Windows — so an ill-formed value survives unchanged in both halves.
#[cfg(unix)]
fn split_at_equals(argument: &OsStr) -> Option<(OsString, OsString)> {
    use std::os::unix::ffi::{OsStrExt, OsStringExt};

    let bytes = argument.as_bytes();
    let separator = bytes.iter().position(|byte| *byte == b'=')?;
    Some((
        OsString::from_vec(bytes[..separator].to_vec()),
        OsString::from_vec(bytes[separator + 1..].to_vec()),
    ))
}

#[cfg(windows)]
fn split_at_equals(argument: &OsStr) -> Option<(OsString, OsString)> {
    use std::os::windows::ffi::{OsStrExt, OsStringExt};

    let units: Vec<u16> = argument.encode_wide().collect();
    let separator = units.iter().position(|unit| *unit == u16::from(b'='))?;
    Some((
        OsString::from_wide(&units[..separator]),
        OsString::from_wide(&units[separator + 1..]),
    ))
}

/// Returns the canonical spelling and arity of a token OMP recognizes as a flag.
fn known_flag(argument: &OsStr) -> Option<(&'static str, Arity)> {
    let lookup = |table: &'static [&'static str]| {
        table
            .iter()
            .copied()
            .find(|name| argument == OsStr::new(*name))
    };
    lookup(REQUIRED_VALUE_FLAGS)
        .map(|name| (name, Arity::Required))
        .or_else(|| lookup(OPTIONAL_VALUE_FLAGS).map(|name| (name, Arity::Optional)))
        .or_else(|| lookup(NO_VALUE_FLAGS).map(|name| (name, Arity::None)))
}

/// Returns whether OMP's tokenizer would treat this token as a flag rather than a positional.
///
/// Only the leading ASCII byte is inspected. `OsStr::as_encoded_bytes` is an ASCII-compatible
/// superset of UTF-8 on both supported platforms, so this is exact for `-` without decoding.
fn is_flag_shaped(argument: &OsStr) -> bool {
    let bytes = argument.as_encoded_bytes();
    bytes.first() == Some(&b'-') && bytes.len() > 1
}

fn reject_flag(name: &str) -> Result<(), AppError> {
    if ROOT_CHANGING_FLAGS.contains(&name) {
        return Err(AppError::Usage(format!(
            "OMP argument {name} relocates the discovery root, profile, or settings layers that \
             SkillMount already inspected, or exits instead of starting a session; use \
             SkillMount's own --cwd option instead"
        )));
    }
    if DISCOVERY_CHANGING_FLAGS.contains(&name) {
        return Err(AppError::Usage(format!(
            "OMP argument {name} changes the Skill, extension, or provider set outside the \
             persistent configuration SkillMount inspected; remove it or run the agent directly"
        )));
    }
    if NON_SESSION_FLAGS.contains(&name) {
        return Err(AppError::Usage(format!(
            "OMP argument {name} resumes, forks, imports, exports, or relocates a session instead \
             of starting the new session SkillMount planned; run that mode directly"
        )));
    }
    Ok(())
}

fn reject_mode(value: Option<&OsStr>) -> Result<(), AppError> {
    let Some(value) = value else {
        return Ok(());
    };
    for mode in REJECTED_MODES {
        if value == OsStr::new(*mode) {
            return Err(AppError::Usage(format!(
                "OMP --mode={mode} runs a protocol server rather than the bounded foreground \
                 session SkillMount supervises; use the default mode, --mode=text, or --mode=json"
            )));
        }
    }
    Ok(())
}

fn reject_command(value: &OsStr) -> Result<(), AppError> {
    for command in NON_SESSION_COMMANDS {
        if value == OsStr::new(*command) {
            return Err(AppError::Usage(format!(
                "OMP command {command:?} does not start a supervised foreground session; run that \
                 command directly"
            )));
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::{ALLOW_HOME_FLAG, Passthrough, validate};
    use crate::error::ExitCategory;
    use std::ffi::OsString;

    fn args(values: &[&str]) -> Vec<OsString> {
        values.iter().map(OsString::from).collect()
    }

    fn rejected(values: &[&str]) -> String {
        let error = validate(&args(values)).expect_err("the argument list must be rejected");
        assert_eq!(error.category(), ExitCategory::Usage, "{values:?}");
        error.to_string()
    }

    #[test]
    fn ordinary_foreground_arguments_are_forwarded_unchanged() {
        for accepted in [
            vec!["--print", "explain this"],
            vec!["--mode", "json"],
            vec!["--mode=text"],
            vec!["--model", "opus", "hello"],
            vec!["--no-session"],
            vec!["--add-dir", "extra"],
            vec!["--thinking", "high"],
            vec!["--auto-approve"],
            vec!["--yolo"],
            vec!["--approval-mode", "write"],
            vec!["--tools", "read,bash"],
            vec!["-p", "prompt"],
        ] {
            assert_eq!(
                validate(&args(&accepted)).expect("the argument list must be accepted"),
                Passthrough { allows_home: false },
                "{accepted:?}"
            );
        }
    }

    #[test]
    fn root_and_settings_relocation_is_rejected_in_every_accepted_spelling() {
        for rejected_args in [
            vec!["--cwd", "other"],
            vec!["--cwd=other"],
            vec!["--profile", "work"],
            vec!["--profile=work"],
            vec!["--alias", "omp-work"],
            vec!["--config", "overlay.yml"],
            vec!["--config=overlay.yml"],
        ] {
            let message = rejected(&rejected_args);
            assert!(
                message.contains("relocates the discovery root"),
                "{message}"
            );
        }
    }

    #[test]
    fn skill_and_provider_controls_are_rejected() {
        for rejected_args in [
            vec!["--no-skills"],
            vec!["--skills", "git-*"],
            vec!["--skills=git-*"],
            vec!["-e", "./ext.ts"],
            vec!["--extension", "./ext.ts"],
            vec!["--hook", "./hook.ts"],
            vec!["--no-extensions"],
            vec!["--plugin-dir", "./plugins/demo"],
        ] {
            let message = rejected(&rejected_args);
            assert!(message.contains("changes the Skill"), "{message}");
        }
    }

    #[test]
    fn resume_import_and_export_paths_are_rejected() {
        for rejected_args in [
            vec!["-c"],
            vec!["--continue"],
            vec!["-r"],
            vec!["--resume", "abc123"],
            vec!["--resume=abc123"],
            vec!["--session", "abc123"],
            vec!["--fork", "abc123"],
            vec!["--from-claude"],
            vec!["--from-codex"],
            vec!["--export", "out.html"],
        ] {
            let message = rejected(&rejected_args);
            assert!(message.contains("resumes, forks, imports"), "{message}");
        }
    }

    #[test]
    fn protocol_modes_are_rejected_and_print_modes_are_not() {
        for mode in ["rpc", "rpc-ui", "acp"] {
            let message = rejected(&["--mode", mode]);
            assert!(message.contains("protocol server"), "{message}");
            let attached = rejected(&[&format!("--mode={mode}")]);
            assert!(attached.contains("protocol server"), "{attached}");
        }
        for mode in ["text", "json"] {
            assert!(validate(&args(&["--mode", mode])).is_ok(), "{mode}");
        }
    }

    #[test]
    fn every_non_session_command_is_rejected_in_the_command_position() {
        for command in [
            "acp", "config", "plugin", "shell", "worktree", "wt", "q", "gc",
        ] {
            let message = rejected(&[command]);
            assert!(message.contains("does not start a supervised"), "{message}");
        }
    }

    #[test]
    fn a_command_word_reached_through_a_flag_value_is_not_a_command() {
        // OMP consumes a required value even when it is flag-shaped, and takes only the first
        // non-flag token as a command candidate.
        for accepted in [
            vec!["--model", "acp"],
            vec!["--system-prompt", "--allow-home"],
            vec!["--thinking", "config"],
            vec!["prompt", "config"],
            vec!["--", "acp"],
            vec!["--", "--cwd", "other"],
        ] {
            assert!(validate(&args(&accepted)).is_ok(), "{accepted:?}");
        }
    }

    #[test]
    fn a_short_boolean_does_not_swallow_the_following_command() {
        let message = rejected(&["-p", "acp"]);
        assert!(message.contains("does not start a supervised"), "{message}");
    }

    #[test]
    fn a_required_value_flag_consumes_a_flag_shaped_token_that_would_otherwise_be_rejected() {
        // `--model` consumes unconditionally, so the following token is a model name rather than a
        // control SkillMount must gate. Getting this wrong would reject an argument OMP accepts.
        assert!(validate(&args(&["--model", "--cwd"])).is_ok());
        assert!(validate(&args(&["--system-prompt", "--no-skills"])).is_ok());

        // A boolean consumes nothing, so the same token stays a control.
        let message = rejected(&["--print", "--cwd", "other"]);
        assert!(
            message.contains("relocates the discovery root"),
            "{message}"
        );
    }

    #[test]
    fn every_optional_value_flag_is_rejected_in_both_forms() {
        // All three of OMP's optional-value flags resume a session, so arity never changes the
        // outcome for them; each spelling must still be named in the diagnostic.
        for spelling in [
            vec!["-r"],
            vec!["-r", "abc"],
            vec!["--resume"],
            vec!["--resume", "abc"],
            vec!["--resume="],
            vec!["--session", "abc"],
        ] {
            let message = rejected(&spelling);
            assert!(message.contains("resumes, forks, imports"), "{spelling:?}");
        }
    }

    #[test]
    fn the_home_escape_permission_is_observed_and_never_injected() {
        assert_eq!(
            validate(&args(&[ALLOW_HOME_FLAG])).expect("--allow-home is accepted"),
            Passthrough { allows_home: true }
        );
        assert_eq!(
            validate(&args(&["--", ALLOW_HOME_FLAG])).expect("a terminated token is accepted"),
            Passthrough { allows_home: false },
            "a token after OMP's own terminator is literal message text"
        );
    }

    #[test]
    fn an_unknown_flag_is_left_to_omp_and_a_unicode_prompt_survives() {
        assert!(validate(&args(&["--not-a-flag", "value"])).is_ok());
        assert!(validate(&args(&["-eFOO"])).is_ok());
        assert!(validate(&args(&["日本語", "説明して"])).is_ok());
    }

    #[cfg(unix)]
    #[test]
    fn a_non_unicode_value_is_classified_without_a_utf8_round_trip() {
        use std::os::unix::ffi::OsStringExt;

        let non_unicode = OsString::from_vec(vec![0x66, 0xFF, 0x6F]);
        let mut arguments = vec![OsString::from("--system-prompt")];
        arguments.push(non_unicode.clone());
        assert!(validate(&arguments).is_ok());

        // The same bytes in the command position are not a command name.
        assert!(validate(&[non_unicode]).is_ok());
    }
}
