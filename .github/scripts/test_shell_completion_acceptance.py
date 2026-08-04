#!/usr/bin/env python3
"""Self-tests for the native shell completion acceptance harness."""

from __future__ import annotations

import io
import json
import tempfile
import unittest
from contextlib import redirect_stdout
from pathlib import Path
from unittest import mock

from shell_completion_acceptance import (
    CASE_ORDER,
    AcceptanceError,
    Fixture,
    completion_cases,
    emit,
    install_completion,
    observation_record,
    require_interpreter,
    main,
)


class ShellCompletionAcceptanceTests(unittest.TestCase):
    def test_case_order_and_observation_output_are_deterministic(self) -> None:
        names = ("syntax",) + tuple(
            case.name for case in completion_cases("asm", "bash")
        )
        self.assertEqual(names, CASE_ORDER)

        first = io.StringIO()
        second = io.StringIO()
        record = observation_record(
            "bash", "asm", "wrapper-enums", ("symlink", "auto", "junction", "auto")
        )
        with redirect_stdout(first):
            emit(record)
        with redirect_stdout(second):
            emit(record)
        self.assertEqual(first.getvalue(), second.getvalue())
        self.assertEqual(
            json.loads(first.getvalue())["candidates"],
            ["auto", "junction", "symlink"],
        )

    def test_every_installed_file_stays_inside_the_isolated_home(self) -> None:
        for shell in ("bash", "zsh", "fish", "powershell"):
            with self.subTest(shell=shell), Fixture(shell, "asm") as fixture:
                installation = install_completion(
                    shell,
                    "asm",
                    b"# generated completion\n",
                    fixture,
                    "/required/interpreter",
                )
                self.assertTrue(installation.script.is_relative_to(fixture.home))
                installed = [path for path in fixture.root.rglob("*") if path.is_file()]
                self.assertIn(installation.script, installed)
                self.assertTrue(
                    all(path.is_relative_to(fixture.root) for path in installed)
                )

    def test_fixture_cleanup_removes_only_its_owned_tree(self) -> None:
        with tempfile.TemporaryDirectory() as temporary:
            parent = Path(temporary)
            sibling = parent / "operator-owned"
            sibling.write_text("keep\n", encoding="utf-8")
            with Fixture("bash", "asm", parent=parent) as fixture:
                owned = fixture.root
                (owned / "owned-sentinel").write_text("remove\n", encoding="utf-8")
                self.assertTrue(owned.exists())
            self.assertFalse(owned.exists())
            self.assertEqual(sibling.read_text(encoding="utf-8"), "keep\n")

    def test_unavailable_advertised_interpreter_fails_closed(self) -> None:
        with mock.patch("shell_completion_acceptance.shutil.which", return_value=None):
            with self.assertRaisesRegex(AcceptanceError, "required-interpreter"):
                require_interpreter("fish")

    def test_duplicate_shell_request_fails_before_executables_are_touched(self) -> None:
        with self.assertRaisesRegex(AcceptanceError, "exactly once"):
            main(
                (
                    "--asm",
                    "missing-asm",
                    "--skillmount",
                    "missing-skillmount",
                    "--target",
                    "aarch64-apple-darwin",
                    "--shell",
                    "bash",
                    "--shell",
                    "bash",
                )
            )


if __name__ == "__main__":
    unittest.main()
