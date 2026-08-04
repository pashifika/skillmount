#!/usr/bin/env python3
"""Self-tests for the native Chocolatey acceptance harness's decision layer."""

from __future__ import annotations

import io
import json
import os
import re
import tempfile
import unittest
import zipfile
from contextlib import redirect_stderr, redirect_stdout
from dataclasses import dataclass
from pathlib import Path
from unittest import mock

import chocolatey_acceptance as harness
import release

VERSION = "0.2.0"
TAG = "v0.2.0"
COMMIT = "b" * 40
REPOSITORY = "pashifika/skillmount"
CHOCO_ROOT = r"C:\ProgramData\chocolatey"
LIB = rf"{CHOCO_ROOT}\lib"
BIN = rf"{CHOCO_ROOT}\bin"
# The planning store is machine-local and root-ignored, so it is absent from a CI checkout. The
# scenario-mapping test resolves it lazily and skips rather than failing when it is not present.
SPEC_GLOB = "rasen/**/chocolatey-distribution/spec.md"


def spec_path() -> Path:
    """Return the `chocolatey-distribution` spec in this checkout, or skip."""

    candidates = sorted(Path(__file__).resolve().parents[2].glob(SPEC_GLOB))
    if not candidates:
        raise unittest.SkipTest("chocolatey-distribution/spec.md is not in this checkout")
    return candidates[0]


SKILLMOUNT = harness.PackageSelection(
    package_id="skillmount",
    command="skillmount",
    selected_executable="skillmount.exe",
    other_command="asm",
    other_executable="asm.exe",
)
ASM = harness.PackageSelection(
    package_id="skillmount-asm",
    command="asm",
    selected_executable="asm.exe",
    other_command="skillmount",
    other_executable="skillmount.exe",
)


class FakeIdentity:
    """One selection-map entry shaped like `package_channels.PackageIdentity`."""

    def __init__(self, package_id: str, command: str) -> None:
        self.package_id = package_id
        self.command = command
        self.other: FakeIdentity | None = None

    @property
    def windows_executable(self) -> str:
        """Return the Windows executable this entry selects."""

        return f"{self.command}.exe"


class FakeChannels:
    """The subset of `package_channels` the orchestration reads."""

    def __init__(self) -> None:
        first = FakeIdentity("skillmount", "skillmount")
        second = FakeIdentity("skillmount-asm", "asm")
        first.other = second
        second.other = first
        self.PACKAGES = (first, second)


@dataclass(frozen=True)
class FakeArchive:
    """One archive identity shaped like `package_channels.ArchiveIdentity`."""

    triple: str
    name: str
    url: str
    sha256: str


@dataclass(frozen=True)
class FakeInputs:
    """The package-inputs fields `replace_archive` rewrites."""

    version: str
    archives: tuple[FakeArchive, ...]


def pe_bytes(machine: int, *, offset: int = 0x80, signature: bytes = b"PE\0\0") -> bytes:
    """Build a synthetic DOS/PE header prefix reporting one COFF machine type."""

    header = bytearray(offset + 8)
    header[0:2] = b"MZ"
    header[0x3C:0x40] = offset.to_bytes(4, "little")
    header[offset : offset + 4] = signature
    header[offset + 4 : offset + 6] = machine.to_bytes(2, "little")
    return bytes(header)


def package_folder(
    root: Path,
    selection: harness.PackageSelection,
    *,
    machine: int = harness.MACHINE_AMD64,
    extra: tuple[str, ...] = (),
    drop: tuple[str, ...] = (),
) -> Path:
    """Materialize one Chocolatey package folder fixture on disk."""

    folder = root / selection.package_id
    names = [
        f"{selection.package_id}.nuspec",
        f"{selection.package_id}.nupkg",
        "tools/chocolateyinstall.ps1",
        "tools/VERSION",
        "tools/LICENSE-APACHE",
        "tools/LICENSE-MIT",
        "tools/VERIFICATION.txt",
        f"tools/{selection.selected_executable}",
        *extra,
    ]
    for name in names:
        if name in drop:
            continue
        path = folder / name
        path.parent.mkdir(parents=True, exist_ok=True)
        if name.endswith(".exe"):
            path.write_bytes(pe_bytes(machine))
        else:
            path.write_text(f"{name}\n", encoding="utf-8")
    return folder


class SafetyRefusalTests(unittest.TestCase):
    """The two refusals that keep an unconsenting host untouched."""

    def test_opt_in_is_required_and_exact(self) -> None:
        for value in (None, "", "0", "true", "yes", "11"):
            environment = {} if value is None else {harness.ACCEPTANCE_VARIABLE: value}
            with self.assertRaises(harness.ChocolateyAcceptanceError) as caught:
                harness.require_opt_in(environment)
            self.assertIn(harness.ACCEPTANCE_VARIABLE, str(caught.exception))
            self.assertIn(repr(value), str(caught.exception))
        harness.require_opt_in({harness.ACCEPTANCE_VARIABLE: "1"})

    def test_main_without_opt_in_exits_nonzero_and_never_reaches_choco(self) -> None:
        class ForbiddenGateway:
            def __init__(self, *arguments: object, **keywords: object) -> None:
                raise AssertionError("the harness located choco without an opt-in")

        stderr = io.StringIO()
        with mock.patch.dict(os.environ, {}, clear=False):
            os.environ.pop(harness.ACCEPTANCE_VARIABLE, None)
            with mock.patch.object(harness, "ChocoGateway", ForbiddenGateway):
                with mock.patch.object(
                    harness, "load_channels", side_effect=AssertionError("rendered too early")
                ):
                    with redirect_stderr(stderr):
                        status = harness.main(["--tag", TAG])
        self.assertEqual(status, 1)
        message = stderr.getvalue()
        self.assertIn(harness.ACCEPTANCE_VARIABLE, message)
        self.assertIn("refusing to run", message)
        self.assertNotIn("package_channels", message)

    def test_a_preinstalled_package_refuses_before_any_lifecycle_command(self) -> None:
        class RecordingGateway:
            """A gateway that records anything beyond the safety query."""

            def __init__(self, *arguments: object, **keywords: object) -> None:
                self.root = Path(CHOCO_ROOT)
                self.environment: dict[str, str] = {}
                self.calls: list[str] = []

            def list_local(self) -> str:
                self.calls.append("list")
                return "Chocolatey v2.4.1\nskillmount-asm 0.1.0\n1 packages installed.\n"

            def __getattr__(self, name: str) -> object:
                raise AssertionError(f"the harness called {name} on an unclean host")

        gateway = RecordingGateway()
        options = harness.argument_parser().parse_args(["--tag", TAG])
        with mock.patch.dict(os.environ, {harness.ACCEPTANCE_VARIABLE: "1"}):
            with mock.patch.object(harness, "ChocoGateway", lambda: gateway):
                with mock.patch.object(harness, "load_channels", return_value=FakeChannels()):
                    with self.assertRaises(harness.ChocolateyAcceptanceError) as caught:
                        harness.run_acceptance(options, Path(CHOCO_ROOT))
        self.assertIn("already reports", str(caught.exception))
        self.assertIn("skillmount-asm==0.1.0", str(caught.exception))
        self.assertEqual(gateway.calls, ["list"])

    def test_require_clean_host_accepts_an_unrelated_package(self) -> None:
        harness.require_clean_host(
            "Chocolatey v2.4.1\nsevenzip 24.9.0\ngit 2.47.0\n2 packages installed.\n",
            ("skillmount", "skillmount-asm"),
        )

    def test_a_missing_choco_fails_closed_at_the_gateway_boundary(self) -> None:
        with mock.patch.object(harness.shutil, "which", return_value=None):
            with self.assertRaisesRegex(harness.ChocolateyAcceptanceError, "choco is required"):
                harness.ChocoGateway(environment={})


