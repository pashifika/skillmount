#!/usr/bin/env python3
"""Deterministic tests for live-smoke secret handling, output proof, and timeouts."""

from __future__ import annotations

import io
import json
import os
import subprocess
import sys
import tarfile
import tempfile
import time
import unittest
from pathlib import Path

from live_agent_smoke import (
    EXPECTED_RESPONSE,
    evaluate_output,
    load_agent_manifest,
    run_agent,
    run_command,
    sha256_file,
    split_environment,
    verify_evidence_safe,
)
from prepare_live_agents import extract_regular_file, sri, verify_archive


class LiveAgentSmokeTests(unittest.TestCase):
    def test_each_agent_receives_only_its_credential_and_logs_are_redacted(self) -> None:
        inherited = {
            **os.environ,
            "CODEX_API_KEY": "codex-unit-secret",
            "OPENAI_API_KEY": "legacy-openai-unit-secret",
            "ANTHROPIC_API_KEY": "anthropic-unit-secret",
            "UNRELATED_PASSWORD": "password-unit-secret",
        }
        base, secrets = split_environment(inherited)
        self.assertNotIn("CODEX_API_KEY", base)
        self.assertNotIn("OPENAI_API_KEY", base)
        self.assertNotIn("ANTHROPIC_API_KEY", base)
        self.assertNotIn("UNRELATED_PASSWORD", base)
        script = (
            "import json, os; "
            f"print(json.dumps({{'text':{EXPECTED_RESPONSE!r},"
            "'own':os.environ.get('CODEX_API_KEY'),"
            "'legacy':os.environ.get('OPENAI_API_KEY'),"
            "'other':os.environ.get('ANTHROPIC_API_KEY'),"
            "'unrelated':os.environ.get('UNRELATED_PASSWORD')}))"
        )
        with tempfile.TemporaryDirectory() as temporary:
            root = Path(temporary)
            result = run_agent(
                name="codex",
                command=[sys.executable, "-c", script],
                state=root / "state",
                evidence=root,
                binary=Path(sys.executable),
                expected_binary_sha256=sha256_file(Path(sys.executable)),
                base_environment=base,
                credential_name="CODEX_API_KEY",
                credential=inherited["CODEX_API_KEY"],
                secrets=secrets,
            )

            self.assertEqual(result["outcome"], "pass")
            logged = (root / "codex.stdout.log").read_text(encoding="utf-8")
            self.assertIn("[REDACTED:CODEX_API_KEY]", logged)
            self.assertIn('"legacy": null', logged)
            self.assertIn('"other": null', logged)
            self.assertIn('"unrelated": null', logged)
            self.assertEqual(
                result["stdout_sha256"], sha256_file(root / "codex.stdout.log")
            )
            self.assertEqual(
                result["stderr_sha256"], sha256_file(root / "codex.stderr.log")
            )
            verify_evidence_safe(root, secrets)

    def test_machine_output_requires_an_exact_winner_value(self) -> None:
        codex = json.dumps(
            {"type": "item.completed", "item": {"text": EXPECTED_RESPONSE}}
        )
        self.assertEqual(evaluate_output("codex", codex), (True, False, True))
        self.assertEqual(
            evaluate_output("codex", "wrapper prefix\n" + codex),
            (False, False, False),
        )
        self.assertEqual(
            evaluate_output("claude", EXPECTED_RESPONSE + "\n"),
            (True, False, True),
        )
        self.assertEqual(
            evaluate_output("claude", "prefix " + EXPECTED_RESPONSE),
            (False, False, True),
        )

    def test_agent_exit_before_observation_is_unverified_not_compatibility_failure(self) -> None:
        inherited = {**os.environ, "CODEX_API_KEY": "invalid-unit-credential"}
        base, secrets = split_environment(inherited)
        with tempfile.TemporaryDirectory() as temporary:
            root = Path(temporary)
            result = run_agent(
                name="codex",
                command=[sys.executable, "-c", "raise SystemExit(7)"],
                state=root / "state",
                evidence=root,
                binary=Path(sys.executable),
                expected_binary_sha256=sha256_file(Path(sys.executable)),
                base_environment=base,
                credential_name="CODEX_API_KEY",
                credential=inherited["CODEX_API_KEY"],
                secrets=secrets,
            )
        self.assertEqual(result["outcome"], "unverified")
        self.assertEqual(result["exit_code"], 7)

    def test_timeout_terminates_descendants_before_they_can_continue(self) -> None:
        child_script = (
            "import sys, time; from pathlib import Path; "
            "time.sleep(2); Path(sys.argv[1]).write_text('survived', encoding='utf-8')"
        )
        parent_script = (
            "import subprocess, sys, time; from pathlib import Path; "
            "subprocess.Popen([sys.executable, '-c', sys.argv[1], sys.argv[2]]); "
            "Path(sys.argv[3]).write_text('started', encoding='utf-8'); "
            "time.sleep(60)"
        )
        with tempfile.TemporaryDirectory() as temporary:
            marker = Path(temporary) / "descendant-survived"
            started = Path(temporary) / "descendant-started"
            result = run_command(
                [
                    sys.executable,
                    "-c",
                    parent_script,
                    child_script,
                    str(marker),
                    str(started),
                ],
                environment=os.environ.copy(),
                timeout=1,
            )
            self.assertTrue(result.timed_out)
            self.assertTrue(started.exists(), "the descendant was not started by the fixture")
            time.sleep(2.2)
            self.assertFalse(marker.exists(), "a timed-out descendant kept running")

    def test_timeout_terminates_descendants_after_the_parent_has_already_exited(self) -> None:
        child_script = (
            "import sys, time; from pathlib import Path; "
            "time.sleep(2); Path(sys.argv[1]).write_text('survived', encoding='utf-8')"
        )
        parent_script = (
            "import subprocess, sys; from pathlib import Path; "
            "subprocess.Popen([sys.executable, '-c', sys.argv[1], sys.argv[2]]); "
            "Path(sys.argv[3]).write_text('started', encoding='utf-8')"
        )
        with tempfile.TemporaryDirectory() as temporary:
            marker = Path(temporary) / "orphan-survived"
            started = Path(temporary) / "orphan-started"
            result = run_command(
                [
                    sys.executable,
                    "-c",
                    parent_script,
                    child_script,
                    str(marker),
                    str(started),
                ],
                environment=os.environ.copy(),
                timeout=1,
            )
            self.assertTrue(result.timed_out)
            self.assertTrue(started.exists(), "the descendant was not started by the fixture")
            time.sleep(2.2)
            self.assertFalse(marker.exists(), "the exited parent left a live descendant")


