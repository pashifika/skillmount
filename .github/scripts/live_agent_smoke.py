#!/usr/bin/env python3
"""Run opt-in real-agent Skill discovery smokes and retain credential-safe evidence.

Agent installation is deliberately outside this harness. The workflow supplies native binaries
from integrity-locked packages, while this process gives each agent only its own credential and
never writes an unredacted child stream to disk.

Every agent is one `AgentCase` record. An opt-in case is selected only by an explicit binary path;
a case this run deliberately did not exercise is recorded as `unknown` with a reason and never
grades the run, while `unverified` stays reserved for a case that ran without producing a
compatibility observation.
"""

from __future__ import annotations

import argparse
import datetime as dt
import hashlib
import json
import os
import platform
import shutil
import signal
import subprocess
import sys
import tempfile
from dataclasses import dataclass
from pathlib import Path
from typing import Iterator

BASE_TOKEN = "SKILLMOUNT_LIVE_BASE"
WINNER_TOKEN = "SKILLMOUNT_LIVE_WINNER_3"
EXPECTED_RESPONSE = f"{BASE_TOKEN} {WINNER_TOKEN}"
PROMPT_TOKEN = "{skillmount-live-prompt}"
LOCKED_INTEGRITY = (
    "npm package pinned to a committed SRI digest in prepare_live_agents.py and rebound to this run "
    "by the agent supply-chain manifest"
)
OMP_INTEGRITY = (
    "GitHub release asset of tag v17.2.9 recorded by its observed sha256 only: OMP publishes no "
    "SHA256SUMS.txt for 17.2.9, so this case cannot be integrity-locked against a published digest "
    "file the way the npm-packaged agents are"
)
WINDOWS_JOB_BOOTSTRAP = "--_skillmount-windows-job-bootstrap"
SENSITIVE_NAME_FRAGMENTS = (
    "API_KEY",
    "AUTH_TOKEN",
    "ACCESS_TOKEN",
    "CREDENTIAL",
    "PASSWORD",
    "PRIVATE_KEY",
    "SECRET",
)


@dataclass(frozen=True)
class CommandResult:
    returncode: int | None
    stdout: str
    stderr: str
    timed_out: bool


@dataclass(frozen=True)
class AgentCase:
    """How one agent is launched, read, gated, and recorded by this harness.

    An `opt_in` case is absent from the committed supply-chain manifest, is selected only by an
    explicit `--<name>-bin`, and is reported as `unknown` rather than graded when it is skipped.
    `unsupported_targets` maps a wrapper target with no published agent asset to the reason it
    cannot be exercised there.
    """

    name: str
    executable: str
    banner: str
    credential_name: str
    destination: str
    passthrough: tuple[str, ...]
    text_response: bool
    integrity: str
    opt_in: bool = False
    unsupported_targets: tuple[tuple[str, str], ...] = ()


AGENT_CASES: dict[str, AgentCase] = {
    case.name: case
    for case in (
        AgentCase(
            name="codex",
            executable="codex",
            banner="codex-cli 0.146.0",
            credential_name="CODEX_API_KEY",
            destination=".agents/skills",
            passthrough=("exec", "--skip-git-repo-check", "--json", PROMPT_TOKEN),
            text_response=False,
            integrity=LOCKED_INTEGRITY,
        ),
        AgentCase(
            name="claude",
            executable="claude",
            banner="2.1.220 (Claude Code)",
            credential_name="ANTHROPIC_API_KEY",
            destination=".claude/skills",
            passthrough=("-p", PROMPT_TOKEN, "--output-format", "text"),
            text_response=True,
            integrity=LOCKED_INTEGRITY,
        ),
        AgentCase(
            name="omp",
            executable="omp",
            banner="omp/17.2.9",
            credential_name="ANTHROPIC_API_KEY",
            destination=".omp/skills",
            # `--print` keeps the session headless, `--mode text` makes the answer one plain
            # response, `--no-session` keeps the transcript out of the runner's home, and
            # `--auto-approve` stops a tool approval prompt from stalling a non-interactive run.
            passthrough=(
                "--print",
                "--mode",
                "text",
                "--no-session",
                "--auto-approve",
                PROMPT_TOKEN,
            ),
            text_response=True,
            integrity=OMP_INTEGRITY,
            opt_in=True,
            unsupported_targets=(
                ("i686-pc-windows-msvc", "no 32-bit OMP asset is published for 17.2.9"),
            ),
        ),
    )
}