class InstalledPackageParsingTests(unittest.TestCase):
    """`choco list` text is the only source of installed-package truth."""

    def test_versions_are_indexed_case_insensitively(self) -> None:
        text = (
            "Chocolatey v2.4.1\n"
            "SkillMount 0.2.0\n"
            "skillmount-asm 0.2.0\n"
            "chocolatey 2.4.1\n"
            "3 packages installed.\n"
        )
        self.assertEqual(
            harness.parse_installed_packages(text),
            {"skillmount": "0.2.0", "skillmount-asm": "0.2.0", "chocolatey": "2.4.1"},
        )
        self.assertEqual(
            harness.preexisting_installations(text, ("skillmount", "skillmount-asm")),
            ("skillmount==0.2.0", "skillmount-asm==0.2.0"),
        )

    def test_headers_footers_and_prose_are_ignored(self) -> None:
        self.assertEqual(
            harness.parse_installed_packages(
                "Chocolatey v2.4.1\nDid you know Pro / Business automatically syncs?\n"
                "0 packages installed.\n"
            ),
            {},
        )

    def test_installed_version_finding_names_expected_and_observed(self) -> None:
        ok = harness.installed_version_finding(
            "skillmount 0.2.0\n", package_id="skillmount", expected_version=VERSION
        )
        self.assertTrue(ok.ok)
        bad = harness.installed_version_finding(
            "skillmount 0.1.0\n", package_id="skillmount", expected_version=VERSION
        )
        self.assertFalse(bad.ok)
        self.assertIn("0.2.0", bad.detail)
        self.assertIn("0.1.0", bad.detail)
        absent = harness.installed_version_finding(
            "", package_id="skillmount", expected_version=VERSION
        )
        self.assertFalse(absent.ok)
        self.assertIn("None", absent.detail)


class PortableExecutableTests(unittest.TestCase):
    """The retained executable's architecture is read from bytes, not from its name."""

    def test_both_machine_types_are_parsed_from_bytes(self) -> None:
        self.assertEqual(harness.pe_machine(pe_bytes(harness.MACHINE_AMD64)), 0x8664)
        self.assertEqual(harness.pe_machine(pe_bytes(harness.MACHINE_I386)), 0x014C)
        self.assertEqual(harness.machine_name(0x8664), "x64")
        self.assertEqual(harness.machine_name(0x014C), "x86")
        self.assertEqual(harness.machine_name(0xAA64), "unknown")
        self.assertEqual(harness.architecture_machine("x64"), 0x8664)
        self.assertEqual(harness.architecture_machine("x86"), 0x014C)
        with self.assertRaisesRegex(harness.ChocolateyAcceptanceError, "unsupported architecture"):
            harness.architecture_machine("arm64")

    def test_a_truncated_or_invalid_header_is_rejected(self) -> None:
        with self.assertRaisesRegex(harness.ChocolateyAcceptanceError, "at least 64 bytes"):
            harness.pe_machine(b"MZ")
        with self.assertRaisesRegex(harness.ChocolateyAcceptanceError, "DOS signature"):
            harness.pe_machine(bytes(128))
        with self.assertRaisesRegex(harness.ChocolateyAcceptanceError, "PE signature"):
            harness.pe_machine(pe_bytes(harness.MACHINE_AMD64, signature=b"NE\0\0"))
        truncated = pe_bytes(harness.MACHINE_AMD64)[:0x50]
        with self.assertRaisesRegex(harness.ChocolateyAcceptanceError, "only 80 were inspected"):
            harness.pe_machine(truncated)

    def test_machine_findings_report_the_mismatch_instead_of_raising(self) -> None:
        good = harness.machine_finding(
            pe_bytes(harness.MACHINE_AMD64), architecture="x64", label="tools/skillmount.exe"
        )
        self.assertTrue(good.ok)
        self.assertIn("0x8664", good.detail)
        wrong = harness.machine_finding(
            pe_bytes(harness.MACHINE_I386), architecture="x64", label="tools/skillmount.exe"
        )
        self.assertFalse(wrong.ok)
        self.assertIn("expected machine 0x8664 (x64)", wrong.detail)
        self.assertIn("observed 0x014c (x86)", wrong.detail)
        broken = harness.machine_finding(b"MZ", architecture="x86", label="tools/asm.exe")
        self.assertFalse(broken.ok)
        self.assertIn("tools/asm.exe", broken.detail)

    def test_only_a_bounded_header_prefix_is_read_from_disk(self) -> None:
        with tempfile.TemporaryDirectory() as temporary:
            path = Path(temporary) / "asm.exe"
            path.write_bytes(pe_bytes(harness.MACHINE_I386) + b"\xff" * (4 * 1024 * 1024))
            header = harness.read_header(path)
            self.assertEqual(len(header), harness.PE_HEADER_LIMIT)
            self.assertEqual(harness.pe_machine(header), 0x014C)


