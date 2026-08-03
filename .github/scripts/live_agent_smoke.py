#!/usr/bin/env python3
"""Run opt-in real-agent Skill discovery smokes and retain evidence.

This harness never installs tools or supplies credentials. The manual workflow owns installation,
and the caller's environment supplies authentication without the harness printing it.
"""

from __future__ import annotations

import argparse
import datetime as dt
import json
import os
import platform
import shutil
import subprocess
import sys
import tempfile
from pathlib import Path

CODEX_VERSION = "codex-cli 0.146.0"
CLAUDE_VERSION = "2.1.220 (Claude Code)"


def parse_args() -> argparse.Namespace:
    parser = argparse.ArgumentParser()
    parser.add_argument("--asm", required=True, type=Path)
    parser.add_argument("--link-mode", required=True, choices=("symlink", "junction"))
    parser.add_argument("--wrapper-target", required=True)
    parser.add_argument("--evidence-dir", required=True, type=Path)
    parser.add_argument("--codex-bin", type=Path)
    parser.add_argument("--claude-bin", type=Path)
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


def capture_version(binary: Path) -> str:
    completed = subprocess.run(
        [str(binary), "--version"],
        check=False,
        capture_output=True,
        text=True,
        timeout=30,
    )
    if completed.returncode != 0:
        raise RuntimeError(f"{binary.name} --version exited {completed.returncode}")
    return completed.stdout.strip()


def skill_body(name: str, token: str) -> str:
    return (
        "---\n"
        f"name: {name}\n"
        "description: Live SkillMount discovery probe. Use only when explicitly requested.\n"
        "---\n"
        "# SkillMount live discovery probe\n\n"
        f"When explicitly asked to use this Skill, respond with exactly `{token}` and no other text.\n"
    )


def create_sources(root: Path, name: str) -> list[Path]:
    sources = []
    for ordinal in range(1, 4):
        source = root / f"source-{ordinal}"
        skill = source / name
        skill.mkdir(parents=True)
        (skill / "SKILL.md").write_text(
            skill_body(name, f"SKILLMOUNT_LIVE_WINNER_{ordinal}"), encoding="utf-8"
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


def run_agent(
    *,
    name: str,
    command: list[str],
    state: Path,
    evidence: Path,
) -> dict[str, object]:
    environment = os.environ.copy()
    environment["SKILLMOUNT_STATE_DIR"] = str(state)
    completed = subprocess.run(
        command,
        check=False,
        capture_output=True,
        text=True,
        timeout=240,
        env=environment,
    )
    (evidence / f"{name}.stdout.log").write_text(completed.stdout, encoding="utf-8")
    (evidence / f"{name}.stderr.log").write_text(completed.stderr, encoding="utf-8")
    winner = "SKILLMOUNT_LIVE_WINNER_3"
    displaced = ("SKILLMOUNT_LIVE_WINNER_1", "SKILLMOUNT_LIVE_WINNER_2")
    passed = (
        completed.returncode == 0
        and winner in completed.stdout
        and not any(token in completed.stdout for token in displaced)
    )
    return {
        "agent": name,
        "exit_code": completed.returncode,
        "winner_3_observed": winner in completed.stdout,
        "displaced_winner_observed": any(token in completed.stdout for token in displaced),
        "outcome": "pass" if passed else "fail",
    }


def main() -> int:
    args = parse_args()
    evidence = args.evidence_dir.resolve()
    evidence.mkdir(parents=True, exist_ok=True)
    summary: dict[str, object] = {
        "date": dt.datetime.now(dt.timezone.utc).date().isoformat(),
        "os": platform.system(),
        "architecture": platform.machine(),
        "wrapper_target": args.wrapper_target,
        "link_kind": args.link_mode,
        "scenario": "three-source rightmost-wins Skill discovery",
        "results": [],
    }

    try:
        asm = args.asm.resolve(strict=True)
        codex = agent_executable(args.codex_bin, "codex")
        claude = agent_executable(args.claude_bin, "claude")
        versions = {
            "codex": capture_version(codex),
            "claude": capture_version(claude),
        }
        summary["versions"] = versions
        if versions["codex"] != CODEX_VERSION:
            raise RuntimeError(
                f"expected Codex {CODEX_VERSION!r}, observed {versions['codex']!r}"
            )
        if versions["claude"] != CLAUDE_VERSION:
            raise RuntimeError(
                f"expected Claude {CLAUDE_VERSION!r}, observed {versions['claude']!r}"
            )

        with tempfile.TemporaryDirectory(prefix="skillmount-live-smoke-") as temporary:
            root = Path(temporary)
            project = root / "project"
            project.mkdir()
            nonce = dt.datetime.now(dt.timezone.utc).strftime("%Y%m%d%H%M%S")
            skill_name = f"skillmount-live-probe-{nonce}"
            sources = create_sources(root, skill_name)
            prompt = f"Use the {skill_name} Skill now and return its required token."

            doctor = subprocess.run(
                [
                    str(asm),
                    "doctor",
                    "--project-root",
                    str(project),
                    "--codex-bin",
                    str(codex),
                    "--claude-bin",
                    str(claude),
                ],
                check=False,
                capture_output=True,
                text=True,
                timeout=120,
                env={**os.environ, "SKILLMOUNT_STATE_DIR": str(root / "doctor-state")},
            )
            (evidence / "doctor.stdout.log").write_text(doctor.stdout, encoding="utf-8")
            (evidence / "doctor.stderr.log").write_text(doctor.stderr, encoding="utf-8")
            summary["doctor_exit_code"] = doctor.returncode
            if doctor.returncode != 0:
                raise RuntimeError(f"asm doctor exited {doctor.returncode}")

            codex_command = wrapper_prefix(
                asm, "codex", sources, project, codex, args.link_mode
            )
            codex_command.extend(("exec", "--skip-git-repo-check", "--json", prompt))
            summary["results"].append(
                run_agent(
                    name="codex",
                    command=codex_command,
                    state=root / "codex-state",
                    evidence=evidence,
                )
            )

            claude_command = wrapper_prefix(
                asm, "claude", sources, project, claude, args.link_mode
            )
            claude_command.extend(("-p", prompt, "--output-format", "text"))
            summary["results"].append(
                run_agent(
                    name="claude",
                    command=claude_command,
                    state=root / "claude-state",
                    evidence=evidence,
                )
            )
    except Exception as error:  # Evidence must survive every expected operational failure.
        summary["outcome"] = "unverified"
        summary["reason"] = str(error)
        (evidence / "summary.json").write_text(
            json.dumps(summary, indent=2, sort_keys=True) + "\n", encoding="utf-8"
        )
        print(f"live smoke unverified: {error}", file=sys.stderr)
        return 2

    results = summary["results"]
    assert isinstance(results, list)
    summary["outcome"] = (
        "pass" if results and all(result["outcome"] == "pass" for result in results) else "fail"
    )
    (evidence / "summary.json").write_text(
        json.dumps(summary, indent=2, sort_keys=True) + "\n", encoding="utf-8"
    )
    print(json.dumps(summary, indent=2, sort_keys=True))
    return 0 if summary["outcome"] == "pass" else 1


if __name__ == "__main__":
    raise SystemExit(main())