def parse_args() -> argparse.Namespace:
    parser = argparse.ArgumentParser()
    parser.add_argument("--asm", required=True, type=Path)
    parser.add_argument("--link-mode", required=True, choices=("symlink", "junction"))
    parser.add_argument("--wrapper-target", required=True)
    parser.add_argument("--evidence-dir", required=True, type=Path)
    parser.add_argument("--agent-manifest", required=True, type=Path)
    parser.add_argument("--codex-bin", type=Path)
    parser.add_argument("--claude-bin", type=Path)
    parser.add_argument("--omp-bin", type=Path)
    return parser.parse_args()


def executable(name: str) -> Path:
    found = shutil.which(name)
    if found is None:
        raise RuntimeError(f"{name} is not installed on PATH")
    return Path(found).resolve()


def agent_executable(explicit: Path | None, name: str) -> Path:
    binary = explicit.resolve(strict=True) if explicit is not None else executable(name)
    if not binary.is_file():
        raise RuntimeError(f"{binary} is not a regular {name} executable")
    if os.name == "nt" and binary.suffix.lower() in (".bat", ".cmd"):
        raise RuntimeError(
            f"{binary} is a command shim; pass the native {name}.exe so SkillMount can launch without cmd.exe"
        )
    return binary


def is_sensitive_name(name: str) -> bool:
    upper = name.upper()
    return upper.endswith("_TOKEN") or any(
        fragment in upper for fragment in SENSITIVE_NAME_FRAGMENTS
    )


def split_environment(source: dict[str, str]) -> tuple[dict[str, str], dict[str, str]]:
    secrets = {
        name: value
        for name, value in source.items()
        if value and is_sensitive_name(name)
    }
    clean = {name: value for name, value in source.items() if name not in secrets}
    return clean, secrets


def redact(text: str, secrets: dict[str, str]) -> str:
    redacted = text
    for name, value in sorted(secrets.items(), key=lambda item: len(item[1]), reverse=True):
        redacted = redacted.replace(value, f"[REDACTED:{name}]")
    return redacted


def write_evidence(path: Path, text: str, secrets: dict[str, str]) -> None:
    path.write_bytes(redact(text, secrets).encode("utf-8"))


def verify_evidence_safe(evidence: Path, secrets: dict[str, str]) -> None:
    encoded = [(name, value.encode()) for name, value in secrets.items()]
    offenders = []
    for path in evidence.rglob("*"):
        if not path.is_file():
            continue
        contents = path.read_bytes()
        if any(value in contents for _, value in encoded):
            offenders.append(path)
    if offenders:
        for path in offenders:
            path.unlink(missing_ok=True)
        names = ", ".join(path.name for path in offenders)
        raise RuntimeError(f"credential material was detected and removed from evidence: {names}")


def unix_session_groups(session_id: int) -> set[int]:
    try:
        observed = subprocess.run(
            ["ps", "-axo", "pid=,pgid=,sid="],
            check=False,
            capture_output=True,
            text=True,
            timeout=10,
        )
    except (OSError, subprocess.TimeoutExpired):
        # A restricted host may prohibit process-table inspection. The session leader's group is
        # still terminated, which lets SkillMount's own supervisor tear down its managed domain.
        return {session_id}
    if observed.returncode != 0:
        return {session_id}
    groups = set()
    for line in observed.stdout.splitlines():
        fields = line.split()
        if len(fields) != 3:
            continue
        try:
            _, process_group, session = (int(field) for field in fields)
        except ValueError:
            continue
        if session == session_id:
            groups.add(process_group)
    return groups


def signal_process_groups(groups: set[int], sent: signal.Signals) -> None:
    for process_group in groups:
        try:
            os.killpg(process_group, sent)
        except ProcessLookupError:
            pass