class PackageFolderTests(unittest.TestCase):
    """The completed package folder must hold exactly its own selected content."""

    def test_member_normalization_rejects_absolute_and_escaping_entries(self) -> None:
        self.assertEqual(harness.normalize_member("tools\\VERSION"), "tools/VERSION")
        self.assertEqual(harness.normalize_member(' "tools/VERSION" '), "tools/VERSION")
        self.assertEqual(harness.normalize_member("./tools/VERSION"), "tools/VERSION")
        for bad in (rf"{LIB}\skillmount\tools\VERSION", "..\\escape", "", "   "):
            with self.assertRaises(harness.ChocolateyAcceptanceError):
                harness.normalize_member(bad)

    def test_listing_index_folds_windows_case(self) -> None:
        indexed = harness.listing_index(("Tools/VERSION", "tools/version"))
        self.assertEqual(indexed, {"tools/version": "Tools/VERSION"})

    def test_a_correct_package_folder_satisfies_every_check(self) -> None:
        with tempfile.TemporaryDirectory() as temporary:
            folder = package_folder(Path(temporary), SKILLMOUNT)
            names = harness.listing(folder)
            self.assertIsNotNone(names)
            findings = harness.package_folder_findings(names, SKILLMOUNT, version=VERSION)
            self.assertEqual([item.check for item in findings if not item.ok], [])
            self.assertEqual(
                harness.required_package_files(SKILLMOUNT),
                (
                    "skillmount.nuspec",
                    "tools/LICENSE-APACHE",
                    "tools/LICENSE-MIT",
                    "tools/VERIFICATION.txt",
                    "tools/VERSION",
                    "tools/chocolateyinstall.ps1",
                    "tools/skillmount.exe",
                ),
            )
            self.assertIn(
                "tools/chocolateyuninstall.ps1",
                harness.optional_package_files(SKILLMOUNT, VERSION),
            )

    def test_a_retained_unselected_executable_is_rejected(self) -> None:
        with tempfile.TemporaryDirectory() as temporary:
            folder = package_folder(Path(temporary), SKILLMOUNT, extra=("tools/asm.exe",))
            findings = harness.package_folder_findings(
                harness.listing(folder), SKILLMOUNT, version=VERSION
            )
            failed = {item.check for item in findings if not item.ok}
            self.assertEqual(
                failed,
                {"package-file-set", "unselected-executable-absent", "foreign-executable-absent"},
            )
            detail = next(
                item.detail for item in findings if item.check == "unselected-executable-absent"
            )
            self.assertIn("asm.exe", detail)

    def test_an_ignore_marker_is_never_a_substitute_for_removal(self) -> None:
        with tempfile.TemporaryDirectory() as temporary:
            folder = package_folder(
                Path(temporary),
                SKILLMOUNT,
                extra=("tools/asm.exe", "tools/asm.exe.ignore"),
            )
            findings = harness.package_folder_findings(
                harness.listing(folder), SKILLMOUNT, version=VERSION
            )
            failed = {item.check for item in findings if not item.ok}
            self.assertIn("ignore-marker-absent", failed)
            self.assertIn("unselected-executable-absent", failed)

    def test_a_missing_required_member_is_named(self) -> None:
        with tempfile.TemporaryDirectory() as temporary:
            folder = package_folder(
                Path(temporary), ASM, drop=("tools/VERIFICATION.txt", "tools/asm.exe")
            )
            findings = harness.package_folder_findings(
                harness.listing(folder), ASM, version=VERSION
            )
            detail = next(item.detail for item in findings if item.check == "package-file-set")
            self.assertIn("tools/VERIFICATION.txt", detail)
            self.assertIn("tools/asm.exe", detail)

    def test_extraction_residue_and_pending_markers_are_rejected(self) -> None:
        with tempfile.TemporaryDirectory() as temporary:
            folder = package_folder(
                Path(temporary),
                SKILLMOUNT,
                extra=(f"tools/skillmount-{TAG}-x86_64-pc-windows-msvc.zip", ".chocolateyPending"),
            )
            findings = harness.package_folder_findings(
                harness.listing(folder), SKILLMOUNT, version=VERSION
            )
            failed = {item.check for item in findings if not item.ok}
            self.assertIn("extraction-residue-absent", failed)

    def test_a_wrong_architecture_executable_passes_the_file_set_but_fails_the_machine_check(
        self,
    ) -> None:
        with tempfile.TemporaryDirectory() as temporary:
            folder = package_folder(
                Path(temporary), SKILLMOUNT, machine=harness.MACHINE_I386
            )
            findings = harness.package_folder_findings(
                harness.listing(folder), SKILLMOUNT, version=VERSION
            )
            self.assertEqual([item.check for item in findings if not item.ok], [])
            machine = harness.machine_finding(
                harness.read_header(folder / "tools" / "skillmount.exe"),
                architecture="x64",
                label="retained",
            )
            self.assertFalse(machine.ok)

    def test_an_absent_folder_is_distinguished_from_an_empty_one(self) -> None:
        with tempfile.TemporaryDirectory() as temporary:
            self.assertIsNone(harness.listing(Path(temporary) / "missing"))
            empty = Path(temporary) / "empty"
            empty.mkdir()
            self.assertEqual(harness.listing(empty), ())


class VersionMetadataTests(unittest.TestCase):
    """The retained release metadata proves which build the package kept."""

    def test_matching_metadata_passes_and_stale_metadata_is_named(self) -> None:
        target = harness.windows_target("x64")
        expected = release.version_metadata(VERSION, TAG, target, COMMIT)
        self.assertEqual(
            [item.ok for item in harness.version_file_findings(expected, expected=expected)],
            [True],
        )
        prior = release.version_metadata("0.1.0", "v0.1.0", target, COMMIT)
        findings = harness.version_file_findings(
            prior, expected=expected, forbidden=("0.1.0", "v0.1.0")
        )
        failed = {item.check for item in findings if not item.ok}
        self.assertEqual(failed, {"version-metadata", "version-metadata-not-stale"})

    def test_an_absent_metadata_file_fails_closed(self) -> None:
        findings = harness.version_file_findings(None, expected=b"x")
        self.assertEqual(len(findings), 1)
        self.assertFalse(findings[0].ok)
        self.assertIn("no VERSION file", findings[0].detail)


