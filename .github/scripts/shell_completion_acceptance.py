#!/usr/bin/env python3
"""Exercise generated completions in isolated native shell processes."""

from __future__ import annotations

import argparse
import errno
import json
import os
import platform
import re
import shlex
import shutil
import subprocess
import sys
import tempfile
import time
from dataclasses import dataclass
from pathlib import Path
from typing import Iterable, Sequence

if os.name != "nt":
    import fcntl
    import pty
    import select
    import struct
    import termios

PROMPT = "SKILLMOUNT_PROMPT> "
SHELL_ORDER = ("bash", "zsh", "fish", "powershell")
CASE_ORDER = (
    "syntax",
    "subcommands",
    "invalid-subcommand-prefix",
    "options",
    "wrapper-enums",
    "wrapper-enum-prefix",
    "invalid-enum-prefix",
    "directory-hint",
    "non-directory-hint",
    "executable-hint",
    "executable-scope",
    "non-executable-hint",
    "opaque-passthrough",
)
BASH_CASE_ORDER = CASE_ORDER + ("literal-executable",)
POWERSHELL_CASE_ORDER = CASE_ORDER + (
    "empty-passthrough",
    "quoted-directory-hint",
    "quoted-passthrough",
    "cursor-before-passthrough",
)
ANSI_ESCAPE = re.compile(
    rb"\x1b(?:\[[0-?]*[ -/]*[@-~]|\][^\x07]*(?:\x07|\x1b\\)|[@-_])"
)
ENUM_CANDIDATES = (
    "auto",
    "symlink",
    "junction",
    "project",
    "staging",
    "error",
    "skip",
    "basic",
    "strict",
    "none",
)
SESSION_OPTION_CANDIDATES = (
    "--skills-dir",
    "--cwd",
    "--project-root",
    "--agent-bin",
    "--link-mode",
    "--mount-mode",
    "--conflict",
    "--validation",
    "--dry-run",
    "--keep-mounts",
    "--no-recover",
    "--verbose",
    "--help",
    "--version",
)
SHELL_ERROR_MARKERS = ("unknown match specification", "compopt:")


class AcceptanceError(RuntimeError):
    """A required native-shell observation could not be proved."""


@dataclass(frozen=True)
class CompletionCase:
    name: str
    line: str
    expected: tuple[str, ...]
    forbidden: tuple[str, ...] = ()
    cursor_position: int | None = None
    tabs: int = 2
    completed: str | None = None
    required: tuple[str, ...] = ()


@dataclass(frozen=True)
class ShellInstallation:
    command: tuple[str, ...]
    environment: dict[str, str]
    script: Path


class Fixture:
    """One owned temporary tree; cleanup cannot reach a sibling path."""

    def __init__(self, shell: str, product: str, parent: Path | None = None) -> None:
        self._temporary = tempfile.TemporaryDirectory(
            prefix=f"skillmount-completion-{shell}-{product}-",
            dir=parent,
        )
        self.root = Path(self._temporary.name)
        self.home = self.root / "home"
        self.work = self.root / "work"
        self.home.mkdir()
        self.work.mkdir()
        for name in ("alpha-directory", "alpha-second-directory"):
            (self.work / name).mkdir()
        (self.work / "alpha-file").write_text("not a directory\n", encoding="utf-8")
        (self.work / "agent-nested").mkdir()
        (self.work / "agent-data.txt").write_text("not executable\n", encoding="utf-8")
        (self.work / "--link-mode").write_text("opaque agent value\n", encoding="utf-8")
        if shell == "bash":
            dangerous = self.work / "danger;id"
            dangerous.write_text("#!/bin/sh\nexit 0\n", encoding="utf-8")
            dangerous.chmod(0o755)
        executable_names = (
            ("agent-probe.exe", "agent-second.exe", "agent-;literal.exe")
            if shell == "powershell"
            else ("agent-probe", "agent-second")
        )
        for name in executable_names:
            candidate = self.work / name
            candidate.write_bytes(b"MZ" if shell == "powershell" else b"#!/bin/sh\nexit 0\n")
            if shell != "powershell":
                candidate.chmod(0o755)

    def __enter__(self) -> Fixture:
        return self

    def __exit__(self, exc_type: object, exc_value: object, traceback: object) -> None:
        self._temporary.cleanup()