class WindowsJob:
    """Kill-on-close Job Object containing one Windows command tree."""

    def __init__(self, handle: int) -> None:
        self.handle: int | None = handle

    @classmethod
    def attach(cls, process: subprocess.Popen[str]) -> WindowsJob | None:
        if os.name != "nt":
            return None

        import ctypes
        from ctypes import wintypes

        class BasicLimitInformation(ctypes.Structure):
            _fields_ = (
                ("per_process_user_time_limit", ctypes.c_longlong),
                ("per_job_user_time_limit", ctypes.c_longlong),
                ("limit_flags", wintypes.DWORD),
                ("minimum_working_set_size", ctypes.c_size_t),
                ("maximum_working_set_size", ctypes.c_size_t),
                ("active_process_limit", wintypes.DWORD),
                ("affinity", ctypes.c_size_t),
                ("priority_class", wintypes.DWORD),
                ("scheduling_class", wintypes.DWORD),
            )

        class IoCounters(ctypes.Structure):
            _fields_ = tuple(
                (name, ctypes.c_ulonglong)
                for name in (
                    "read_operation_count",
                    "write_operation_count",
                    "other_operation_count",
                    "read_transfer_count",
                    "write_transfer_count",
                    "other_transfer_count",
                )
            )

        class ExtendedLimitInformation(ctypes.Structure):
            _fields_ = (
                ("basic_limit_information", BasicLimitInformation),
                ("io_info", IoCounters),
                ("process_memory_limit", ctypes.c_size_t),
                ("job_memory_limit", ctypes.c_size_t),
                ("peak_process_memory_used", ctypes.c_size_t),
                ("peak_job_memory_used", ctypes.c_size_t),
            )

        kernel32 = ctypes.WinDLL("kernel32", use_last_error=True)
        kernel32.CreateJobObjectW.argtypes = (ctypes.c_void_p, wintypes.LPCWSTR)
        kernel32.CreateJobObjectW.restype = wintypes.HANDLE
        kernel32.SetInformationJobObject.argtypes = (
            wintypes.HANDLE,
            ctypes.c_int,
            ctypes.c_void_p,
            wintypes.DWORD,
        )
        kernel32.SetInformationJobObject.restype = wintypes.BOOL
        kernel32.AssignProcessToJobObject.argtypes = (wintypes.HANDLE, wintypes.HANDLE)
        kernel32.AssignProcessToJobObject.restype = wintypes.BOOL
        kernel32.CloseHandle.argtypes = (wintypes.HANDLE,)
        kernel32.CloseHandle.restype = wintypes.BOOL

        handle = kernel32.CreateJobObjectW(None, None)
        if not handle:
            raise RuntimeError(f"CreateJobObjectW failed: {ctypes.WinError(ctypes.get_last_error())}")
        try:
            information = ExtendedLimitInformation()
            information.basic_limit_information.limit_flags = 0x00002000
            if not kernel32.SetInformationJobObject(
                handle, 9, ctypes.byref(information), ctypes.sizeof(information)
            ):
                raise RuntimeError(
                    f"SetInformationJobObject failed: {ctypes.WinError(ctypes.get_last_error())}"
                )
            process_handle = getattr(process, "_handle", None)
            if process_handle is None or not kernel32.AssignProcessToJobObject(
                handle, process_handle
            ):
                if process.poll() is not None:
                    kernel32.CloseHandle(handle)
                    return None
                raise RuntimeError(
                    f"AssignProcessToJobObject failed: {ctypes.WinError(ctypes.get_last_error())}"
                )
        except Exception:
            kernel32.CloseHandle(handle)
            raise
        return cls(int(handle))

    def terminate(self) -> None:
        if self.handle is None:
            return
        import ctypes
        from ctypes import wintypes

        kernel32 = ctypes.WinDLL("kernel32", use_last_error=True)
        kernel32.TerminateJobObject.argtypes = (wintypes.HANDLE, wintypes.UINT)
        kernel32.TerminateJobObject.restype = wintypes.BOOL
        if not kernel32.TerminateJobObject(self.handle, 1):
            raise RuntimeError(
                f"TerminateJobObject failed: {ctypes.WinError(ctypes.get_last_error())}"
            )

    def close(self) -> None:
        if self.handle is None:
            return
        import ctypes
        from ctypes import wintypes

        handle, self.handle = self.handle, None
        kernel32 = ctypes.WinDLL("kernel32", use_last_error=True)
        kernel32.CloseHandle.argtypes = (wintypes.HANDLE,)
        kernel32.CloseHandle.restype = wintypes.BOOL
        if not kernel32.CloseHandle(handle):
            raise RuntimeError(
                f"CloseHandle for process Job Object failed: {ctypes.WinError(ctypes.get_last_error())}"
            )