class ShimResolutionTests(unittest.TestCase):
    """Command-path ownership is judged from `where.exe` text and shim diagnostics."""

    def test_where_output_with_zero_one_and_two_matches(self) -> None:
        self.assertEqual(
            harness.parse_where_output("INFO: Could not find files for the given pattern(s).\n"),
            (),
        )
        self.assertEqual(
            harness.parse_where_output(f"{BIN}\\skillmount.exe\r\n"),
            (rf"{BIN}\skillmount.exe",),
        )
        two = harness.parse_where_output(
            f"{BIN}\\skillmount.exe\r\nC:\\tools\\skillmount.exe\r\n"
        )
        self.assertEqual(two, (rf"{BIN}\skillmount.exe", r"C:\tools\skillmount.exe"))
        self.assertEqual(
            harness.parse_where_output(f"{BIN}\\skillmount.exe\n{BIN.lower()}\\SKILLMOUNT.EXE\n"),
            (rf"{BIN}\skillmount.exe",),
        )
        with self.assertRaisesRegex(harness.ChocolateyAcceptanceError, "not absolute"):
            harness.parse_where_output("skillmount.exe\n")

    def test_only_the_exact_command_name_counts_as_a_product_shim(self) -> None:
        text = f"{BIN}\\skillmount.exe\n{BIN}\\skillmount-helper.exe\n{BIN}\\asm.exe\n"
        self.assertEqual(harness.resolved_shims(text, "skillmount"), (rf"{BIN}\skillmount.exe",))
        self.assertEqual(harness.resolved_shims(text, "asm"), (rf"{BIN}\asm.exe",))
        self.assertEqual(harness.resolved_shims(text, "missing"), ())

    def test_shim_target_is_taken_from_absolute_path_tokens_only(self) -> None:
        shim = rf"{BIN}\skillmount.exe"
        target = rf"{LIB}\skillmount\tools\skillmount.exe"
        text = (
            f"Shim: '{shim}'\r\n"
            f"Path to Executable: '{target}'\r\n"
            "Arguments: none\r\n"
        )
        self.assertEqual(
            harness.parse_shim_target(text, shim_path=shim, executable="skillmount.exe"), target
        )
        with self.assertRaisesRegex(harness.ChocolateyAcceptanceError, "exactly one shim target"):
            harness.parse_shim_target(
                f"Shim: '{shim}'\r\n", shim_path=shim, executable="skillmount.exe"
            )
        ambiguous = f"{target}\n{LIB}\\other\\tools\\skillmount.exe\n"
        with self.assertRaisesRegex(harness.ChocolateyAcceptanceError, "exactly one shim target"):
            harness.parse_shim_target(ambiguous, shim_path=shim, executable="skillmount.exe")

    def test_windows_containment_is_case_insensitive_and_boundary_aware(self) -> None:
        selected = rf"{LIB}\skillmount\tools\skillmount.exe"
        self.assertTrue(harness.windows_path_inside(selected, rf"{LIB}\skillmount"))
        self.assertTrue(
            harness.windows_path_inside(
                rf"{LIB.lower()}\SKILLMOUNT\TOOLS\skillmount.exe", rf"{LIB}\skillmount"
            )
        )
        self.assertFalse(
            harness.windows_path_inside(
                rf"{LIB}\skillmount-asm\tools\asm.exe", rf"{LIB}\skillmount"
            )
        )
        self.assertFalse(harness.windows_path_inside(rf"{LIB}\skillmount", rf"{LIB}\skillmount"))
        self.assertFalse(
            harness.windows_path_inside(r"C:\other\skillmount.exe", rf"{LIB}\skillmount")
        )
        with self.assertRaisesRegex(harness.ChocolateyAcceptanceError, "traversal"):
            harness.windows_path_inside(rf"{LIB}\skillmount\..\other", rf"{LIB}\skillmount")
        self.assertEqual(
            harness.folded_parts(rf"{BIN}\asm.exe"),
            ("c:", "programdata", "chocolatey", "bin", "asm.exe"),
        )

    def test_one_owned_shim_inside_the_package_satisfies_every_check(self) -> None:
        findings = harness.shim_findings(
            SKILLMOUNT,
            selected_where=f"{BIN}\\skillmount.exe\n",
            other_where="INFO: Could not find files for the given pattern(s).\n",
            shim_target=rf"{LIB}\skillmount\tools\skillmount.exe",
            package_folder=rf"{LIB}\skillmount",
            shim_directory=BIN,
        )
        self.assertEqual([item.check for item in findings if not item.ok], [])

    def test_two_product_shims_and_an_external_target_are_rejected(self) -> None:
        findings = harness.shim_findings(
            SKILLMOUNT,
            selected_where=f"{BIN}\\skillmount.exe\nC:\\tools\\skillmount.exe\n",
            other_where="",
            shim_target=r"C:\tools\skillmount.exe",
            package_folder=rf"{LIB}\skillmount",
            shim_directory=BIN,
        )
        failed = {item.check for item in findings if not item.ok}
        self.assertEqual(
            failed, {"shim-resolves-once", "shim-directory", "shim-target-inside-package"}
        )

    def test_a_pair_command_shim_owned_by_this_package_is_rejected(self) -> None:
        findings = harness.shim_findings(
            SKILLMOUNT,
            selected_where=f"{BIN}\\skillmount.exe\n",
            other_where=f"{LIB}\\skillmount\\tools\\asm.exe\n",
            shim_target=rf"{LIB}\skillmount\tools\skillmount.exe",
            package_folder=rf"{LIB}\skillmount",
            shim_directory=BIN,
        )
        failed = {item.check for item in findings if not item.ok}
        self.assertEqual(failed, {"pair-command-not-owned"})

    def test_a_pair_command_owned_by_its_own_package_is_accepted(self) -> None:
        findings = harness.shim_findings(
            SKILLMOUNT,
            selected_where=f"{BIN}\\skillmount.exe\n",
            other_where=f"{BIN}\\asm.exe\n",
            shim_target=rf"{LIB}\skillmount\tools\skillmount.exe",
            package_folder=rf"{LIB}\skillmount",
            shim_directory=BIN,
        )
        self.assertEqual([item.check for item in findings if not item.ok], [])

    def test_an_unresolvable_shim_target_fails_closed(self) -> None:
        findings = harness.shim_findings(
            ASM,
            selected_where=f"{BIN}\\asm.exe\n",
            other_where="",
            shim_target=None,
            package_folder=rf"{LIB}\skillmount-asm",
            shim_directory=BIN,
        )
        failed = {item.check for item in findings if not item.ok}
        self.assertEqual(failed, {"shim-target-inside-package"})

    def test_co_installed_packages_must_own_distinct_targets(self) -> None:
        ok = harness.independent_ownership_findings(
            SKILLMOUNT,
            ASM,
            left_target=rf"{LIB}\skillmount\tools\skillmount.exe",
            right_target=rf"{LIB}\skillmount-asm\tools\asm.exe",
            left_folder=rf"{LIB}\skillmount",
            right_folder=rf"{LIB}\skillmount-asm",
        )
        self.assertEqual([item.check for item in ok if not item.ok], [])
        shared = rf"{LIB}\skillmount\tools\skillmount.exe"
        bad = harness.independent_ownership_findings(
            SKILLMOUNT,
            ASM,
            left_target=shared,
            right_target=shared,
            left_folder=rf"{LIB}\skillmount",
            right_folder=rf"{LIB}\skillmount-asm",
        )
        failed = {item.check for item in bad if not item.ok}
        self.assertEqual(failed, {"distinct-shim-targets", "package-owned-targets"})
        missing = harness.independent_ownership_findings(
            SKILLMOUNT,
            ASM,
            left_target=None,
            right_target=shared,
            left_folder=rf"{LIB}\skillmount",
            right_folder=rf"{LIB}\skillmount-asm",
        )
        self.assertEqual(len(missing), 1)
        self.assertFalse(missing[0].ok)


class CommandOutputTests(unittest.TestCase):
    """Version and help output prove which command the package installed."""

    def test_exactly_one_version_is_required(self) -> None:
        self.assertEqual(harness.parse_reported_version("skillmount 0.2.0\n"), "0.2.0")
        self.assertEqual(harness.parse_reported_version("asm 0.2.0 (0.2.0)\n"), "0.2.0")
        for text in ("", "skillmount\n", "skillmount 0.2.0 and 0.1.0\n"):
            with self.assertRaises(harness.ChocolateyAcceptanceError):
                harness.parse_reported_version(text)

    def test_command_mentions_respect_word_boundaries(self) -> None:
        self.assertTrue(harness.mentions_command("skillmount 0.2.0", "skillmount"))
        self.assertFalse(harness.mentions_command("skillmount-asm 0.2.0", "skillmount"))
        self.assertFalse(harness.mentions_command("skillmount-asm 0.2.0", "asm"))
        self.assertTrue(harness.mentions_command("Usage: asm [OPTIONS]", "asm"))

    def test_version_findings_judge_status_value_and_command(self) -> None:
        good = harness.version_findings(
            harness.CommandResult(("asm", "--version"), 0, "asm 0.2.0\n"),
            ASM,
            expected_version=VERSION,
        )
        self.assertEqual([item.check for item in good if not item.ok], [])
        wrong = harness.version_findings(
            harness.CommandResult(("asm", "--version"), 0, "skillmount 0.1.0\n"),
            ASM,
            expected_version=VERSION,
        )
        failed = {item.check for item in wrong if not item.ok}
        self.assertEqual(failed, {"reported-version", "reported-command"})
        broken = harness.version_findings(
            harness.CommandResult(("asm", "--version"), 9, "boom\n"), ASM, expected_version=VERSION
        )
        self.assertEqual(
            [item.check for item in broken if not item.ok], ["version-status", "reported-version"]
        )

    def test_help_findings_require_the_selected_command_alone(self) -> None:
        good = harness.help_findings(
            harness.CommandResult(("skillmount", "--help"), 0, "Usage: skillmount [OPTIONS]\n"),
            SKILLMOUNT,
        )
        self.assertEqual([item.check for item in good if not item.ok], [])
        leaked = harness.help_findings(
            harness.CommandResult(("skillmount", "--help"), 1, "run asm instead\n"), SKILLMOUNT
        )
        failed = {item.check for item in leaked if not item.ok}
        self.assertEqual(failed, {"help-status", "help-names-command", "help-omits-pair-command"})