def completion_cases(
    product: str, shell: str, work: Path | None = None
) -> tuple[CompletionCase, ...]:
    executable_names = (
        ("agent-probe.exe", "agent-second.exe", "agent-`;literal.exe")
        if shell == "powershell"
        else ("agent-probe", "agent-second")
    )
    executable_names += ("agent-nested",)
    executable_forbidden = ("agent-data.txt",)
    if shell == "powershell":
        executable_forbidden += ("agent-;literal.exe",)
    non_executable_prefix = (Path(".") if work is None else work) / "agent-data"
    executable_prefix = (Path(".") if work is None else work) / "agent-"
    executable_scope_names = (
        executable_names
        + ("alpha-directory", "alpha-second-directory")
        + (("danger;id",) if shell == "bash" else ())
    )
    no_match_required = ("alpha-f",) if shell == "powershell" else ()
    cases = (
        CompletionCase(
            "subcommands",
            f"{product} c",
            ("claude", "cleanup", "codex", "completions"),
        ),
        CompletionCase(
            "invalid-subcommand-prefix",
            f"{product} alpha-f",
            (),
            ("alpha-file",),
            required=no_match_required,
        ),
        CompletionCase(
            "options",
            f"{product} codex --pro",
            ("--project-root",),
            tabs=1,
            completed=(
                f"{product} codex --project-root="
                if shell == "zsh"
                else f"{product} codex --project-root "
            ),
        ),
        CompletionCase(
            "wrapper-enums",
            f"{product} codex --skills-dir alpha-directory --link-mode ",
            ("auto", "junction", "symlink"),
            tuple(
                value
                for value in ENUM_CANDIDATES
                if value not in {"auto", "junction", "symlink"}
            ),
        ),
        CompletionCase(
            "wrapper-enum-prefix",
            f"{product} codex --skills-dir alpha-directory --link-mode s",
            ("symlink",),
            forbidden=tuple(
                value for value in ENUM_CANDIDATES if value != "symlink"
            ),
            tabs=1,
            completed=(
                f"{product} codex --skills-dir alpha-directory --link-mode symlink "
            ),
        ),
        CompletionCase(
            "invalid-enum-prefix",
            f"{product} codex --link-mode alpha-f",
            (),
            ("alpha-file",),
            required=no_match_required,
        ),
        CompletionCase(
            "directory-hint",
            f"{product} codex --skills-dir alpha-",
            ("alpha-directory", "alpha-second-directory"),
            ("alpha-file",),
        ),
        CompletionCase(
            "non-directory-hint",
            f"{product} codex --skills-dir alpha-f",
            (),
            ("alpha-file",),
            required=no_match_required,
        ),
        CompletionCase(
            "executable-hint",
            f"{product} codex --agent-bin {executable_prefix}",
            executable_names,
            executable_forbidden,
        ),
        CompletionCase(
            "executable-scope",
            f"{product} codex --agent-bin ",
            executable_scope_names,
            executable_forbidden + ("printf",),
        ),
        CompletionCase(
            "non-executable-hint",
            f"{product} codex --agent-bin {non_executable_prefix}",
            (),
            ("agent-data.txt",),
            required=(str(non_executable_prefix),) if shell == "powershell" else (),
        ),
        CompletionCase(
            "opaque-passthrough",
            f"{product} codex -- --",
            (),
            SESSION_OPTION_CANDIDATES,
            required=("--",) if shell == "powershell" else (),
        ),
    )
    if shell == "bash":
        return cases + (
            CompletionCase(
                "literal-executable",
                f"{product} codex --agent-bin danger",
                (r"danger\;id",),
                ("danger;id",),
                tabs=1,
                completed=f"{product} codex --agent-bin danger\\;id ",
            ),
        )
    if shell != "powershell":
        return cases

    before_passthrough = f"{product} codex --pro"
    return cases + (
        CompletionCase(
            "empty-passthrough",
            f"{product} codex -- ",
            (),
            SESSION_OPTION_CANDIDATES,
            required=(" \n",),
        ),
        CompletionCase(
            "quoted-directory-hint",
            f"{product} codex --skills-dir 'alpha",
            ("alpha-directory", "alpha-second-directory"),
            ("alpha-file",),
        ),
        CompletionCase(
            "quoted-passthrough",
            f'{product} codex "--" --li',
            (),
            ("--link-mode",),
            required=("--li",),
        ),
        CompletionCase(
            "cursor-before-passthrough",
            f"{before_passthrough} -- --later",
            ("--project-root",),
            cursor_position=len(before_passthrough),
        ),
    )


