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
    "options",
    "wrapper-enums",
    "directory-hint",
    "executable-hint",
    "opaque-passthrough",
)
ANSI_ESCAPE = re.compile(
    rb"\x1b(?:\[[0-?]*[ -/]*[@-~]|\][^\x07]*(?:\x07|\x1b\\)|[@-_])"
)


class AcceptanceError(RuntimeError):
    """A required native-shell observation could not be proved."""


@dataclass(frozen=True)
class CompletionCase:
    name: str
    line: str
    expected: tuple[str, ...]
    forbidden: tuple[str, ...] = ()


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
        executable_names = (
            ("agent-probe.exe", "agent-second.exe")
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
        ("agent-probe.exe", "agent-second.exe")
        if shell == "powershell"
        else ("agent-probe", "agent-second")
    )
    executable_prefix = (Path(".") if work is None else work) / "agent-"
    return (
        CompletionCase(
            "subcommands",
            f"{product} c",
            ("claude", "cleanup", "codex", "completions"),
        ),
        CompletionCase("options", f"{product} codex --pro", ("--project-root",)),
        CompletionCase(
            "wrapper-enums",
            f"{product} codex --skills-dir alpha-directory --link-mode ",
            ("auto", "junction", "symlink"),
        ),
        CompletionCase(
            "directory-hint",
            f"{product} codex --skills-dir alpha-",
            ("alpha-directory", "alpha-second-directory"),
        ),
        CompletionCase(
            "executable-hint",
            f"{product} codex --agent-bin {executable_prefix}",
            executable_names,
        ),
        CompletionCase(
            "opaque-passthrough",
            f"{product} codex --skills-dir alpha-directory -- --li",
            (),
            ("--link-mode",),
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
    installation: ShellInstallation, fixture: Fixture, line: str
) -> str:
    master, slave = pty.openpty()
    fcntl.ioctl(slave, termios.TIOCSWINSZ, struct.pack("HHHH", 60, 120, 0, 0))

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
        os.write(master, line.encode() + b"\t\t")
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
$completion = [System.Management.Automation.CommandCompletion]::CompleteInput(
    $line, $line.Length, $null
)
$completion.CompletionMatches | ForEach-Object { $_.CompletionText }
"""


def powershell_completion(
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


def verify_case(case: CompletionCase, observed: str) -> list[str]:
    missing = [candidate for candidate in case.expected if candidate not in observed]
    forbidden = [candidate for candidate in case.forbidden if candidate in observed]
    if missing or forbidden:
        raise AcceptanceError(
            f"case {case.name!r} failed: missing={missing!r} forbidden={forbidden!r} "
            f"observed={observed!r}"
        )
    return [candidate for candidate in case.expected if candidate in observed]


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
                observed = powershell_completion(installation, fixture, case.line)
            else:
                observed = interactive_completion(installation, fixture, case.line)
            candidates = verify_case(case, observed)
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