class FailureModeTests(unittest.TestCase):
    """Negative phases assert their specific failure mode, not just any failure."""

    def test_expected_and_forbidden_markers_are_both_enforced(self) -> None:
        result = harness.CommandResult(
            ("choco", "install", "skillmount"),
            1,
            "ERROR: Checksum for 'skillmount.zip' did not meet 'abc' for checksum type 'sha256'.",
        )
        findings = harness.failure_findings(
            result, expected_markers=("checksum", "abc"), forbidden_markers=("was successful",)
        )
        self.assertEqual([item.check for item in findings if not item.ok], [])
        self.assertEqual(
            [item.check for item in findings],
            ["exit-status", "failure-message", "no-unexpected-message"],
        )

    def test_a_wrong_failure_mode_is_reported(self) -> None:
        result = harness.CommandResult(("choco", "install"), 1, "The install was successful.")
        findings = harness.failure_findings(
            result, expected_markers=("checksum",), forbidden_markers=("was successful",)
        )
        failed = {item.check for item in findings if not item.ok}
        self.assertEqual(failed, {"failure-message", "no-unexpected-message"})

    def test_a_zero_status_is_a_failure_when_nonzero_is_required(self) -> None:
        result = harness.CommandResult(("choco", "install"), 0, "checksum mismatch reported")
        findings = harness.failure_findings(result, expected_markers=("checksum",))
        self.assertFalse(findings[0].ok)
        self.assertEqual(findings[0].check, "exit-status")
        tolerated = harness.failure_findings(
            result, expected_markers=("checksum",), require_nonzero=False
        )
        self.assertEqual([item.check for item in tolerated if not item.ok], [])

    def test_a_negative_phase_must_name_a_marker(self) -> None:
        with self.assertRaisesRegex(harness.ChocolateyAcceptanceError, "at least one expected"):
            harness.failure_findings(
                harness.CommandResult(("choco",), 1, ""), expected_markers=()
            )


class CleanupTests(unittest.TestCase):
    """Nothing package-owned may survive an uninstall or a failed install."""

    def test_an_absent_folder_and_shim_satisfy_cleanup(self) -> None:
        findings = harness.cleanup_findings(
            SKILLMOUNT,
            package_folder_names=None,
            where_output="INFO: Could not find files for the given pattern(s).\n",
            package_folder=rf"{LIB}\skillmount",
        )
        self.assertEqual([item.check for item in findings if not item.ok], [])

    def test_surviving_files_or_shims_are_named(self) -> None:
        findings = harness.cleanup_findings(
            SKILLMOUNT,
            package_folder_names=("tools/skillmount.exe",),
            where_output=f"{BIN}\\skillmount.exe\n",
            package_folder=rf"{LIB}\skillmount",
        )
        failed = {item.check: item.detail for item in findings if not item.ok}
        self.assertEqual(set(failed), {"package-folder-absent", "shim-absent"})
        self.assertIn("tools/skillmount.exe", failed["package-folder-absent"])
        self.assertIn("skillmount.exe", failed["shim-absent"])

    def test_a_survivor_must_remain_functional(self) -> None:
        findings = harness.survivor_findings(
            ASM,
            package_folder_names=("tools/asm.exe",),
            version_result=harness.CommandResult(("asm", "--version"), 0, "asm 0.2.0\n"),
            expected_version=VERSION,
        )
        self.assertEqual([item.check for item in findings if not item.ok], [])
        removed = harness.survivor_findings(
            ASM,
            package_folder_names=None,
            version_result=harness.CommandResult(("asm", "--version"), 1, ""),
            expected_version=VERSION,
        )
        failed = {item.check for item in removed if not item.ok}
        self.assertIn("package-folder-retained", failed)
        self.assertIn("version-status", failed)


class ResidueTests(unittest.TestCase):
    """Profiles, PATH values, user files, and product state must be byte-identical."""

    def build(self, root: Path) -> harness.ResidueTargets:
        """Create one residue fixture with a profile, sentinels, and a state tree."""

        profile = root / "profile" / "Microsoft.PowerShell_profile.ps1"
        profile.parent.mkdir(parents=True, exist_ok=True)
        profile.write_text("Set-Alias ll Get-ChildItem\n", encoding="utf-8")
        project = root / "project" / "sentinel.json"
        project.parent.mkdir(parents=True, exist_ok=True)
        project.write_text('{"kind":"project"}\n', encoding="utf-8")
        skill = root / "skills" / "source.md"
        skill.parent.mkdir(parents=True, exist_ok=True)
        skill.write_text("skill bytes\n", encoding="utf-8")
        state = root / "state"
        (state / "transactions").mkdir(parents=True, exist_ok=True)
        (state / "transactions" / "journal.json").write_text("[]\n", encoding="utf-8")
        return harness.ResidueTargets(
            profiles=(profile,),
            project_sentinel=project,
            skill_sentinel=skill,
            state_directory=state,
        )

    def test_digests_are_stable_and_absence_is_explicit(self) -> None:
        with tempfile.TemporaryDirectory() as temporary:
            root = Path(temporary)
            targets = self.build(root)
            self.assertEqual(harness.file_digest(root / "missing"), harness.ABSENT)
            self.assertEqual(harness.tree_digest(root / "missing"), harness.ABSENT)
            first = harness.tree_digest(targets.state_directory)
            self.assertEqual(first, harness.tree_digest(targets.state_directory))
            self.assertEqual(harness.text_digest("PATH"), harness.text_digest("PATH"))
            self.assertNotEqual(harness.text_digest("a"), harness.text_digest("b"))

    def test_an_unchanged_run_produces_no_residue_finding(self) -> None:
        with tempfile.TemporaryDirectory() as temporary:
            targets = self.build(Path(temporary))
            values = {scope: f"{scope}-path" for scope in targets.path_scopes}
            before = harness.residue_snapshot(targets, path_values=values)
            after = harness.residue_snapshot(targets, path_values=values)
            findings = harness.residue_findings(before, after)
            self.assertEqual([item.check for item in findings if not item.ok], [])
            self.assertIn("unchanged:project-sentinel", [item.check for item in findings])
            self.assertIn("unchanged:path:Machine", [item.check for item in findings])
            self.assertIn(
                f"unchanged:profile:{targets.profiles[0]}", [item.check for item in findings]
            )

    def test_every_kind_of_change_is_detected(self) -> None:
        with tempfile.TemporaryDirectory() as temporary:
            root = Path(temporary)
            targets = self.build(root)
            values = {scope: f"{scope}-path" for scope in targets.path_scopes}
            before = harness.residue_snapshot(targets, path_values=values)
            targets.profiles[0].write_text(
                "Set-Alias ll Get-ChildItem\n# added\n", encoding="utf-8"
            )
            targets.project_sentinel.write_text('{"kind":"tampered"}\n', encoding="utf-8")
            targets.skill_sentinel.unlink()
            (targets.state_directory / "transactions" / "journal.json").write_text(
                '[{"leaked":true}]\n', encoding="utf-8"
            )
            values["Machine"] = rf"{values['Machine']};{BIN}"
            after = harness.residue_snapshot(targets, path_values=values)
            failed = {item.check for item in harness.residue_findings(before, after) if not item.ok}
            self.assertEqual(
                failed,
                {
                    f"unchanged:profile:{targets.profiles[0]}",
                    "unchanged:project-sentinel",
                    "unchanged:skill-source-sentinel",
                    "unchanged:state-directory",
                    "unchanged:path:Machine",
                },
            )

    def test_a_changed_label_set_fails_closed(self) -> None:
        findings = harness.residue_findings({"a": "1", "b": "2"}, {"a": "1", "c": "3"})
        labels = next(item for item in findings if item.check == "residue-labels")
        self.assertFalse(labels.ok)
        self.assertIn("'b'", labels.detail)
        self.assertIn("'c'", labels.detail)

    def test_the_state_directory_matches_the_product_resolution(self) -> None:
        self.assertEqual(
            harness.state_directory({"LOCALAPPDATA": r"C:\Users\a\AppData\Local"}, windows=True),
            Path(r"C:\Users\a\AppData\Local") / "skillmount",
        )
        self.assertEqual(
            harness.state_directory({"HOME": "/Users/a"}, windows=False),
            Path("/Users/a/Library/Application Support/skillmount"),
        )
        self.assertEqual(
            harness.state_directory(
                {harness.STATE_OVERRIDE_VARIABLE: "/tmp/state", "HOME": "/Users/a"},
                windows=False,
            ),
            Path("/tmp/state"),
        )
        with self.assertRaisesRegex(harness.ChocolateyAcceptanceError, "LOCALAPPDATA"):
            harness.state_directory({}, windows=True)
        with self.assertRaisesRegex(harness.ChocolateyAcceptanceError, "HOME"):
            harness.state_directory({}, windows=False)

    def test_profile_paths_must_be_absolute(self) -> None:
        text = f"{BIN}\\profile.ps1\r\nC:\\Users\\a\\profile.ps1\r\n"
        self.assertEqual(
            harness.parse_profile_paths(text),
            (rf"{BIN}\profile.ps1", r"C:\Users\a\profile.ps1"),
        )
        with self.assertRaisesRegex(harness.ChocolateyAcceptanceError, "no \\$PROFILE paths"):
            harness.parse_profile_paths("\n")
        with self.assertRaisesRegex(harness.ChocolateyAcceptanceError, "not absolute"):
            harness.parse_profile_paths("profile.ps1\n")

    def test_the_chocolatey_root_honours_its_environment_variable(self) -> None:
        self.assertEqual(harness.chocolatey_root({}), Path(harness.DEFAULT_CHOCOLATEY_ROOT))
        self.assertEqual(
            harness.chocolatey_root({harness.CHOCOLATEY_ROOT_VARIABLE: r"D:\choco"}),
            Path(r"D:\choco"),
        )
        root = Path(CHOCO_ROOT)
        self.assertEqual(
            harness.package_folder_path(root, "skillmount-asm"), root / "lib" / "skillmount-asm"
        )
        self.assertEqual(harness.shim_directory_path(root), root / "bin")