def emit(record: dict[str, object]) -> None:
    print(json.dumps(record, sort_keys=True, separators=(",", ":")))


def observation_record(
    shell: str, product: str, case: str, candidates: Iterable[str]
) -> dict[str, object]:
    return {
        "candidates": sorted(set(candidates)),
        "case": case,
        "product": product,
        "shell": shell,
        "status": "pass",
    }


def require_interpreter(shell: str) -> tuple[str, str]:
    executable_name = "pwsh" if shell == "powershell" else shell
    executable = shutil.which(executable_name)
    if executable is None:
        raise AcceptanceError(
            f"required-interpreter: advertised shell {shell!r} is unavailable"
        )
    result = subprocess.run(
        [executable, "--version"],
        check=False,
        stdout=subprocess.PIPE,
        stderr=subprocess.PIPE,
        timeout=15,
    )
    if result.returncode != 0:
        raise AcceptanceError(
            f"required-interpreter: {shell} version probe failed with "
            f"exit {result.returncode}: {decode(result.stderr)}"
        )
    version = decode(result.stdout or result.stderr).strip().splitlines()[0]
    return executable, version


def generate_script(binary: Path, shell: str, environment: dict[str, str]) -> bytes:
    result = subprocess.run(
        [str(binary), "completions", shell],
        check=False,
        stdout=subprocess.PIPE,
        stderr=subprocess.PIPE,
        env=environment,
        timeout=30,
    )
    if result.returncode != 0 or result.stderr or not result.stdout:
        raise AcceptanceError(
            f"{binary.name} could not generate {shell} completion: "
            f"exit={result.returncode} stderr={decode(result.stderr)!r}"
        )
    return result.stdout


def isolated_environment(fixture: Fixture) -> dict[str, str]:
    environment = os.environ.copy()
    environment.update(
        {
            "HOME": str(fixture.home),
            "USERPROFILE": str(fixture.home),
            "XDG_CONFIG_HOME": str(fixture.home),
            "ZDOTDIR": str(fixture.home),
            "TERM": "xterm-256color",
        }
    )
    return environment


