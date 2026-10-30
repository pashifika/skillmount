#!/usr/bin/env python3
"""Regression tests for reversible CI core-tap isolation."""

import tempfile
import unittest
from pathlib import Path
from unittest import mock

from provision_native_shells import isolated_core, main


class IsolatedCoreTests(unittest.TestCase):
    def test_original_dirty_files_restored_after_success_or_failure(self):
        for fail in (False, True):
            with self.subTest(fail=fail), tempfile.TemporaryDirectory() as temporary:
                root = Path(temporary)
                original, prepared, storage = root / "core", root / "clone", root / "storage"
                for path in (original, prepared, storage):
                    path.mkdir()
                (original / "rustup.rb").write_bytes(b"runner local edit\n")
                (original / "untracked").write_bytes(b"keep me\n")
                (prepared / "rustup.rb").write_bytes(b"pinned formula\n")
                identity = original.stat().st_ino
                def exercise():
                    with isolated_core(original, prepared, storage):
                        self.assertEqual((original / "rustup.rb").read_bytes(), b"pinned formula\n")
                        self.assertFalse((original / "untracked").exists())
                        if fail:
                            raise RuntimeError("install failed")
                if fail:
                    with self.assertRaisesRegex(RuntimeError, "install failed"):
                        exercise()
                else:
                    exercise()
                self.assertEqual(original.stat().st_ino, identity)
                self.assertEqual((original / "rustup.rb").read_bytes(), b"runner local edit\n")
                self.assertEqual((original / "untracked").read_bytes(), b"keep me\n")
                self.assertTrue((storage / "used-core").is_dir())

    def test_existing_recovery_destination_is_preserved(self):
        with tempfile.TemporaryDirectory() as temporary:
            root = Path(temporary)
            for name in ("core", "clone", "original-core"):
                (root / name).mkdir()
            with self.assertRaisesRegex(RuntimeError, "existing recovery path"):
                with isolated_core(root / "core", root / "clone", root):
                    self.fail("must not enter")
            self.assertTrue((root / "core").is_dir())
            self.assertTrue((root / "clone").is_dir())

    def test_failed_initial_swap_restores_original(self):
        with tempfile.TemporaryDirectory() as temporary:
            root = Path(temporary)
            original, prepared, storage = root / "core", root / "clone", root / "storage"
            for path in (original, prepared, storage):
                path.mkdir()
            (original / "dirty").write_text("preserve")
            real_rename = Path.rename
            def rename(path, destination):
                if path == prepared:
                    raise OSError("swap failed")
                return real_rename(path, destination)
            with mock.patch.object(Path, "rename", rename):
                with self.assertRaisesRegex(OSError, "swap failed"):
                    with isolated_core(original, prepared, storage):
                        self.fail("must not enter")
            self.assertEqual((original / "dirty").read_text(), "preserve")

    def test_replaced_tap_is_not_overwritten_during_recovery(self):
        with tempfile.TemporaryDirectory() as temporary:
            root = Path(temporary)
            original, prepared, storage = root / "core", root / "clone", root / "storage"
            for path in (original, prepared, storage):
                path.mkdir()
            (original / "dirty").write_text("preserve")
            with self.assertRaisesRegex(RuntimeError, "changed identity"):
                with isolated_core(original, prepared, storage):
                    original.rename(root / "displaced-clone")
                    original.mkdir()
                    (original / "unexpected").write_text("also preserve")
            self.assertEqual((original / "unexpected").read_text(), "also preserve")
            self.assertEqual((storage / "original-core" / "dirty").read_text(), "preserve")

    def test_ci_uses_isolation_and_requires_real_version_upgrade(self):
        workflow = (Path(__file__).resolve().parents[1] / "workflows" / "ci.yml").read_text()
        self.assertIn("python3 -B .github/scripts/provision_native_shells.py", workflow)
        self.assertIn("python .github/scripts/test_provision_native_shells.py", workflow)
        self.assertNotIn('git -C "$core" checkout', workflow)
        self.assertIn("--prior-tag v0.2.0 --require-upgrade", workflow)

    def test_local_execution_refused(self):
        with mock.patch.dict("os.environ", {}, clear=True):
            with self.assertRaisesRegex(RuntimeError, "disposable macOS Actions"):
                main()


if __name__ == "__main__":
    unittest.main()