class AgentPackageTests(unittest.TestCase):
    @unittest.skipUnless(os.name == "nt", "Windows command resolution regression")
    def test_prepare_cli_launches_windows_npm_cmd_from_path(self) -> None:
        with tempfile.TemporaryDirectory() as temporary:
            root = Path(temporary)
            marker = root / "npm-invocation.txt"
            (root / "npm.cmd").write_text(
                "@echo off\r\n"
                "> \"%SKILLMOUNT_NPM_MARKER%\" echo %*\r\n"
                "exit /b 23\r\n",
                encoding="utf-8",
            )
            environment = os.environ.copy()
            environment["PATH"] = str(root) + os.pathsep + environment.get("PATH", "")
            environment["SKILLMOUNT_NPM_MARKER"] = str(marker)

            completed = subprocess.run(
                [
                    sys.executable,
                    str(Path(__file__).with_name("prepare_live_agents.py")),
                    "--platform",
                    "windows-x64",
                    "--output-dir",
                    str(root / "agents"),
                ],
                check=False,
                capture_output=True,
                text=True,
                env=environment,
                timeout=30,
            )

            self.assertTrue(
                marker.is_file(),
                f"prepare CLI did not launch npm.cmd:\n{completed.stderr}",
            )
            self.assertIn(
                "pack @openai/codex@0.146.0-win32-x64 --ignore-scripts --json",
                marker.read_text(encoding="utf-8"),
            )

    @unittest.skipUnless(os.name == "nt", "Windows npm metadata regression")
    def test_prepare_cli_accepts_npm_object_metadata(self) -> None:
        with tempfile.TemporaryDirectory() as temporary:
            root = Path(temporary)
            fixture = root / "npm-fixture.py"
            fixture.write_text(
                "import json\n"
                "print(json.dumps({\n"
                "    '@openai/codex': {\n"
                "        'name': '@openai/codex',\n"
                "        'version': '0.146.0-win32-x64',\n"
                "        'integrity': "
                "'sha512-b3lxMYeR0+IhstNo4JjX1P9cPc1xwVcCVkPd1lD1wpWPJ0SBhpIkPczwbu3ZRkJcdyl342+rgyf4DUrbZLdrGA==',\n"
                "        'filename': '../unsafe.tgz',\n"
                "    },\n"
                "}))\n",
                encoding="utf-8",
            )
            (root / "npm.cmd").write_text(
                "@echo off\r\n"
                '"%SKILLMOUNT_TEST_PYTHON%" "%SKILLMOUNT_NPM_FIXTURE%"\r\n'
                "exit /b %ERRORLEVEL%\r\n",
                encoding="utf-8",
            )
            environment = os.environ.copy()
            environment["PATH"] = str(root) + os.pathsep + environment.get("PATH", "")
            environment["SKILLMOUNT_TEST_PYTHON"] = sys.executable
            environment["SKILLMOUNT_NPM_FIXTURE"] = str(fixture)

            completed = subprocess.run(
                [
                    sys.executable,
                    str(Path(__file__).with_name("prepare_live_agents.py")),
                    "--platform",
                    "windows-x64",
                    "--output-dir",
                    str(root / "agents"),
                ],
                check=False,
                capture_output=True,
                text=True,
                env=environment,
                timeout=30,
            )

            self.assertIn("npm returned an unsafe archive name", completed.stderr)

    def test_manifest_binds_both_native_binary_hashes_and_platform(self) -> None:
        with tempfile.TemporaryDirectory() as temporary:
            root = Path(temporary)
            binaries = {"codex": root / "codex", "claude": root / "claude"}
            for agent, binary in binaries.items():
                binary.write_text(agent, encoding="utf-8")
            digests = {agent: sha256_file(binary) for agent, binary in binaries.items()}
            manifest = root / "manifest.json"
            manifest.write_text(
                json.dumps(
                    {
                        "schema": 1,
                        "platform": "macos-arm64",
                        "packages": [
                            {"agent": agent, "binary_sha256": digests[agent]}
                            for agent in ("codex", "claude")
                        ],
                    }
                ),
                encoding="utf-8",
            )

            loaded = load_agent_manifest(
                manifest, binaries, digests, "aarch64-apple-darwin"
            )
            self.assertEqual(loaded["platform"], "macos-arm64")
            with self.assertRaisesRegex(RuntimeError, "does not match wrapper target"):
                load_agent_manifest(
                    manifest, binaries, digests, "x86_64-pc-windows-msvc"
                )

    def test_verified_archive_extracts_only_the_named_regular_file(self) -> None:
        with tempfile.TemporaryDirectory() as temporary:
            root = Path(temporary)
            archive = root / "agent.tgz"
            payload = b"native-agent"
            with tarfile.open(archive, "w:gz") as bundle:
                info = tarfile.TarInfo("package/bin/agent")
                info.size = len(payload)
                info.mode = 0o755
                bundle.addfile(info, io.BytesIO(payload))

            verify_archive(archive, sri(archive))
            destination = root / "out" / "agent"
            extract_regular_file(archive, "package/bin/agent", destination)
            self.assertEqual(destination.read_bytes(), payload)

    def test_archive_integrity_and_link_members_fail_closed(self) -> None:
        with tempfile.TemporaryDirectory() as temporary:
            root = Path(temporary)
            archive = root / "agent.tgz"
            with tarfile.open(archive, "w:gz") as bundle:
                info = tarfile.TarInfo("package/bin/agent")
                info.type = tarfile.SYMTYPE
                info.linkname = "../../outside"
                bundle.addfile(info)

            with self.assertRaisesRegex(RuntimeError, "integrity mismatch"):
                verify_archive(archive, "sha512-invalid")
            with self.assertRaisesRegex(RuntimeError, "not a regular file"):
                extract_regular_file(archive, "package/bin/agent", root / "agent")


if __name__ == "__main__":
    unittest.main()
