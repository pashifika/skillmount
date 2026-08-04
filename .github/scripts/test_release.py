#!/usr/bin/env python3
"""Behavioral tests for deterministic SkillMount release preparation."""

from __future__ import annotations

import os
import shutil
import subprocess
import tarfile
import tempfile
import unittest
import zipfile
from pathlib import Path

import release

VERSION = "0.1.0"
TAG = "v0.1.0"
COMMIT = "a" * 40


class ReleaseTests(unittest.TestCase):
    """Cover preflight, native package layout, and aggregate integrity."""

    def setUp(self) -> None:
        """Create minimal repository and product-binary fixtures."""

        self.temporary = tempfile.TemporaryDirectory()
        self.root = Path(self.temporary.name)
        self.repository = self.root / "repository"
        self.repository.mkdir()
        (self.repository / "LICENSE-APACHE").write_text(
            "Apache License fixture\n", encoding="utf-8"
        )
        (self.repository / "LICENSE-MIT").write_text(
            "MIT License fixture\n", encoding="utf-8"
        )
        self.binary_directories: dict[str, Path] = {}
        for target in release.TARGETS:
            binary_directory = self.root / "binaries" / target.triple
            binary_directory.mkdir(parents=True)
            for executable in release.executable_names(target):
                binary = binary_directory / executable
                binary.write_bytes(f"binary:{target.triple}:{executable}\n".encode())
                binary.chmod(0o755)
            self.binary_directories[target.triple] = binary_directory

    def tearDown(self) -> None:
        """Remove the isolated fixture tree."""

        self.temporary.cleanup()

    def package_all(self, root: Path) -> list[Path]:
        """Package every target into distinct workflow-artifact directories."""

        archives = []
        for target in release.TARGETS:
            output = root / f"release-package-{target.triple}"
            archives.append(
                release.package_release(
                    self.repository,
                    self.binary_directories[target.triple],
                    output,
                    target=target,
                    version=VERSION,
                    tag=TAG,
                    commit=COMMIT,
                )
            )
        return archives

    def test_exact_stable_tag_validation(self) -> None:
        """Accept stable SemVer tags and reject every broader trigger shape."""

        self.assertEqual(release.stable_version_from_tag("v0.1.0"), "0.1.0")
        self.assertEqual(release.stable_version_from_tag("v12.34.56"), "12.34.56")
        for malformed in (
            "v1",
            "1.2.3",
            "v1.2",
            "v01.2.3",
            "v1.02.3",
            "v1.2.03",
            "v1.2.3-beta.1",
            "v1.2.3+build.1",
            "v1.2.3\nignored",
        ):
            with self.subTest(tag=malformed):
                with self.assertRaises(release.ReleaseError):
                    release.stable_version_from_tag(malformed)

    def test_tag_preflight_requires_matching_version_commit_and_main_ancestry(self) -> None:
        """Publish only an exact tag bound to the checked-out main-history commit."""

        result = release.evaluate_preflight(
            event_name="push",
            ref_name=TAG,
            commit=COMMIT,
            package_version=VERSION,
            tag_commit=COMMIT,
            main_contains_commit=True,
            workflow_files_match_main=True,
        )
        self.assertTrue(result.publish)
        self.assertEqual(result.tag, TAG)

        invalid_cases = (
            {
                "ref_name": "v0.2.0",
                "tag_commit": COMMIT,
                "main_contains_commit": True,
                "workflow_files_match_main": True,
            },
            {
                "ref_name": TAG,
                "tag_commit": "b" * 40,
                "main_contains_commit": True,
                "workflow_files_match_main": True,
            },
            {
                "ref_name": TAG,
                "tag_commit": COMMIT,
                "main_contains_commit": False,
                "workflow_files_match_main": True,
            },
            {
                "ref_name": TAG,
                "tag_commit": COMMIT,
                "main_contains_commit": True,
                "workflow_files_match_main": False,
            },
        )
        for case in invalid_cases:
            with self.subTest(case=case):
                with self.assertRaises(release.ReleaseError):
                    release.evaluate_preflight(
                        event_name="push",
                        commit=COMMIT,
                        package_version=VERSION,
                        **case,
                    )

    def test_manual_preflight_is_read_only_for_non_tag_refs(self) -> None:
        """Allow selected manual refs without ever making them publishable."""

        for selected_ref in ("main", "dev/0.1.x", COMMIT):
            with self.subTest(selected_ref=selected_ref):
                result = release.evaluate_preflight(
                    event_name="workflow_dispatch",
                    ref_name=selected_ref,
                    commit=COMMIT,
                    package_version=VERSION,
                    tag_commit=None,
                    main_contains_commit=None,
                    workflow_files_match_main=None,
                )
                self.assertFalse(result.publish)
                self.assertEqual(result.tag, TAG)
                self.assertEqual(result.commit, COMMIT)

    def test_workflow_outputs_expose_only_validated_fixed_targets(self) -> None:
        """Emit version, commit, publish policy, and the explicit three-row matrix."""

        outputs = release.workflow_outputs(
            release.PreflightResult(VERSION, TAG, COMMIT, False)
        )
        self.assertEqual(outputs["version"], VERSION)
        self.assertEqual(outputs["tag"], TAG)
        self.assertEqual(outputs["commit"], COMMIT)
        self.assertEqual(outputs["publish"], "false")
        self.assertEqual(
            outputs["matrix"],
            '{"include":[{"host":"x86_64-pc-windows-msvc",'
            '"name":"windows-x64","runner":"windows-2025",'
            '"runner_arch":"X64","target":"x86_64-pc-windows-msvc"},'
            '{"host":"x86_64-pc-windows-msvc","name":"windows-x86",'
            '"runner":"windows-2025","runner_arch":"X64",'
            '"target":"i686-pc-windows-msvc"},{"host":"aarch64-apple-darwin",'
            '"name":"macos-arm64","runner":"macos-15","runner_arch":"ARM64",'
            '"target":"aarch64-apple-darwin"}]}'
        )

    def test_real_git_ancestry_and_workflow_tree_checks_are_independent(self) -> None:
        """Allow unrelated main movement while rejecting workflow divergence."""

        repository = self.root / "git-repository"
        repository.mkdir()
        self.run_git(repository, "init", "-b", "main")
        self.run_git(repository, "config", "user.name", "Release Test")
        self.run_git(repository, "config", "user.email", "release@example.invalid")
        tracked = repository / "tracked.txt"
        tracked.write_text("base\n", encoding="utf-8")
        workflow = repository / ".github" / "workflows" / "release.yml"
        workflow.parent.mkdir(parents=True)
        workflow.write_text("name: Release\n", encoding="utf-8")
        self.run_git(repository, "add", "tracked.txt", ".github/workflows/release.yml")
        self.run_git(repository, "commit", "-m", "base")
        base = self.run_git(repository, "rev-parse", "HEAD")
        tracked.write_text("main\n", encoding="utf-8")
        self.run_git(repository, "commit", "-am", "main")
        main_tip = self.run_git(repository, "rev-parse", "HEAD")
        self.run_git(repository, "switch", "-c", "topic", base)
        tracked.write_text("topic\n", encoding="utf-8")
        workflow.write_text("name: Diverged release\n", encoding="utf-8")
        self.run_git(repository, "commit", "-am", "topic")
        topic_tip = self.run_git(repository, "rev-parse", "HEAD")

        self.assertTrue(release.git_is_ancestor(repository, base, main_tip))
        self.assertFalse(release.git_is_ancestor(repository, topic_tip, main_tip))
        self.assertTrue(
            release.git_paths_match(
                repository, base, main_tip, ".github/workflows"
            )
        )
        self.assertFalse(
            release.git_paths_match(
                repository, topic_tip, main_tip, ".github/workflows"
            )
        )

    def test_asset_names_and_archive_layout_are_exact(self) -> None:
        """Build target-specific archives with one predictable top-level directory."""

        archives = self.package_all(self.root / "packages")
        self.assertEqual(
            sorted(archive.name for archive in archives),
            list(release.expected_archive_names(TAG)),
        )
        for target, archive in zip(release.TARGETS, archives, strict=True):
            release.inspect_archive(
                archive,
                target=target,
                version=VERSION,
                tag=TAG,
                commit=COMMIT,
            )

    def test_repackaging_is_byte_deterministic(self) -> None:
        """Normalize ZIP and tar.gz bytes for the same validated inputs."""

        for target in release.TARGETS:
            with self.subTest(target=target.triple):
                first = release.package_release(
                    self.repository,
                    self.binary_directories[target.triple],
                    self.root / "first" / target.name,
                    target=target,
                    version=VERSION,
                    tag=TAG,
                    commit=COMMIT,
                )
                second = release.package_release(
                    self.repository,
                    self.binary_directories[target.triple],
                    self.root / "second" / target.name,
                    target=target,
                    version=VERSION,
                    tag=TAG,
                    commit=COMMIT,
                )
                self.assertEqual(first.read_bytes(), second.read_bytes())

    def test_windows_archives_cover_both_requested_msvc_targets(self) -> None:
        """Produce normalized ZIP packages for Windows x64 and x86."""

        windows_targets = [target for target in release.TARGETS if target.extension == ".zip"]
        self.assertEqual(
            [target.triple for target in windows_targets],
            ["x86_64-pc-windows-msvc", "i686-pc-windows-msvc"],
        )
        for target in windows_targets:
            archive = release.package_release(
                self.repository,
                self.binary_directories[target.triple],
                self.root / "windows" / target.name,
                target=target,
                version=VERSION,
                tag=TAG,
                commit=COMMIT,
            )
            with zipfile.ZipFile(archive) as package:
                root = release.asset_stem(TAG, target)
                for executable in ("asm.exe", "skillmount.exe"):
                    member = package.getinfo(f"{root}/{executable}")
                    self.assertEqual((member.external_attr >> 16) & 0o777, 0o755)

    def test_macos_archive_preserves_executable_permissions(self) -> None:
        """Preserve executable modes in the Apple Silicon tar.gz package."""

        target = release.target_for("aarch64-apple-darwin")
        archive = release.package_release(
            self.repository,
            self.binary_directories[target.triple],
            self.root / "macos",
            target=target,
            version=VERSION,
            tag=TAG,
            commit=COMMIT,
        )
        with tarfile.open(archive, mode="r:gz") as package:
            root = release.asset_stem(TAG, target)
            for executable in ("asm", "skillmount"):
                self.assertEqual(package.getmember(f"{root}/{executable}").mode, 0o755)

    def test_archive_inspection_rejects_unrelated_or_unsafe_content(self) -> None:
        """Reject caches, debug files, unrelated files, absolute paths, and traversal."""

        target = release.target_for("x86_64-pc-windows-msvc")
        archive = release.package_release(
            self.repository,
            self.binary_directories[target.triple],
            self.root / "valid",
            target=target,
            version=VERSION,
            tag=TAG,
            commit=COMMIT,
        )
        root = release.asset_stem(TAG, target)
        for index, member in enumerate(
            (
                f"{root}/target/cache.bin",
                f"{root}/debug.pdb",
                "unrelated.txt",
                "/absolute.txt",
                "../escape.txt",
            )
        ):
            with self.subTest(member=member):
                malicious = self.root / f"malicious-{index}" / archive.name
                malicious.parent.mkdir()
                shutil.copyfile(archive, malicious)
                with zipfile.ZipFile(malicious, mode="a") as package:
                    package.writestr(member, b"unexpected")
                with self.assertRaises(release.ReleaseError):
                    release.inspect_archive(
                        malicious,
                        target=target,
                        version=VERSION,
                        tag=TAG,
                        commit=COMMIT,
                    )

    def test_aggregate_requires_exactly_three_archives_and_verifies_checksums(self) -> None:
        """Assemble only the complete target set and verify deterministic SHA-256 lines."""

        input_directory = self.root / "workflow-artifacts"
        self.package_all(input_directory)
        output_directory = self.root / "verified"
        release.aggregate_release(
            input_directory,
            output_directory,
            version=VERSION,
            tag=TAG,
            commit=COMMIT,
        )
        release.verify_release_set(
            output_directory,
            version=VERSION,
            tag=TAG,
            commit=COMMIT,
        )
        checksum_names = list(
            release.parse_checksum_file(output_directory / release.CHECKSUM_FILE)
        )
        self.assertEqual(checksum_names, list(release.expected_archive_names(TAG)))

    def test_aggregate_rejects_missing_duplicate_and_extra_archives(self) -> None:
        """Fail closed on every incomplete or ambiguous workflow-artifact set."""

        source = self.root / "source-artifacts"
        archives = self.package_all(source)

        missing = self.root / "missing"
        shutil.copytree(source, missing)
        next(missing.rglob(archives[0].name)).unlink()

        duplicate = self.root / "duplicate"
        shutil.copytree(source, duplicate)
        duplicate_directory = duplicate / "duplicate-artifact"
        duplicate_directory.mkdir()
        shutil.copyfile(archives[0], duplicate_directory / archives[0].name)

        extra = self.root / "extra"
        shutil.copytree(source, extra)
        (extra / "debug-symbols.pdb").write_bytes(b"debug")

        for index, input_directory in enumerate((missing, duplicate, extra)):
            with self.subTest(input_directory=input_directory.name):
                with self.assertRaises(release.ReleaseError):
                    release.aggregate_release(
                        input_directory,
                        self.root / f"rejected-{index}",
                        version=VERSION,
                        tag=TAG,
                        commit=COMMIT,
                    )

    def test_release_set_rejects_tampered_archive(self) -> None:
        """Detect archive mutation even when the expected filename remains unchanged."""

        input_directory = self.root / "tamper-input"
        self.package_all(input_directory)
        output_directory = self.root / "tamper-output"
        release.aggregate_release(
            input_directory,
            output_directory,
            version=VERSION,
            tag=TAG,
            commit=COMMIT,
        )
        archive = output_directory / release.expected_archive_names(TAG)[0]
        with archive.open("ab") as output:
            output.write(b"tampered")
        with self.assertRaises(release.ReleaseError):
            release.verify_release_set(
                output_directory,
                version=VERSION,
                tag=TAG,
                commit=COMMIT,
            )

    @staticmethod
    def run_git(repository: Path, *arguments: str) -> str:
        """Run a deterministic local Git fixture command."""

        completed = subprocess.run(
            ("git", *arguments),
            cwd=repository,
            stdout=subprocess.PIPE,
            stderr=subprocess.PIPE,
            text=True,
            check=False,
        )
        if completed.returncode != 0:
            raise AssertionError(
                f"git {' '.join(arguments)} failed: {completed.stderr.strip()}"
            )
        return completed.stdout.strip()


class ReleaseSmokeTests(unittest.TestCase):
    """Exercise parity checks with real executable fixture shims on Unix."""

    @unittest.skipIf(os.name == "nt", "POSIX shebang fixture is not a Windows executable")
    def test_both_executable_names_report_the_validated_version(self) -> None:
        """Reject a fallback executable whose observable version diverges."""

        with tempfile.TemporaryDirectory() as temporary:
            binary_directory = Path(temporary)
            for executable in ("asm", "skillmount"):
                path = binary_directory / executable
                path.write_text(
                    "#!/usr/bin/env python3\nprint('SkillMount 0.1.0')\n",
                    encoding="utf-8",
                )
                path.chmod(0o755)
            release.smoke_pair(
                binary_directory,
                release.target_for("aarch64-apple-darwin"),
                VERSION,
            )
            (binary_directory / "skillmount").write_text(
                "#!/usr/bin/env python3\nprint('SkillMount 9.9.9')\n",
                encoding="utf-8",
            )
            with self.assertRaises(release.ReleaseError):
                release.smoke_pair(
                    binary_directory,
                    release.target_for("aarch64-apple-darwin"),
                    VERSION,
                )


if __name__ == "__main__":
    unittest.main()
