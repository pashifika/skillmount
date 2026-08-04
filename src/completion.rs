//! Static shell-completion generation over the shared CLI command graph.

use std::collections::BTreeSet;
use std::io::{self, Write};

use clap::{Arg, Command, ValueHint};
use clap_complete::aot::{Bash, Fish, Generator, Zsh};

use crate::cli::{self, CompletionInput, CompletionShell};

/// Generates one completion script without touching product or filesystem state.
pub(crate) fn generate(input: CompletionInput, writer: &mut dyn Write) -> io::Result<()> {
    let mut command = cli::command();
    let bin_name = input.product.registration_name();
    command.set_bin_name(bin_name);
    command.build();

    match input.shell {
        CompletionShell::Bash => {
            generate_guarded_aot(&Bash, &command, bin_name, GuardKind::Bash, writer)
        }
        CompletionShell::Zsh => {
            generate_guarded_aot(&Zsh, &command, bin_name, GuardKind::Zsh, writer)
        }
        CompletionShell::Fish => {
            generate_guarded_aot(&Fish, &command, bin_name, GuardKind::Fish, writer)
        }
        CompletionShell::PowerShell => generate_powershell(&command, writer),
    }
}

#[derive(Clone, Copy)]
enum GuardKind {
    Bash,
    Zsh,
    Fish,
}

fn generate_guarded_aot<G: Generator>(
    generator: &G,
    command: &Command,
    bin_name: &str,
    kind: GuardKind,
    writer: &mut dyn Write,
) -> io::Result<()> {
    let mut generated = Vec::new();
    generator.try_generate(command, &mut generated)?;
    let guard_name = format!("_{bin_name}_before_passthrough");

    match kind {
        GuardKind::Bash => {
            write!(
                generated,
                "\n{guard_name}() {{\n\
                 \x20   local i\n\
                 \x20   for ((i = 1; i < COMP_CWORD; i++)); do\n\
                 \x20       if [[ \"${{COMP_WORDS[i]}}\" == \"--\" ]]; then\n\
                 \x20           COMPREPLY=()\n\
                 \x20           return 0\n\
                 \x20       fi\n\
                 \x20   done\n\
                 \x20   _{bin_name} \"$@\"\n\
                 }}\n\n\
                 if [[ \"${{BASH_VERSINFO[0]}}\" -eq 4 && \"${{BASH_VERSINFO[1]}}\" -ge 4 || \"${{BASH_VERSINFO[0]}}\" -gt 4 ]]; then\n\
                 \x20   complete -F {guard_name} -o nosort -o bashdefault -o default {bin_name}\n\
                 else\n\
                 \x20   complete -F {guard_name} -o bashdefault -o default {bin_name}\n\
                 fi\n"
            )?;
        }
        GuardKind::Zsh => {
            write!(
                generated,
                "\n{guard_name}() {{\n\
                 \x20   local index\n\
                 \x20   for ((index = 2; index < CURRENT; index++)); do\n\
                 \x20       if [[ \"${{words[index]}}\" == \"--\" ]]; then\n\
                 \x20           return 0\n\
                 \x20       fi\n\
                 \x20   done\n\
                 \x20   _{bin_name} \"$@\"\n\
                 }}\n\
                 compdef {guard_name} {bin_name}\n"
            )?;
        }
        GuardKind::Fish => {
            generated = guard_fish_completions(generated, bin_name, &guard_name)?;
        }
    }

    writer.write_all(&generated)
}

fn guard_fish_completions(
    generated: Vec<u8>,
    bin_name: &str,
    guard_name: &str,
) -> io::Result<Vec<u8>> {
    let generated = String::from_utf8(generated)
        .map_err(|error| io::Error::new(io::ErrorKind::InvalidData, error))?;
    let completion_prefix = format!("complete -c {bin_name} ");
    let guard_condition = format!(" -n '{guard_name}'");
    let mut guarded = format!(
        "function {guard_name}\n\
         \x20   not contains -- -- (commandline -opc)\n\
         end\n\n"
    );
    guarded.reserve(generated.len() + guard_condition.len() * generated.lines().count());

    for line in generated.split_inclusive('\n') {
        let (content, newline) = line
            .strip_suffix('\n')
            .map_or((line, ""), |content| (content, "\n"));
        guarded.push_str(content);
        if content.starts_with(&completion_prefix) {
            guarded.push_str(&guard_condition);
        }
        guarded.push_str(newline);
    }

    Ok(guarded.into_bytes())
}