def install_completion(
    shell: str,
    product: str,
    script: bytes,
    fixture: Fixture,
    interpreter: str,
    binary_root: Path | None = None,
) -> ShellInstallation:
    environment = isolated_environment(fixture)
    if binary_root is not None:
        environment["PATH"] = (
            str(binary_root) + os.pathsep + environment.get("PATH", "")
        )
    generated = fixture.home / "generated"
    generated.mkdir()

    if shell == "bash":
        script_path = generated / f"{product}.bash"
        script_path.write_bytes(script)
        config = fixture.home / ".bashrc"
        config.write_text(
            f"PS1={shlex.quote(PROMPT)}\n"
            "PROMPT_COMMAND=\n"
            f"source {shlex.quote(str(script_path))}\n",
            encoding="utf-8",
        )
        command = (interpreter, "--noprofile", "--rcfile", str(config), "-i")
    elif shell == "zsh":
        functions = fixture.home / "zsh-functions"
        functions.mkdir()
        script_path = functions / f"_{product}"
        script_path.write_bytes(script)
        config = fixture.home / ".zshenv"
        config.write_text(
            f"fpath=({shlex.quote(str(functions))} $fpath)\n"
            "autoload -U +X compinit && compinit -u -d $ZDOTDIR/.zcompdump\n"
            "precmd_functions=\"\"\n"
            f"PS1={shlex.quote(PROMPT)}\n"
            f"PROMPT={shlex.quote(PROMPT)}\n",
            encoding="utf-8",
        )
        command = (interpreter, "--noglobalrcs", "-i")
    elif shell == "fish":
        script_path = fixture.home / "fish" / "completions" / f"{product}.fish"
        script_path.parent.mkdir(parents=True)
        script_path.write_bytes(script)
        config = fixture.home / "fish" / "config.fish"
        config.write_text(
            "set -g fish_greeting ''\n"
            "set -g fish_autosuggestion_enabled 0\n"
            "function fish_title\nend\n"
            "function fish_prompt\n"
            f"    printf {shlex.quote(PROMPT)}\n"
            "end\n",
            encoding="utf-8",
        )
        command = (interpreter, "--interactive")
    elif shell == "powershell":
        script_path = generated / f"{product}.ps1"
        script_path.write_bytes(script)
        command = (interpreter, "-NoLogo", "-NoProfile", "-NonInteractive")
    else:
        raise AcceptanceError(f"unsupported harness shell {shell!r}")

    return ShellInstallation(command, environment, script_path)


def syntax_check(shell: str, installation: ShellInstallation) -> None:
    if shell == "bash" or shell == "zsh":
        command = [installation.command[0], "-n", str(installation.script)]
    elif shell == "fish":
        command = [installation.command[0], "--no-execute", str(installation.script)]
    else:
        command = [
            installation.command[0],
            "-NoLogo",
            "-NoProfile",
            "-NonInteractive",
            "-Command",
            (
                "$tokens=$null; $errors=$null; "
                "[System.Management.Automation.Language.Parser]::ParseFile("
                "$env:SKILLMOUNT_COMPLETION_SCRIPT,[ref]$tokens,[ref]$errors) > $null; "
                "if ($errors.Count -ne 0) { $errors | Out-String | Write-Error; exit 1 }"
            ),
        ]
    environment = installation.environment.copy()
    environment["SKILLMOUNT_COMPLETION_SCRIPT"] = str(installation.script)
    result = subprocess.run(
        command,
        check=False,
        stdout=subprocess.PIPE,
        stderr=subprocess.PIPE,
        env=environment,
        timeout=30,
    )
    if result.returncode != 0:
        raise AcceptanceError(
            f"{shell} syntax check failed: {decode(result.stderr or result.stdout)}"
        )


def decode(value: bytes) -> str:
    return value.decode("utf-8", errors="replace")


def normalized_terminal_output(value: bytes) -> str:
    value = ANSI_ESCAPE.sub(b"", value)
    output: list[str] = []
    for character in decode(value):
        if character == "\b":
            if output:
                output.pop()
        elif character == "\r":
            output.append("\n")
        elif character == "\a" or (ord(character) < 32 and character not in "\n\t"):
            continue
        else:
            output.append(character)
    return "".join(output)


def _read_available(master: int, timeout: float) -> bytes:
    ready, _, _ = select.select([master], [], [], timeout)
    if not ready:
        return b""
    try:
        return os.read(master, 65536)
    except OSError as error:
        if error.errno == errno.EIO:
            return b""
        raise