class ArtifactTests(unittest.TestCase):
    """Corrupted candidates are derived from verified archive bytes, never invented."""

    def build_archive(self, root: Path) -> tuple[Path, release.Target]:
        """Write a ZIP archive with the exact release member layout."""

        target = harness.windows_target("x64")
        stem = release.asset_stem(TAG, target)
        archive = root / release.asset_name(TAG, target)
        with zipfile.ZipFile(archive, "w") as container:
            for name in release.expected_file_names(target):
                payload = (
                    pe_bytes(harness.MACHINE_AMD64) if name.endswith(".exe") else b"payload\n"
                )
                container.writestr(f"{stem}/{name}", payload)
        return archive, target

    def test_a_malformed_archive_is_not_a_zip(self) -> None:
        with tempfile.TemporaryDirectory() as temporary:
            path = Path(temporary) / "broken" / "archive.zip"
            harness.corrupt_archive(path)
            self.assertTrue(path.is_file())
            self.assertFalse(zipfile.is_zipfile(path))

    def test_dropping_the_selected_member_keeps_every_other_member(self) -> None:
        with tempfile.TemporaryDirectory() as temporary:
            root = Path(temporary)
            archive, target = self.build_archive(root)
            member = f"{release.asset_stem(TAG, target)}/skillmount.exe"
            mutated = root / "mutated" / archive.name
            harness.zip_without_member(archive, mutated, member)
            with zipfile.ZipFile(mutated) as container:
                names = set(container.namelist())
            self.assertNotIn(member, names)
            self.assertIn(f"{release.asset_stem(TAG, target)}/asm.exe", names)
            with self.assertRaisesRegex(harness.ChocolateyAcceptanceError, "has no member"):
                harness.zip_without_member(archive, mutated, "absent/member")

    def test_only_the_two_product_executables_are_extracted(self) -> None:
        with tempfile.TemporaryDirectory() as temporary:
            root = Path(temporary)
            archive, target = self.build_archive(root)
            destination = root / "binaries"
            harness.extract_executables(archive, target, tag=TAG, destination=destination)
            self.assertEqual(
                sorted(path.name for path in destination.iterdir()),
                ["asm.exe", "skillmount.exe"],
            )
            self.assertEqual(
                harness.pe_machine(harness.read_header(destination / "asm.exe")), 0x8664
            )
            stripped = root / "stripped" / archive.name
            harness.zip_without_member(
                archive, stripped, f"{release.asset_stem(TAG, target)}/asm.exe"
            )
            with self.assertRaisesRegex(harness.ChocolateyAcceptanceError, "has no member"):
                harness.extract_executables(
                    stripped, target, tag=TAG, destination=root / "partial"
                )

    def test_a_verified_download_rejects_mismatched_bytes(self) -> None:
        with tempfile.TemporaryDirectory() as temporary:
            root = Path(temporary)
            archive, _ = self.build_archive(root)
            digest = release.sha256_file(archive)
            destination = root / "downloads" / archive.name
            self.assertEqual(
                harness.download_verified(
                    harness.file_url(archive), destination, expected_sha256=digest
                ),
                digest,
            )
            self.assertEqual(release.sha256_file(destination), digest)
            with self.assertRaisesRegex(harness.ChocolateyAcceptanceError, "expected"):
                harness.download_verified(
                    harness.file_url(archive),
                    root / "downloads" / "again.zip",
                    expected_sha256=harness.flip_digest(digest),
                )
            with self.assertRaisesRegex(harness.ChocolateyAcceptanceError, "cannot download"):
                harness.download_verified(
                    (root / "absent.zip").as_uri(),
                    root / "downloads" / "absent.zip",
                    expected_sha256=digest,
                )

    def test_a_flipped_digest_is_well_formed_and_different(self) -> None:
        digest = "0" * 64
        flipped = harness.flip_digest(digest)
        self.assertNotEqual(flipped, digest)
        self.assertTrue(re.fullmatch(r"[0-9a-f]{64}", flipped))
        self.assertEqual(harness.flip_digest("a" * 63 + "0"), "a" * 63 + "1")
        with self.assertRaisesRegex(harness.ChocolateyAcceptanceError, "64-character"):
            harness.flip_digest("abc")

    def test_exactly_one_archive_identity_is_replaced(self) -> None:
        inputs = FakeInputs(
            version=VERSION,
            archives=(
                FakeArchive("i686-pc-windows-msvc", "x86.zip", "https://x86", "a" * 64),
                FakeArchive("x86_64-pc-windows-msvc", "x64.zip", "https://x64", "b" * 64),
            ),
        )
        replaced = harness.replace_archive(
            inputs, triple="x86_64-pc-windows-msvc", url="file:///x64", sha256="c" * 64
        )
        self.assertEqual(replaced.archives[0], inputs.archives[0])
        self.assertEqual(replaced.archives[1].url, "file:///x64")
        self.assertEqual(replaced.archives[1].sha256, "c" * 64)
        self.assertEqual(replaced.archives[1].name, "x64.zip")
        self.assertEqual(inputs.archives[1].url, "https://x64")
        with self.assertRaisesRegex(harness.ChocolateyAcceptanceError, "no archive for"):
            harness.replace_archive(inputs, triple="aarch64-apple-darwin", sha256="d" * 64)

    def test_windows_targets_come_from_the_release_definitions(self) -> None:
        self.assertEqual(harness.windows_target("x64").triple, "x86_64-pc-windows-msvc")
        self.assertEqual(harness.windows_target("x86").triple, "i686-pc-windows-msvc")
        with self.assertRaisesRegex(harness.ChocolateyAcceptanceError, "no target named"):
            harness.windows_target("arm64")

    def test_appending_to_a_missing_install_script_fails_closed(self) -> None:
        with tempfile.TemporaryDirectory() as temporary:
            root = Path(temporary) / "skillmount"
            (root / "tools").mkdir(parents=True)
            script = root / "tools" / "chocolateyinstall.ps1"
            script.write_text("$ErrorActionPreference = 'Stop'\n", encoding="utf-8")
            harness.append_install_script(root, "skillmount", ("throw 'boom'",))
            self.assertTrue(script.read_text(encoding="utf-8").endswith("throw 'boom'\n"))
            with self.assertRaisesRegex(harness.ChocolateyAcceptanceError, "chocolateyinstall"):
                harness.append_install_script(
                    Path(temporary) / "absent", "skillmount", ("throw 'boom'",)
                )

    def test_file_urls_address_local_archives(self) -> None:
        with tempfile.TemporaryDirectory() as temporary:
            path = Path(temporary) / "archive.zip"
            path.write_bytes(b"payload")
            url = harness.file_url(path)
            self.assertTrue(url.startswith("file://"))
            self.assertTrue(url.endswith("archive.zip"))