/// Generates the metadata-aware static PowerShell integration missing from `clap_complete` 4.6.8.
fn generate_powershell(command: &Command, writer: &mut dyn Write) -> io::Result<()> {
    let bin_name = command
        .get_bin_name()
        .expect("the completion entry point sets the binary name");

    writer.write_all(
        b"using namespace System.Management.Automation\n\nRegister-ArgumentCompleter -Native -CommandName ",
    )?;
    write_powershell_literal(writer, bin_name)?;
    writer.write_all(
        b" -ScriptBlock {\n    param($wordToComplete, $commandAst, $cursorPosition)\n\n    $commandElements = @($commandAst.CommandElements)\n    $tokens = @($commandElements | ForEach-Object {\n        if ($_ -is [System.Management.Automation.Language.StringConstantExpressionAst]) {\n            $_.Value\n        } else {\n            $_.Extent.Text\n        }\n    })\n    if ($tokens -contains '--') {\n        return\n    }\n\n    $command = ",
    )?;
    write_powershell_literal(writer, bin_name)?;
    writer.write_all(b"\n    for ($i = 1; $i -lt $tokens.Count; $i++) {\n        $value = $tokens[$i]\n        if ($value -eq $wordToComplete -or $value.StartsWith('-')) {\n            break\n        }\n        $nextCommand = switch (\"$command;$value\") {\n")?;
    write_transition_cases(writer, command, bin_name)?;
    writer.write_all(
        b"            default { $null }\n        }\n        if ($null -eq $nextCommand) {\n            break\n        }\n        $command = $nextCommand\n    }\n\n    $option = $null\n    $valuePrefix = $wordToComplete\n    $completionPrefix = ''\n    if ($wordToComplete -match '^(--[^=]+)=(.*)$') {\n        $option = $Matches[1]\n        $valuePrefix = $Matches[2]\n        $completionPrefix = \"$option=\"\n    } else {\n        $currentIsElement = $tokens.Count -gt 0 -and $tokens[-1] -eq $wordToComplete\n        $previousIndex = if ($currentIsElement) { $tokens.Count - 2 } else { $tokens.Count - 1 }\n        if ($previousIndex -ge 1) {\n            $option = $tokens[$previousIndex]\n        }\n    }\n\n    $optionKey = if ($null -eq $option) { '' } else { \"$command;$option\" }\n    $values = switch ($optionKey) {\n",
    )?;
    write_option_value_cases(writer, command, bin_name)?;
    writer.write_all(b"        default { $null }\n    }\n    if ($null -eq $option) {\n        $values = switch ($command) {\n")?;
    write_positional_value_cases(writer, command, bin_name)?;
    writer.write_all(
        b"            default { $null }\n        }\n    }\n    if ($null -ne $values) {\n        @($values) |\n            Where-Object { $_.StartsWith($valuePrefix, [System.StringComparison]::OrdinalIgnoreCase) } |\n            Sort-Object |\n            ForEach-Object {\n                $completionText = \"$completionPrefix$_\"\n                [CompletionResult]::new($completionText, $_, [CompletionResultType]::ParameterValue, $_)\n            }\n        return\n    }\n\n    $pathKind = switch ($optionKey) {\n",
    )?;
    write_path_hint_cases(writer, command, bin_name)?;
    writer.write_all(
        b"        default { $null }\n    }\n    if ($null -ne $pathKind) {\n        $separatorIndex = [Math]::Max($valuePrefix.LastIndexOf('/'), $valuePrefix.LastIndexOf('\\'))\n        if ($separatorIndex -ge 0) {\n            $displayParent = $valuePrefix.Substring(0, $separatorIndex + 1)\n            $leaf = $valuePrefix.Substring($separatorIndex + 1)\n            $lookupParent = $displayParent\n        } else {\n            $displayParent = ''\n            $leaf = $valuePrefix\n            $lookupParent = '.'\n        }\n        $pattern = [System.Management.Automation.WildcardPattern]::Escape($leaf) + '*'\n        $pathExtensions = @($env:PATHEXT -split ';' | Where-Object { $_ } | ForEach-Object { $_.ToLowerInvariant() })\n        Get-ChildItem -LiteralPath $lookupParent -Force -ErrorAction SilentlyContinue |\n            Where-Object {\n                $_.Name -like $pattern -and\n                (($pathKind -ne 'directory') -or $_.PSIsContainer) -and\n                (($pathKind -ne 'executable') -or\n                    (-not $_.PSIsContainer -and\n                        ($pathExtensions.Count -eq 0 -or $pathExtensions -contains $_.Extension.ToLowerInvariant())))\n            } |\n            Sort-Object -Property Name |\n            ForEach-Object {\n                $candidate = $displayParent + $_.Name\n                if ($_.PSIsContainer) {\n                    $candidate += [System.IO.Path]::DirectorySeparatorChar\n                }\n                $completionText = if ($candidate -match '\\s') {\n                    \"'\" + $candidate.Replace(\"'\", \"''\") + \"'\"\n                } else {\n                    $candidate\n                }\n                $completionText = $completionPrefix + $completionText\n                [CompletionResult]::new($completionText, $candidate, [CompletionResultType]::ProviderItem, $candidate)\n            }\n        return\n    }\n\n    $candidates = switch ($command) {\n",
    )?;
    write_command_candidate_cases(writer, command, bin_name)?;
    writer.write_all(
        b"        default { @() }\n    }\n    @($candidates) |\n        Where-Object { $_.StartsWith($wordToComplete, [System.StringComparison]::OrdinalIgnoreCase) } |\n        Sort-Object |\n        ForEach-Object {\n            [CompletionResult]::new($_, $_, [CompletionResultType]::ParameterValue, $_)\n        }\n}\n",
    )
}