def terminate_process_tree(
    process: subprocess.Popen[str], windows_job: WindowsJob | None
) -> None:
    if os.name == "nt":
        if windows_job is not None:
            windows_job.terminate()
            try:
                process.wait(timeout=5)
            except subprocess.TimeoutExpired:
                process.kill()
                process.wait(timeout=5)
            return
        try:
            killed = subprocess.run(
                ["taskkill.exe", "/PID", str(process.pid), "/T", "/F"],
                check=False,
                capture_output=True,
                text=True,
                timeout=15,
            )
        except (OSError, subprocess.TimeoutExpired) as error:
            process.kill()
            process.wait(timeout=5)
            raise RuntimeError(
                f"taskkill could not run for timed-out process tree {process.pid}"
            ) from error
        if killed.returncode != 0 and process.poll() is None:
            process.kill()
            process.wait(timeout=5)
            raise RuntimeError(
                f"taskkill could not terminate timed-out process tree {process.pid}"
            )
        return

    groups = unix_session_groups(process.pid)
    if not groups:
        groups = {process.pid}
    signal_process_groups(groups, signal.SIGTERM)
    try:
        process.wait(timeout=5)
    except subprocess.TimeoutExpired:
        pass
    remaining = unix_session_groups(process.pid)
    signal_process_groups(groups | remaining, signal.SIGKILL)


def run_command(
    command: list[str], *, environment: dict[str, str], timeout: float
) -> CommandResult:
    options: dict[str, object] = {}
    launched_command = command
    if os.name == "nt":
        options["creationflags"] = subprocess.CREATE_NEW_PROCESS_GROUP
        options["stdin"] = subprocess.PIPE
        # The bootstrap cannot launch the requested command until its stdin release byte arrives.
        # This gives the parent time to place it in the Job Object first, closing the otherwise
        # unavoidable race between CreateProcess and AssignProcessToJobObject.
        launched_command = [
            sys.executable,
            str(Path(__file__).resolve()),
            WINDOWS_JOB_BOOTSTRAP,
            *command,
        ]
    else:
        options["start_new_session"] = True
    process = subprocess.Popen(
        launched_command,
        stdout=subprocess.PIPE,
        stderr=subprocess.PIPE,
        text=True,
        encoding="utf-8",
        errors="replace",
        env=environment,
        **options,
    )
    windows_job: WindowsJob | None = None
    try:
        windows_job = WindowsJob.attach(process)
        if os.name == "nt":
            if windows_job is None:
                process.communicate(timeout=5)
                raise RuntimeError("Windows process bootstrap exited before Job Object containment")
            if process.stdin is None:
                raise RuntimeError("Windows process bootstrap has no release pipe")
            process.stdin.write("\x01")
            process.stdin.close()
            process.stdin = None
    except Exception as job_error:
        if process.stdin is not None:
            process.stdin.close()
            process.stdin = None
        try:
            terminate_process_tree(process, windows_job)
            process.communicate(timeout=15)
        except Exception as termination_error:
            for stream in (process.stdout, process.stderr):
                if stream is not None:
                    stream.close()
            raise RuntimeError(
                f"could not contain or terminate process tree {process.pid}: {termination_error}"
            ) from job_error
        if windows_job is not None:
            windows_job.close()
        raise
    try:
        stdout, stderr = process.communicate(timeout=timeout)
        return CommandResult(process.returncode, stdout, stderr, False)
    except subprocess.TimeoutExpired:
        terminate_process_tree(process, windows_job)
        try:
            stdout, stderr = process.communicate(timeout=15)
        except subprocess.TimeoutExpired as error:
            if process.poll() is None:
                process.kill()
                try:
                    process.wait(timeout=5)
                except subprocess.TimeoutExpired:
                    pass
            for stream in (process.stdout, process.stderr):
                if stream is not None:
                    stream.close()
            raise RuntimeError(
                f"timed-out process {process.pid} retained an open output pipe after tree termination"
            ) from error
        return CommandResult(process.returncode, stdout, stderr, True)
    finally:
        if windows_job is not None:
            windows_job.close()