class SelectionTests(unittest.TestCase):
    """Selections and phase requests come from the shared map, never from literals."""

    def test_selection_facts_are_derived_from_the_identity_pair(self) -> None:
        channels = FakeChannels()
        selection = harness.selection_for(channels.PACKAGES[1])
        self.assertEqual(selection, ASM)
        self.assertEqual(harness.selection_for(channels.PACKAGES[0]), SKILLMOUNT)

    def test_package_order_follows_the_selection_map(self) -> None:
        channels = FakeChannels()
        self.assertEqual(
            [item.package_id for item in harness.selected_packages(channels, ())],
            ["skillmount", "skillmount-asm"],
        )
        self.assertEqual(
            [
                item.package_id
                for item in harness.selected_packages(channels, ("skillmount-asm",))
            ],
            ["skillmount-asm"],
        )
        with self.assertRaisesRegex(harness.ChocolateyAcceptanceError, "unknown package ids"):
            harness.selected_packages(channels, ("skillmount-cli",))

    def test_requested_phases_default_to_the_whole_matrix_in_order(self) -> None:
        parser = harness.argument_parser()
        self.assertEqual(harness.requested_phases(parser.parse_args([])), harness.PHASES)
        options = parser.parse_args(["--phase", "residue", "--phase", "pack"])
        self.assertEqual(harness.requested_phases(options), ("pack", "residue"))

    def test_the_phase_selector_rejects_an_unknown_phase(self) -> None:
        options = harness.argument_parser().parse_args([])
        options.phase = ["not-a-phase"]
        with self.assertRaisesRegex(harness.ChocolateyAcceptanceError, "unknown phases"):
            harness.requested_phases(options)


class ScenarioCoverageTests(unittest.TestCase):
    """Every spec scenario maps to a named phase, and every phase is claimed."""

    def test_the_map_is_internally_consistent(self) -> None:
        harness.validate_scenario_map()
        self.assertEqual(len(harness.PHASES), len(set(harness.PHASES)))
        self.assertEqual(
            harness.PHASES, harness.POSITIVE_PHASES + harness.NEGATIVE_PHASES
        )

    def test_an_unknown_or_unclaimed_phase_is_rejected(self) -> None:
        with self.assertRaisesRegex(harness.ChocolateyAcceptanceError, "unknown phases"):
            harness.validate_scenario_map(
                (harness.ScenarioMapping("bogus", ("not-a-phase",)),)
            )
        with self.assertRaisesRegex(harness.ChocolateyAcceptanceError, "names no phase"):
            harness.validate_scenario_map((harness.ScenarioMapping("empty", ()),))
        with self.assertRaisesRegex(harness.ChocolateyAcceptanceError, "no scenario"):
            harness.validate_scenario_map((harness.ScenarioMapping("partial", ("pack",)),))

    def test_every_scenario_in_the_spec_is_mapped(self) -> None:
        text = spec_path().read_text(encoding="utf-8")
        scenarios = tuple(
            line.removeprefix("#### Scenario:").strip()
            for line in text.splitlines()
            if line.startswith("#### Scenario:")
        )
        self.assertGreaterEqual(len(scenarios), 18)
        mapped = {mapping.scenario for mapping in harness.SCENARIO_MAP}
        self.assertEqual(set(scenarios) - mapped, set())
        self.assertEqual(mapped - set(scenarios), set())

    def test_coverage_findings_distinguish_a_narrowed_run_from_a_gap(self) -> None:
        complete = harness.coverage_findings(
            requested_phases=harness.PHASES,
            requested_packages=("skillmount", "skillmount-asm"),
            executed=harness.PHASES,
            narrowed=False,
        )
        self.assertEqual([item.check for item in complete if not item.ok], [])
        self.assertIn("matrix-coverage", [item.check for item in complete])
        narrowed = harness.coverage_findings(
            requested_phases=("pack",),
            requested_packages=("skillmount",),
            executed=("pack",),
            narrowed=True,
        )
        self.assertEqual([item.check for item in narrowed if not item.ok], [])
        self.assertIn("narrowed-run", [item.check for item in narrowed])
        detail = next(item.detail for item in narrowed if item.check == "narrowed-run")
        self.assertIn("'skillmount'", detail)
        for unexercised in ("residue", "co-install", "interrupted-install"):
            self.assertIn(unexercised, detail)
        gap = harness.coverage_findings(
            requested_phases=("pack", "residue"),
            requested_packages=("skillmount",),
            executed=("pack",),
            narrowed=False,
        )
        failed = {item.check for item in gap if not item.ok}
        self.assertEqual(failed, {"phase-coverage", "matrix-coverage"})