fn write_transition_cases(writer: &mut dyn Write, command: &Command, path: &str) -> io::Result<()> {
    for subcommand in command
        .get_subcommands()
        .filter(|command| !command.is_hide_set())
    {
        let child_path = format!("{path};{}", subcommand.get_name());
        for name in subcommand.get_name_and_visible_aliases() {
            writer.write_all(b"            ")?;
            write_powershell_literal(writer, &format!("{path};{name}"))?;
            writer.write_all(b" { ")?;
            write_powershell_literal(writer, &child_path)?;
            writer.write_all(b"; break }\n")?;
        }
        write_transition_cases(writer, subcommand, &child_path)?;
    }
    Ok(())
}

fn write_option_value_cases(
    writer: &mut dyn Write,
    command: &Command,
    path: &str,
) -> io::Result<()> {
    for argument in command
        .get_opts()
        .filter(|argument| !argument.is_hide_set())
    {
        let values = possible_values(argument);
        if values.is_empty() {
            continue;
        }
        for option in option_names(argument) {
            write_array_case(writer, 8, &format!("{path};{option}"), &values)?;
        }
    }
    for subcommand in command
        .get_subcommands()
        .filter(|command| !command.is_hide_set())
    {
        write_option_value_cases(
            writer,
            subcommand,
            &format!("{path};{}", subcommand.get_name()),
        )?;
    }
    Ok(())
}