def capture_version(binary: Path, environment: dict[str, str]) -> str:
    completed = run_command(
        [str(binary), "--version"], environment=environment, timeout=30
    )
    if completed.timed_out:
        raise RuntimeError(f"{binary.name} --version timed out")
    if completed.returncode != 0:
        raise RuntimeError(f"{binary.name} --version exited {completed.returncode}")
    return completed.stdout.strip()


def sha256_file(path: Path) -> str:
    digest = hashlib.sha256()
    with path.open("rb") as stream:
        for block in iter(lambda: stream.read(1024 * 1024), b""):
            digest.update(block)
    return digest.hexdigest()


def sha256_text(value: str) -> str:
    return hashlib.sha256(value.encode("utf-8")).hexdigest()


def load_agent_manifest(
    path: Path,
    binaries: dict[str, Path],
    binary_digests: dict[str, str],
    wrapper_target: str,
) -> dict[str, object]:
    manifest = json.loads(path.read_text(encoding="utf-8"))
    if not isinstance(manifest, dict) or manifest.get("schema") != 1:
        raise RuntimeError("agent supply-chain manifest has an unsupported schema")
    expected_platform = (
        "macos-arm64"
        if wrapper_target == "aarch64-apple-darwin"
        else "windows-x64"
        if wrapper_target in ("x86_64-pc-windows-msvc", "i686-pc-windows-msvc")
        else None
    )
    if expected_platform is None or manifest.get("platform") != expected_platform:
        raise RuntimeError(
            f"agent supply-chain manifest does not match wrapper target {wrapper_target}"
        )
    packages = manifest.get("packages")
    if not isinstance(packages, list) or len(packages) != 2:
        raise RuntimeError("agent supply-chain manifest has no package records")
    records = {
        record.get("agent"): record for record in packages if isinstance(record, dict)
    }
    if set(records) != set(binaries):
        raise RuntimeError("agent supply-chain manifest does not name exactly codex and claude")
    for agent in binaries:
        expected = records[agent].get("binary_sha256")
        observed = binary_digests[agent]
        if not isinstance(expected, str) or expected != observed:
            raise RuntimeError(f"{agent} binary does not match its integrity-locked package")
    return manifest


def skill_body(name: str, token: str) -> str:
    return (
        "---\n"
        f"name: {name}\n"
        "description: Live SkillMount discovery probe. Use only when explicitly requested.\n"
        "---\n"
        "# SkillMount live discovery probe\n\n"
        f"When explicitly asked to use this Skill, contribute the token `{token}`. If another "
        "live discovery probe Skill is requested at the same time, return the requested tokens in "
        "order separated by one space and no other text.\n"
    )


def create_sources(root: Path, overlay_name: str, base_name: str) -> list[Path]:
    sources = []
    for ordinal in range(1, 4):
        source = root / f"source-{ordinal}"
        skill = source / overlay_name
        skill.mkdir(parents=True)
        (skill / "SKILL.md").write_text(
            skill_body(overlay_name, f"SKILLMOUNT_LIVE_WINNER_{ordinal}"),
            encoding="utf-8",
        )
        if ordinal == 1:
            base = source / base_name
            base.mkdir()
            (base / "SKILL.md").write_text(
                skill_body(base_name, BASE_TOKEN), encoding="utf-8"
            )
        sources.append(source)
    return sources


def wrapper_prefix(
    asm: Path,
    agent: str,
    sources: list[Path],
    project: Path,
    agent_bin: Path,
    link_mode: str,
) -> list[str]:
    command = [str(asm), agent]
    for source in sources:
        command.extend(("--skills-dir", str(source)))
    command.extend(
        (
            "--project-root",
            str(project),
            "--cwd",
            str(project),
            "--agent-bin",
            str(agent_bin),
            "--link-mode",
            link_mode,
            "--",
        )
    )
    return command


def string_values(value: object) -> Iterator[str]:
    if isinstance(value, str):
        yield value
    elif isinstance(value, list):
        for item in value:
            yield from string_values(item)
    elif isinstance(value, dict):
        for item in value.values():
            yield from string_values(item)