class ReportTests(unittest.TestCase):
    """The report records every finding, the scenario map, and the exit decision."""

    def phases(self, *, ok: bool) -> tuple[harness.PhaseResult, ...]:
        """Return one passing or failing phase result."""

        return (
            harness.PhaseResult(
                name="selected-only",
                package_id="skillmount",
                architecture="x64",
                findings=(
                    harness.Finding("package-file-set", True, "exact set"),
                    harness.Finding("pe-machine", ok, "machine detail"),
                ),
                evidence=(("package_folder", rf"{LIB}\skillmount"),),
            ),
        )

    def test_phase_status_requires_at_least_one_assertion(self) -> None:
        empty = harness.PhaseResult("pack", "skillmount", "", ())
        self.assertFalse(empty.ok)
        self.assertEqual(empty.as_json()["status"], "failed")
        self.assertTrue(self.phases(ok=True)[0].ok)

    def test_a_document_records_findings_scenarios_and_evidence(self) -> None:
        coverage = harness.coverage_findings(
            requested_phases=("selected-only",),
            requested_packages=("skillmount",),
            executed=("selected-only",),
            narrowed=True,
        )
        document = harness.report_document(
            status=harness.report_status(self.phases(ok=True), coverage),
            environment={"choco": "2.4.1"},
            provenance={"tag": TAG},
            packages=("skillmount",),
            nupkg_digests={"skillmount": "a" * 64},
            phases=self.phases(ok=True),
            coverage=coverage,
            narrowed=True,
        )
        self.assertEqual(document["schema"], harness.REPORT_SCHEMA)
        self.assertEqual(document["status"], "passed")
        self.assertFalse(document["complete"])
        self.assertEqual(document["packages"], ["skillmount"])
        self.assertEqual(document["nupkg_digests"], {"skillmount": "a" * 64})
        self.assertEqual(document["phases"][0]["status"], "passed")
        self.assertEqual(
            document["phases"][0]["evidence"], {"package_folder": rf"{LIB}\skillmount"}
        )
        self.assertEqual(
            [entry["scenario"] for entry in document["scenarios"]],
            [mapping.scenario for mapping in harness.SCENARIO_MAP],
        )
        self.assertEqual(harness.failed_checks(document), ())

    def test_one_failed_finding_fails_the_document(self) -> None:
        coverage = harness.coverage_findings(
            requested_phases=("selected-only",),
            requested_packages=("skillmount",),
            executed=("selected-only",),
            narrowed=True,
        )
        phases = self.phases(ok=False)
        self.assertEqual(harness.report_status(phases, coverage), "failed")
        document = harness.report_document(
            status="failed",
            environment={},
            provenance={},
            packages=("skillmount",),
            nupkg_digests={},
            phases=phases,
            coverage=coverage,
            narrowed=True,
        )
        self.assertEqual(
            harness.failed_checks(document), ("selected-only[skillmount]/pe-machine",)
        )

    def test_a_failed_coverage_finding_alone_fails_the_document(self) -> None:
        coverage = harness.coverage_findings(
            requested_phases=("selected-only", "residue"),
            requested_packages=("skillmount",),
            executed=("selected-only",),
            narrowed=False,
        )
        self.assertEqual(harness.report_status(self.phases(ok=True), coverage), "failed")
        document = harness.report_document(
            status="failed",
            environment={},
            provenance={},
            packages=("skillmount",),
            nupkg_digests={},
            phases=self.phases(ok=True),
            coverage=coverage,
            narrowed=False,
        )
        self.assertEqual(
            harness.failed_checks(document),
            ("coverage/phase-coverage", "coverage/matrix-coverage"),
        )

    def test_an_empty_run_never_reports_success(self) -> None:
        self.assertEqual(harness.report_status((), ()), "failed")

    def test_the_report_is_deterministic_json_with_a_trailing_newline(self) -> None:
        with tempfile.TemporaryDirectory() as temporary:
            path = Path(temporary) / "nested" / "report.json"
            document = {"status": "passed", "b": 1, "a": 2}
            harness.write_report(path, document)
            text = path.read_text(encoding="utf-8")
            self.assertTrue(text.endswith("\n"))
            self.assertEqual(json.loads(text), document)
            self.assertLess(text.index('"a"'), text.index('"b"'))

    def test_install_evidence_is_recorded_as_flat_text(self) -> None:
        evidence = harness.InstallEvidence(
            package_folder=Path(rf"{LIB}\skillmount"),
            names=("tools/skillmount.exe",),
            version_bytes=b"VERSION",
            executable_header=pe_bytes(harness.MACHINE_AMD64),
            selected_where=f"{BIN}\\skillmount.exe\n",
            other_where="",
            shim_path=Path(rf"{BIN}\skillmount.exe"),
            shim_target=rf"{LIB}\skillmount\tools\skillmount.exe",
            shim_metadata="noop",
        )
        recorded = dict(harness.install_evidence_pairs(evidence))
        self.assertEqual(recorded["shim_target"], rf"{LIB}\skillmount\tools\skillmount.exe")
        self.assertEqual(recorded["resolved"], rf"{BIN}\skillmount.exe")
        unresolved = harness.install_evidence_pairs(
            harness.InstallEvidence(
                package_folder=Path(rf"{LIB}\skillmount"),
                names=None,
                version_bytes=None,
                executable_header=b"",
                selected_where="",
                other_where="",
                shim_path=Path(rf"{BIN}\skillmount.exe"),
                shim_target=None,
                shim_metadata="",
            )
        )
        self.assertEqual(dict(unresolved)["shim_target"], "")

    def test_command_output_is_collapsed_for_evidence(self) -> None:
        self.assertEqual(harness.collapse("a\r\n  b\tc\n"), "a b c")
        result = harness.CommandResult(("choco", "install", "skillmount"), 0, "")
        self.assertEqual(result.command, "choco install skillmount")


class HelpAndRefusalIntegrationTests(unittest.TestCase):
    """The command-line surface stays usable on a host with no Chocolatey."""

    def test_help_exits_zero_without_touching_the_host(self) -> None:
        stdout = io.StringIO()
        with redirect_stdout(stdout):
            with self.assertRaises(SystemExit) as caught:
                harness.argument_parser().parse_args(["--help"])
        self.assertEqual(caught.exception.code, 0)
        self.assertIn("--binary-directory-x64", stdout.getvalue())
        self.assertIn("--report", stdout.getvalue())

    def test_local_mode_requires_a_tag(self) -> None:
        stderr = io.StringIO()
        with mock.patch.dict(os.environ, {harness.ACCEPTANCE_VARIABLE: "1"}):
            with redirect_stderr(stderr):
                self.assertEqual(harness.main([]), 1)
        self.assertIn("--tag is required without --inputs", stderr.getvalue())

    def test_local_mode_requires_both_binary_directories(self) -> None:
        options = harness.argument_parser().parse_args(
            ["--tag", TAG, "--binary-directory-x64", "bin"]
        )
        with self.assertRaisesRegex(
            harness.ChocolateyAcceptanceError, "--binary-directory-x86"
        ):
            harness.resolve_inputs(FakeChannels(), options, Path("."))

    def test_a_channel_failure_is_reported_as_a_harness_failure(self) -> None:
        class Rejecting(FakeChannels):
            """A channel module that rejects every artifact it is handed."""

            class ChannelError(RuntimeError):
                pass

            class PackageInputs:
                @classmethod
                def from_json(cls, text: str) -> object:
                    raise Rejecting.ChannelError("tag v9.9.9 does not match version 0.2.0")

            def generate_chocolatey_sources(self, *arguments: object, **keywords: object) -> None:
                raise Rejecting.ChannelError("template uses unknown token @BOGUS@")

        channels = Rejecting()
        with tempfile.TemporaryDirectory() as temporary:
            root = Path(temporary)
            artifact = root / "inputs.json"
            artifact.write_text("{}\n", encoding="utf-8")
            options = harness.argument_parser().parse_args(["--inputs", str(artifact)])
            with self.assertRaises(harness.ChocolateyAcceptanceError) as caught:
                harness.resolve_inputs(channels, options, root)
            self.assertIn("cannot trust the preflight inputs", str(caught.exception))
            self.assertIn("does not match version", str(caught.exception))
            with self.assertRaises(harness.ChocolateyAcceptanceError) as rendered:
                harness.render_sources(
                    channels,
                    None,
                    template_directory=Path("packaging/chocolatey"),
                    output_directory=root / "sources",
                )
            self.assertIn("cannot render Chocolatey package sources", str(rendered.exception))
            self.assertIn("@BOGUS@", str(rendered.exception))

    def test_inspection_errors_cover_channel_and_release_failures(self) -> None:
        class Channels:
            class ChannelError(RuntimeError):
                pass

        errors = harness.inspection_errors(Channels())
        self.assertIn(Channels.ChannelError, errors)
        self.assertIn(release.ReleaseError, errors)
        self.assertIn(zipfile.BadZipFile, errors)


if __name__ == "__main__":
    unittest.main()