fn write_positional_value_cases(
    writer: &mut dyn Write,
    command: &Command,
    path: &str,
) -> io::Result<()> {
    let values = command
        .get_positionals()
        .filter(|argument| !argument.is_hide_set())
        .flat_map(possible_values)
        .collect::<BTreeSet<_>>();
    if !values.is_empty() {
        write_array_case(writer, 12, path, &values)?;
    }
    for subcommand in command
        .get_subcommands()
        .filter(|command| !command.is_hide_set())
    {
        write_positional_value_cases(
            writer,
            subcommand,
            &format!("{path};{}", subcommand.get_name()),
        )?;
    }
    Ok(())
}

fn write_path_hint_cases(writer: &mut dyn Write, command: &Command, path: &str) -> io::Result<()> {
    for argument in command
        .get_opts()
        .filter(|argument| !argument.is_hide_set())
    {
        let kind = match argument.get_value_hint() {
            ValueHint::DirPath => Some("directory"),
            ValueHint::ExecutablePath => Some("executable"),
            _ => None,
        };
        let Some(kind) = kind else {
            continue;
        };
        for option in option_names(argument) {
            writer.write_all(b"        ")?;
            write_powershell_literal(writer, &format!("{path};{option}"))?;
            writer.write_all(b" { ")?;
            write_powershell_literal(writer, kind)?;
            writer.write_all(b"; break }\n")?;
        }
    }
    for subcommand in command
        .get_subcommands()
        .filter(|command| !command.is_hide_set())
    {
        write_path_hint_cases(
            writer,
            subcommand,
            &format!("{path};{}", subcommand.get_name()),
        )?;
    }
    Ok(())
}

fn write_command_candidate_cases(
    writer: &mut dyn Write,
    command: &Command,
    path: &str,
) -> io::Result<()> {
    let mut candidates = BTreeSet::new();
    for subcommand in command
        .get_subcommands()
        .filter(|command| !command.is_hide_set())
    {
        candidates.extend(
            subcommand
                .get_name_and_visible_aliases()
                .into_iter()
                .map(str::to_owned),
        );
    }
    for argument in command
        .get_arguments()
        .filter(|argument| !argument.is_positional() && !argument.is_hide_set())
    {
        candidates.extend(option_names(argument));
    }
    write_array_case(writer, 8, path, &candidates)?;

    for subcommand in command
        .get_subcommands()
        .filter(|command| !command.is_hide_set())
    {
        write_command_candidate_cases(
            writer,
            subcommand,
            &format!("{path};{}", subcommand.get_name()),
        )?;
    }
    Ok(())
}

fn option_names(argument: &Arg) -> BTreeSet<String> {
    let mut names = BTreeSet::new();
    if let Some(shorts) = argument.get_short_and_visible_aliases() {
        names.extend(shorts.into_iter().map(|short| format!("-{short}")));
    }
    if let Some(longs) = argument.get_long_and_visible_aliases() {
        names.extend(longs.into_iter().map(|long| format!("--{long}")));
    }
    names
}

fn possible_values(argument: &Arg) -> BTreeSet<String> {
    argument
        .get_possible_values()
        .into_iter()
        .filter(|value| !value.is_hide_set())
        .map(|value| value.get_name().to_owned())
        .collect()
}

fn write_array_case(
    writer: &mut dyn Write,
    indentation: usize,
    key: &str,
    values: &BTreeSet<String>,
) -> io::Result<()> {
    for _ in 0..indentation {
        writer.write_all(b" ")?;
    }
    write_powershell_literal(writer, key)?;
    writer.write_all(b" { @(")?;
    for (index, value) in values.iter().enumerate() {
        if index != 0 {
            writer.write_all(b", ")?;
        }
        write_powershell_literal(writer, value)?;
    }
    writer.write_all(b"); break }\n")
}

fn write_powershell_literal(writer: &mut dyn Write, value: &str) -> io::Result<()> {
    writer.write_all(b"'")?;
    let mut parts = value.split('\'');
    if let Some(first) = parts.next() {
        writer.write_all(first.as_bytes())?;
    }
    for part in parts {
        writer.write_all(b"''")?;
        writer.write_all(part.as_bytes())?;
    }
    writer.write_all(b"'")
}