def evaluate_output(name: str, stdout: str) -> tuple[bool, bool, bool]:
    displaced = ("SKILLMOUNT_LIVE_WINNER_1", "SKILLMOUNT_LIVE_WINNER_2")
    if AGENT_CASES[name].text_response:
        normalized = stdout.strip()
        return (
            normalized == EXPECTED_RESPONSE,
            any(token in normalized for token in displaced),
            True,
        )

    try:
        records = [json.loads(line) for line in stdout.splitlines() if line.strip()]
    except json.JSONDecodeError:
        return False, any(token in stdout for token in displaced), False
    values = [text.strip() for record in records for text in string_values(record)]
    return (
        EXPECTED_RESPONSE in values,
        any(token in text for text in values for token in displaced),
        True,
    )


def case_command(
    *,
    asm: Path,
    case: AgentCase,
    sources: list[Path],
    project: Path,
    binary: Path,
    link_mode: str,
    prompt: str,
) -> list[str]:
    command = wrapper_prefix(asm, case.name, sources, project, binary, link_mode)
    command.extend(prompt if token == PROMPT_TOKEN else token for token in case.passthrough)
    return command


def skip_reason(
    case: AgentCase,
    *,
    wrapper_target: str,
    binary: Path | None,
    credential: str | None,
    banner_reason: str | None,
) -> str | None:
    """Why an opt-in case is deliberately not exercised here, or None when it must run.

    `banner_reason` carries an already-observed version drift or unavailable banner, which keeps an
    opt-in case out of the run instead of aborting it the way a locked agent's banner does.
    """
    unsupported = dict(case.unsupported_targets).get(wrapper_target)
    if unsupported is not None:
        return unsupported
    if binary is None:
        return f"--{case.name}-bin was not supplied, so the opt-in {case.name} case is unselected"
    if banner_reason is not None:
        return banner_reason
    if not credential:
        return (
            f"{case.credential_name} is unavailable, so the {case.name} case has no credential to "
            "authenticate with"
        )
    return None


def unknown_result(case: AgentCase, reason: str) -> dict[str, object]:
    """Records a case this run did not exercise; an unknown outcome never grades the run."""
    return {"agent": case.name, "outcome": "unknown", "reason": reason}


def run_agent(
    *,
    name: str,
    command: list[str],
    state: Path,
    evidence: Path,
    binary: Path,
    expected_binary_sha256: str,
    base_environment: dict[str, str],
    credential_name: str,
    credential: str,
    secrets: dict[str, str],
) -> dict[str, object]:
    if sha256_file(binary) != expected_binary_sha256:
        raise RuntimeError(f"{name} binary changed before its credential-bearing launch")
    environment = base_environment.copy()
    environment["SKILLMOUNT_STATE_DIR"] = str(state)
    environment[credential_name] = credential
    completed = run_command(command, environment=environment, timeout=240)
    write_evidence(evidence / f"{name}.stdout.log", completed.stdout, secrets)
    write_evidence(evidence / f"{name}.stderr.log", completed.stderr, secrets)
    winner, displaced, machine_output_valid = evaluate_output(name, completed.stdout)
    journals = list((state / "transactions").glob("*.journal"))
    passed = (
        not completed.timed_out
        and completed.returncode == 0
        and winner
        and not displaced
        and machine_output_valid
        and not journals
    )
    if completed.timed_out:
        outcome = "unverified"
        reason = "agent execution timed out after its process tree was terminated"
    elif completed.returncode != 0:
        outcome = "unverified"
        reason = "agent exited before a compatibility observation; authentication may be unavailable"
    else:
        outcome = "pass" if passed else "fail"
        reason = None
    return {
        "agent": name,
        "exit_code": completed.returncode,
        "timed_out": completed.timed_out,
        "machine_output_valid": machine_output_valid,
        "base_and_winner_3_observed_as_exact_response": winner,
        "displaced_winner_observed": displaced,
        "journal_residue_count": len(journals),
        "stdout_sha256": sha256_text(redact(completed.stdout, secrets)),
        "stderr_sha256": sha256_text(redact(completed.stderr, secrets)),
        "outcome": outcome,
        **({"reason": reason} if reason is not None else {}),
    }