def _wait_for_prompt(master: int, process: subprocess.Popen[bytes]) -> bytes:
    observed = bytearray()
    deadline = time.monotonic() + 12
    marker = PROMPT.encode()
    while time.monotonic() < deadline:
        chunk = _read_available(master, 0.2)
        observed.extend(chunk)
        if marker in observed:
            return bytes(observed)
        if process.poll() is not None:
            break
    raise AcceptanceError(
        "interactive shell did not reach the isolated prompt: "
        + normalized_terminal_output(bytes(observed))
    )


def _collect_completion(master: int) -> bytes:
    observed = bytearray()
    started = time.monotonic()
    last_data = started
    while time.monotonic() - started < 4:
        chunk = _read_available(master, 0.1)
        if chunk:
            observed.extend(chunk)
            last_data = time.monotonic()
        now = time.monotonic()
        if now - started >= 0.7 and now - last_data >= 0.3:
            break
    return bytes(observed)


def interactive_completion(
    installation: ShellInstallation, fixture: Fixture, line: str, tabs: int
) -> str:
    master, slave = pty.openpty()
    fcntl.ioctl(slave, termios.TIOCSWINSZ, struct.pack("HHHH", 60, 512, 0, 0))

    def configure_controlling_terminal() -> None:
        os.setsid()
        fcntl.ioctl(slave, termios.TIOCSCTTY, 0)

    process = subprocess.Popen(
        installation.command,
        cwd=fixture.work,
        env=installation.environment,
        stdin=slave,
        stdout=slave,
        stderr=slave,
        close_fds=True,
        preexec_fn=configure_controlling_terminal,
    )
    os.close(slave)
    try:
        _wait_for_prompt(master, process)
        os.write(master, line.encode() + b"\t" * tabs)
        return normalized_terminal_output(_collect_completion(master))
    finally:
        if process.poll() is None:
            process.kill()
            process.wait(timeout=2)
        os.close(master)


FISH_COMPLETE = """
source "$SKILLMOUNT_COMPLETION_SCRIPT"
cd "$SKILLMOUNT_COMPLETION_WORK"
complete --do-complete="$SKILLMOUNT_COMPLETION_LINE"
"""


def fish_completion(
    installation: ShellInstallation, fixture: Fixture, line: str
) -> str:
    environment = installation.environment.copy()
    environment.update(
        {
            "SKILLMOUNT_COMPLETION_SCRIPT": str(installation.script),
            "SKILLMOUNT_COMPLETION_WORK": str(fixture.work),
            "SKILLMOUNT_COMPLETION_LINE": line,
        }
    )
    result = subprocess.run(
        [installation.command[0], "--no-config", "--command", FISH_COMPLETE],
        check=False,
        stdout=subprocess.PIPE,
        stderr=subprocess.PIPE,
        env=environment,
        timeout=30,
    )
    if result.returncode != 0:
        raise AcceptanceError(
            f"Fish completion failed: {decode(result.stderr or result.stdout)}"
        )
    return decode(result.stdout)


POWERSHELL_COMPLETE = r"""
$ErrorActionPreference = 'Stop'
. $env:SKILLMOUNT_COMPLETION_SCRIPT
Set-Location -LiteralPath $env:SKILLMOUNT_COMPLETION_WORK
$line = $env:SKILLMOUNT_COMPLETION_LINE
$cursorPosition = if ($env:SKILLMOUNT_COMPLETION_CURSOR) {
    [int]$env:SKILLMOUNT_COMPLETION_CURSOR
} else {
    $line.Length
}
$completion = [System.Management.Automation.CommandCompletion]::CompleteInput(
    $line, $cursorPosition, $null
)
$completion.CompletionMatches | ForEach-Object { $_.CompletionText }
"""


