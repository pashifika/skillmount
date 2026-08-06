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
    AGENT_CASES,
    EXPECTED_RESPONSE,
    PROMPT_TOKEN,
    case_command,
    evaluate_output,
    load_agent_manifest,
    run_agent,
    run_command,
    sha256_file,
    skip_reason,
    split_environment,
    unknown_result,
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


class OptInOmpCaseTests(unittest.TestCase):
    def checkout_snapshot(self) -> dict[str, str]:
        """Digests the harness sources and lists the discovery roots a leaked mount would create."""
        checkout = Path(__file__).resolve().parents[2]
        snapshot = {
            str(entry.relative_to(checkout)): sha256_file(entry)
            for entry in sorted((checkout / ".github" / "scripts").iterdir())
            if entry.is_file()
        }
        for root in (".omp", ".claude", ".agents"):
            observed = checkout / root
            snapshot[root] = (
                ",".join(sorted(child.name for child in observed.iterdir()))
                if observed.is_dir()
                else "absent"
            )
        return snapshot

    def omp_stub_run(self, root: Path, argv_dump: Path, secret: str) -> dict[str, object]:
        """Runs the OMP case with its real command line, but a stub in place of the wrapper."""
        project = root / "project"
        project.mkdir(exist_ok=True)
        script = (
            "import os, sys; from pathlib import Path; "
            "Path(sys.argv[1]).write_text(chr(10).join(sys.argv[2:]), encoding='utf-8'); "
            f"print({EXPECTED_RESPONSE!r} if os.environ.get('ANTHROPIC_API_KEY') else 'unauthenticated')"
        )
        command = case_command(
            asm=root / "asm",
            case=AGENT_CASES["omp"],
            sources=[root / "source-1", root / "source-2", root / "source-3"],
            project=project,
            binary=root / "omp",
            link_mode="symlink",
            prompt="Use the live discovery probe Skills now.",
        )
        command[0:1] = [sys.executable, "-c", script, str(argv_dump)]
        base, secrets = split_environment({**os.environ, "ANTHROPIC_API_KEY": secret})
        return run_agent(
            name="omp",
            command=command,
            state=root / "omp-state",
            evidence=root / "evidence",
            binary=Path(sys.executable),
            expected_binary_sha256=sha256_file(Path(sys.executable)),
            base_environment=base,
            credential_name="ANTHROPIC_API_KEY",
            credential=secret,
            secrets=secrets,
        )

    def test_omp_is_registered_as_an_opt_in_project_case_mounting_into_dot_omp_skills(self) -> None:
        case = AGENT_CASES["omp"]
        self.assertEqual(case.executable, "omp")
        self.assertEqual(case.banner, "omp/17.2.9")
        self.assertTrue(case.banner.startswith("omp/"))
        self.assertEqual(case.destination, ".omp/skills")
        self.assertEqual(case.credential_name, "ANTHROPIC_API_KEY")
        self.assertTrue(case.opt_in)
        self.assertFalse(AGENT_CASES["codex"].opt_in or AGENT_CASES["claude"].opt_in)
        # OMP answers `--mode text` in prose, so its response is read as text, not as JSON records.
        self.assertEqual(evaluate_output("omp", EXPECTED_RESPONSE + "\n"), (True, False, True))
        self.assertIn("SHA256SUMS.txt", case.integrity)

    def test_an_unselected_or_uncredentialed_omp_case_is_unknown_rather_than_failed(self) -> None:
        case = AGENT_CASES["omp"]
        unselected = skip_reason(
            case,
            wrapper_target="aarch64-apple-darwin",
            binary=None,
            credential="unit-credential",
            banner_reason=None,
        )
        assert unselected is not None
        self.assertIn("--omp-bin", unselected)
        uncredentialed = skip_reason(
            case,
            wrapper_target="aarch64-apple-darwin",
            binary=Path(sys.executable),
            credential=None,
            banner_reason=None,
        )
        assert uncredentialed is not None
        self.assertIn("ANTHROPIC_API_KEY", uncredentialed)
        for reason in (unselected, uncredentialed):
            self.assertEqual(unknown_result(case, reason)["outcome"], "unknown")
        self.assertIsNone(
            skip_reason(
                case,
                wrapper_target="aarch64-apple-darwin",
                binary=Path(sys.executable),
                credential="unit-credential",
                banner_reason=None,
            )
        )

    def test_windows_x86_is_skipped_because_17_2_9_publishes_no_32_bit_asset(self) -> None:
        case = AGENT_CASES["omp"]
        skipped = skip_reason(
            case,
            wrapper_target="i686-pc-windows-msvc",
            binary=Path(sys.executable),
            credential="unit-credential",
            banner_reason=None,
        )
        self.assertEqual(skipped, "no 32-bit OMP asset is published for 17.2.9")
        self.assertEqual(unknown_result(case, skipped)["outcome"], "unknown")
        # The x86 workflow leg supplies no binary either, and the published-asset reason still wins
        # over the unselected one so the evidence names why that runner can never exercise OMP.
        self.assertEqual(
            skip_reason(
                case,
                wrapper_target="i686-pc-windows-msvc",
                binary=None,
                credential=None,
                banner_reason=None,
            ),
            skipped,
        )
        # 17.2.9 does publish omp-windows-x64.exe, so the 64-bit Windows leg stays exercised.
        self.assertIsNone(
            skip_reason(
                case,
                wrapper_target="x86_64-pc-windows-msvc",
                binary=Path(sys.executable),
                credential="unit-credential",
                banner_reason=None,
            )
        )

    def test_no_built_command_line_carries_the_credential_the_child_reads_from_its_environment(
        self,
    ) -> None:
        secret = "sk-omp-unit-credential-value"
        with tempfile.TemporaryDirectory() as temporary:
            root = Path(temporary)
            (root / "evidence").mkdir()
            argv_dump = root / "argv.txt"
            result = self.omp_stub_run(root, argv_dump, secret)

            forwarded = argv_dump.read_text(encoding="utf-8").splitlines()
            self.assertNotIn(secret, "\n".join(forwarded))
            self.assertNotIn(PROMPT_TOKEN, forwarded)
            # A mutating OMP session forwards the operator's passthrough and nothing else.
            self.assertEqual(
                forwarded[forwarded.index("--") + 1 :],
                [
                    "--print",
                    "--mode",
                    "text",
                    "--no-session",
                    "--auto-approve",
                    "Use the live discovery probe Skills now.",
                ],
            )
            self.assertEqual(result["outcome"], "pass")
            for case in AGENT_CASES.values():
                command = case_command(
                    asm=root / "asm",
                    case=case,
                    sources=[root / "source-1"],
                    project=root / "project",
                    binary=root / case.executable,
                    link_mode="symlink",
                    prompt="Use the live discovery probe Skills now.",
                )
                self.assertNotIn(secret, " ".join(command))
                self.assertNotIn(PROMPT_TOKEN, command)

    def test_the_omp_case_leaves_the_repository_checkout_untouched(self) -> None:
        checkout = Path(__file__).resolve().parents[2]
        before = self.checkout_snapshot()
        with tempfile.TemporaryDirectory() as temporary:
            root = Path(temporary)
            (root / "evidence").mkdir()
            self.omp_stub_run(root, root / "argv.txt", "sk-omp-unit-credential-value")

            project = root / "project"
            destination = project / AGENT_CASES["omp"].destination
            self.assertTrue(destination.is_relative_to(project))
            self.assertFalse(destination.is_relative_to(checkout))
            self.assertTrue((root / "omp-state").is_relative_to(root))
            self.assertTrue((root / "evidence" / "omp.stdout.log").is_file())
        self.assertEqual(self.checkout_snapshot(), before)


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