def workflow_context(environment: dict[str, str]) -> dict[str, str]:
    keys = (
        "GITHUB_REPOSITORY",
        "GITHUB_RUN_ID",
        "GITHUB_RUN_ATTEMPT",
        "GITHUB_SHA",
        "RUNNER_NAME",
        "RUNNER_OS",
        "RUNNER_ARCH",
    )
    context = {key.lower(): environment[key] for key in keys if environment.get(key)}
    if all(
        environment.get(key)
        for key in ("GITHUB_SERVER_URL", "GITHUB_REPOSITORY", "GITHUB_RUN_ID")
    ):
        context["run_url"] = (
            f"{environment['GITHUB_SERVER_URL']}/{environment['GITHUB_REPOSITORY']}"
            f"/actions/runs/{environment['GITHUB_RUN_ID']}"
        )
    return context


def write_summary(
    evidence: Path, summary: dict[str, object], secrets: dict[str, str]
) -> None:
    write_evidence(
        evidence / "summary.json",
        json.dumps(summary, indent=2, sort_keys=True) + "\n",
        secrets,
    )
    verify_evidence_safe(evidence, secrets)


def main() -> int:
    args = parse_args()
    evidence = args.evidence_dir.resolve()
    evidence.mkdir(parents=True, exist_ok=True)
    inherited_environment = os.environ.copy()
    base_environment, secrets = split_environment(inherited_environment)
    base_environment["DISABLE_AUTOUPDATER"] = "1"
    started = dt.datetime.now(dt.timezone.utc)
    summary: dict[str, object] = {
        "schema": 1,
        "started_at": started.isoformat(),
        "date": started.date().isoformat(),
        "os": platform.system(),
        "architecture": platform.machine(),
        "wrapper_target": args.wrapper_target,
        "link_kind": args.link_mode,
        "scenario": "three-source rightmost-wins and non-shadowed base Skill discovery",
        "workflow": workflow_context(inherited_environment),
        "results": [],
    }

    try:
        asm = args.asm.resolve(strict=True)
        explicit = {
            "codex": args.codex_bin,
            "claude": args.claude_bin,
            "omp": args.omp_bin,
        }
        binaries: dict[str, Path] = {}
        for case in AGENT_CASES.values():
            # An opt-in case is never resolved from PATH: selecting it must be an operator act.
            if case.opt_in and explicit[case.name] is None:
                continue
            binaries[case.name] = agent_executable(explicit[case.name], case.executable)
        locked = {
            name: binary for name, binary in binaries.items() if not AGENT_CASES[name].opt_in
        }
        manifest_path = args.agent_manifest.resolve(strict=True)
        binary_digests = {
            "asm": sha256_file(asm),
            **{name: sha256_file(binary) for name, binary in binaries.items()},
        }
        summary["agent_supply_chain"] = load_agent_manifest(
            manifest_path, locked, binary_digests, args.wrapper_target
        )
        summary["binary_sha256"] = binary_digests
        summary["agent_integrity"] = {name: AGENT_CASES[name].integrity for name in binaries}
        summary["skillmount_version"] = capture_version(asm, base_environment)

        versions: dict[str, str] = {}
        banner_reasons: dict[str, str] = {}
        for name, binary in binaries.items():
            case = AGENT_CASES[name]
            try:
                observed = capture_version(binary, base_environment)
            except RuntimeError as error:
                if not case.opt_in:
                    raise
                banner_reasons[name] = redact(str(error), secrets)
                continue
            versions[name] = observed
            if observed == case.banner:
                continue
            if not case.opt_in:
                raise RuntimeError(f"expected {name} {case.banner!r}, observed {observed!r}")
            banner_reasons[name] = (
                f"{name} reported {observed!r} rather than the last-tested {case.banner!r}"
            )
        summary["versions"] = versions
        for name, binary in binaries.items():
            if sha256_file(binary) != binary_digests[name]:
                raise RuntimeError(f"{name} binary changed during its credential-free probe")

        credentials: dict[str, str] = {}
        for case in AGENT_CASES.values():
            value = inherited_environment.get(case.credential_name)
            if value:
                credentials[case.credential_name] = value
            elif not case.opt_in:
                raise RuntimeError(f"required credential {case.credential_name} is unavailable")

        with tempfile.TemporaryDirectory(prefix="skillmount-live-smoke-") as temporary:
            root = Path(temporary)
            project = root / "project"
            project.mkdir()
            nonce = started.strftime("%Y%m%d%H%M%S")
            overlay_name = f"skillmount-live-overlay-{nonce}"
            base_name = f"skillmount-live-base-{nonce}"
            sources = create_sources(root, overlay_name, base_name)
            prompt = (
                f"Use the {base_name} Skill and then the {overlay_name} Skill now, in that order, "
                "and return their required tokens."
            )

            doctor_environment = base_environment.copy()
            doctor_environment["SKILLMOUNT_STATE_DIR"] = str(root / "doctor-state")
            doctor_command = [str(asm), "doctor", "--project-root", str(project)]
            for name, binary in binaries.items():
                doctor_command.extend((f"--{name}-bin", str(binary)))
            doctor = run_command(
                doctor_command,
                environment=doctor_environment,
                timeout=120,
            )
            write_evidence(evidence / "doctor.stdout.log", doctor.stdout, secrets)
            write_evidence(evidence / "doctor.stderr.log", doctor.stderr, secrets)
            summary["doctor"] = {
                "exit_code": doctor.returncode,
                "timed_out": doctor.timed_out,
                "stdout_sha256": sha256_text(redact(doctor.stdout, secrets)),
                "stderr_sha256": sha256_text(redact(doctor.stderr, secrets)),
            }
            if doctor.timed_out:
                raise RuntimeError("asm doctor timed out")
            if doctor.returncode != 0:
                raise RuntimeError(f"asm doctor exited {doctor.returncode}")

            for case in AGENT_CASES.values():
                reason = skip_reason(
                    case,
                    wrapper_target=args.wrapper_target,
                    binary=binaries.get(case.name),
                    credential=credentials.get(case.credential_name),
                    banner_reason=banner_reasons.get(case.name),
                )
                if reason is not None:
                    summary["results"].append(unknown_result(case, reason))
                    continue
                binary = binaries[case.name]
                result = run_agent(
                    name=case.name,
                    command=case_command(
                        asm=asm,
                        case=case,
                        sources=sources,
                        project=project,
                        binary=binary,
                        link_mode=args.link_mode,
                        prompt=prompt,
                    ),
                    state=root / f"{case.name}-state",
                    evidence=evidence,
                    binary=binary,
                    expected_binary_sha256=binary_digests[case.name],
                    base_environment=base_environment,
                    credential_name=case.credential_name,
                    credential=credentials[case.credential_name],
                    secrets=secrets,
                )
                result["discovery_destination"] = str(project / case.destination)
                summary["results"].append(result)
    except Exception as error:  # Evidence must survive every expected operational failure.
        summary["finished_at"] = dt.datetime.now(dt.timezone.utc).isoformat()
        summary["outcome"] = "unverified"
        summary["reason"] = redact(str(error), secrets)
        write_summary(evidence, summary, secrets)
        print(f"live smoke unverified: {redact(str(error), secrets)}", file=sys.stderr)
        return 2

    results = summary["results"]
    assert isinstance(results, list)
    summary["finished_at"] = dt.datetime.now(dt.timezone.utc).isoformat()
    # An `unknown` case was deliberately not exercised, so it can neither grade nor block the run.
    graded = {result["outcome"] for result in results} - {"unknown"}
    if graded == {"pass"}:
        summary["outcome"] = "pass"
    elif "fail" in graded:
        summary["outcome"] = "fail"
    else:
        summary["outcome"] = "unverified"
    write_summary(evidence, summary, secrets)
    print(json.dumps(summary, indent=2, sort_keys=True))
    if summary["outcome"] == "pass":
        return 0
    if summary["outcome"] == "unverified":
        return 2
    return 1


def windows_job_bootstrap(command: list[str]) -> int:
    """Waits until Job assignment, then runs one command inside that inherited Job."""
    if os.name != "nt" or not command:
        return 125
    if sys.stdin.read(1) != "\x01":
        return 125
    child = subprocess.Popen(command)
    return child.wait()


if __name__ == "__main__":
    if len(sys.argv) > 1 and sys.argv[1] == WINDOWS_JOB_BOOTSTRAP:
        raise SystemExit(windows_job_bootstrap(sys.argv[2:]))
    raise SystemExit(main())