def powershell_completion(
    installation: ShellInstallation,
    fixture: Fixture,
    line: str,
    cursor_position: int | None = None,
) -> str:
    environment = installation.environment.copy()
    environment.update(
        {
            "SKILLMOUNT_COMPLETION_SCRIPT": str(installation.script),
            "SKILLMOUNT_COMPLETION_WORK": str(fixture.work),
            "SKILLMOUNT_COMPLETION_LINE": line,
            "SKILLMOUNT_COMPLETION_CURSOR": (
                "" if cursor_position is None else str(cursor_position)
            ),
        }
    )
    result = subprocess.run(
        [*installation.command, "-Command", POWERSHELL_COMPLETE],
        check=False,
        stdout=subprocess.PIPE,
        stderr=subprocess.PIPE,
        env=environment,
        timeout=30,
    )
    if result.returncode != 0:
        raise AcceptanceError(
            f"PowerShell completion failed: {decode(result.stderr or result.stdout)}"
        )
    return decode(result.stdout)


def normalize_candidate(value: str) -> str:
    candidate = value.split("\t", 1)[0].rstrip("\r\n")
    if candidate == " ":
        return candidate
    candidate = candidate.strip().removesuffix("*").rstrip("/\\")
    if "/" in candidate:
        candidate = candidate.rsplit("/", 1)[-1]
    elif re.match(r"^(?:[A-Za-z]:\\|\\\\)", candidate):
        candidate = candidate.rsplit("\\", 1)[-1]
    return candidate


def machine_candidates(observed: str) -> set[str]:
    return {
        candidate
        for line in observed.splitlines()
        if (candidate := normalize_candidate(line))
    }


def menu_candidates(observed: str, product: str) -> set[str]:
    blocks: list[list[str]] = []
    block: list[str] = []
    for line in observed.splitlines():
        if line.strip():
            block.append(line)
        elif block:
            blocks.append(block)
            block = []
    if block:
        blocks.append(block)
    if len(blocks) < 2:
        return set()

    candidates: set[str] = set()
    for candidate_block in blocks[1:]:
        for line in candidate_block:
            if (
                line == product
                or line.startswith(f"{product} ")
                or line.startswith(PROMPT)
            ):
                continue
            candidate_text = line.split(" -- ", 1)[0]
            for value in candidate_text.split():
                if candidate := normalize_candidate(value):
                    candidates.add(candidate)
    return candidates


def verify_case(shell: str, case: CompletionCase, observed: str) -> list[str]:
    expected = {normalize_candidate(candidate) for candidate in case.expected}
    required = {normalize_candidate(text) for text in case.required}
    diagnostics = [marker for marker in SHELL_ERROR_MARKERS if marker in observed]

    if shell == "fish" or shell == "powershell":
        actual = machine_candidates(observed)
        missing = sorted(expected - actual)
        missing_required = sorted(required - actual)
        unexpected = sorted(actual - expected - required)
    elif not expected:
        actual = set()
        missing = []
        missing_required = [
            text for text in case.required if text not in observed
        ]
        unexpected = [] if observed == case.line else [observed]
    elif len(expected) == 1 and case.tabs == 1:
        if case.completed is None:
            raise AcceptanceError(
                f"case {case.name!r} omitted its exact completed command line"
            )
        candidate = next(iter(expected))
        actual = {candidate} if observed == case.completed else set()
        missing = sorted(expected - actual)
        missing_required = [
            text for text in case.required if text not in observed
        ]
        unexpected = [] if observed == case.completed else [observed]
    else:
        actual = menu_candidates(observed, case.line.split(" ", 1)[0])
        missing = sorted(expected - actual)
        missing_required = [
            text for text in case.required if text not in observed
        ]
        unexpected = sorted(actual - expected)

    normalized_forbidden = {normalize_candidate(value) for value in case.forbidden}
    forbidden = sorted(
        (actual & normalized_forbidden)
        | {value for value in case.forbidden if value in observed}
    )
    if missing or missing_required or unexpected or forbidden or diagnostics:
        raise AcceptanceError(
            f"case {case.name!r} failed: missing={missing!r} "
            f"missing_required={missing_required!r} unexpected={unexpected!r} "
            f"forbidden={forbidden!r} diagnostics={diagnostics!r} "
            f"actual={sorted(actual)!r} observed={observed!r}"
        )
    return sorted(actual)


def verify_binary(binary: Path) -> tuple[Path, str]:
    binary = binary.resolve(strict=True)
    result = subprocess.run(
        [str(binary), "--version"],
        check=False,
        stdout=subprocess.PIPE,
        stderr=subprocess.PIPE,
        timeout=15,
    )
    if result.returncode != 0:
        raise AcceptanceError(
            f"{binary} version probe failed: {decode(result.stderr or result.stdout)}"
        )
    return binary, decode(result.stdout or result.stderr).strip()


def run_shell(binary: Path, product: str, shell: str) -> None:
    interpreter, version = require_interpreter(shell)
    emit(
        {
            "case": "required-interpreter",
            "shell": shell,
            "status": "pass",
            "version": version,
        }
    )
    with Fixture(shell, product) as fixture:
        environment = isolated_environment(fixture)
        script = generate_script(binary, shell, environment)
        installation = install_completion(
            shell, product, script, fixture, interpreter, binary.parent
        )
        syntax_check(shell, installation)
        emit(observation_record(shell, product, "syntax", ()))
        for case in completion_cases(product, shell, fixture.work):
            if shell == "fish":
                observed = fish_completion(installation, fixture, case.line)
            elif shell == "powershell":
                observed = powershell_completion(
                    installation, fixture, case.line, case.cursor_position
                )
            else:
                observed = interactive_completion(
                    installation, fixture, case.line, case.tabs
                )
            candidates = verify_case(shell, case, observed)
            emit(observation_record(shell, product, case.name, candidates))


def parse_args(arguments: Sequence[str]) -> argparse.Namespace:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--asm", required=True, type=Path)
    parser.add_argument("--skillmount", required=True, type=Path)
    parser.add_argument(
        "--target",
        required=True,
        choices=(
            "aarch64-apple-darwin",
            "x86_64-pc-windows-msvc",
            "i686-pc-windows-msvc",
        ),
    )
    parser.add_argument(
        "--shell",
        required=True,
        action="append",
        choices=SHELL_ORDER,
        dest="shells",
    )
    return parser.parse_args(arguments)


def main(arguments: Sequence[str] | None = None) -> int:
    options = parse_args(sys.argv[1:] if arguments is None else arguments)
    requested = tuple(shell for shell in SHELL_ORDER if shell in options.shells)
    if len(options.shells) != len(set(options.shells)):
        raise AcceptanceError("each --shell value must be supplied exactly once")
    if os.name == "nt" and requested != ("powershell",):
        raise AcceptanceError("native Windows acceptance supports only powershell")
    if os.name != "nt" and "powershell" in requested:
        raise AcceptanceError("powershell acceptance must run on native Windows")

    asm, asm_version = verify_binary(options.asm)
    skillmount, skillmount_version = verify_binary(options.skillmount)
    if asm_version != skillmount_version:
        raise AcceptanceError(
            f"product version mismatch: asm={asm_version!r} skillmount={skillmount_version!r}"
        )
    emit(
        {
            "case": "environment",
            "platform": platform.platform(),
            "products": ["asm", "skillmount"],
            "revision": os.environ.get("GITHUB_SHA", "working-tree"),
            "shells": list(requested),
            "target": options.target,
            "skillmount": asm_version,
            "status": "pass",
        }
    )

    for shell in requested:
        for binary, product in ((asm, "asm"), (skillmount, "skillmount")):
            run_shell(binary, product, shell)
    return 0


if __name__ == "__main__":
    try:
        raise SystemExit(main())
    except (AcceptanceError, OSError, subprocess.SubprocessError) as error:
        print(f"shell completion acceptance failed: {error}", file=sys.stderr)
        raise SystemExit(1) from error
