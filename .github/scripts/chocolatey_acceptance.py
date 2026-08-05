#!/usr/bin/env python3
"""Prove the Chocolatey package pair's native Windows lifecycle without publishing it.

Every decision this harness makes lives in a pure function that takes already-collected
evidence (package-folder listings, ``where.exe`` output text, shim diagnostics, raw PE header
bytes, residue digests, and ``choco`` output text) and returns findings. The Chocolatey boundary
is a thin shell-free :class:`ChocoGateway`, so the decision layer is unit-testable on any host.
"""

from __future__ import annotations

import argparse
import contextlib
import dataclasses
import hashlib
import json
import os
import platform
import re
import shutil
import subprocess
import sys
import tempfile
import urllib.request
import zipfile
from dataclasses import dataclass
from pathlib import Path, PureWindowsPath
from typing import TYPE_CHECKING, Any, Iterator, Mapping, Sequence

import release

if TYPE_CHECKING:
    import package_channels

ACCEPTANCE_VARIABLE = "SKILLMOUNT_CHOCOLATEY_ACCEPTANCE"
ACCEPTANCE_VALUE = "1"
STATE_OVERRIDE_VARIABLE = "SKILLMOUNT_STATE_DIR"
STATE_DIRECTORY_NAME = "skillmount"
CHOCOLATEY_ROOT_VARIABLE = "ChocolateyInstall"
DEFAULT_CHOCOLATEY_ROOT = r"C:\ProgramData\chocolatey"
DEFAULT_REPOSITORY = "pashifika/skillmount"
DEFAULT_TEMPLATE_DIRECTORY = Path("packaging/chocolatey")
REPORT_SCHEMA = 2
PRIOR_VERSION = "0.1.0"
INTERRUPTION_MARKER = "skillmount-acceptance-injected-interruption"
SUCCESS_MARKER = "was successful"
DOWNLOAD_CHUNK = 1024 * 1024

CARGO_MANIFEST_NAME = "Cargo.toml"
CARGO_PACKAGE_HEADER = "[package]"
CARGO_VERSION_PATTERN = re.compile(r'^version = "([^"\n]+)"[ \t]*$', re.MULTILINE)
CARGO_SECTION_PATTERN = re.compile(r"^\[", re.MULTILINE)

DOS_SIGNATURE = b"MZ"
PE_SIGNATURE = b"PE\0\0"
PE_OFFSET_FIELD = 0x3C
PE_HEADER_LIMIT = 4096
MACHINE_AMD64 = 0x8664
MACHINE_I386 = 0x014C
ARCHITECTURE_MACHINES = {"x64": MACHINE_AMD64, "x86": MACHINE_I386}
MACHINE_ARCHITECTURES = {machine: name for name, machine in ARCHITECTURE_MACHINES.items()}

IGNORE_SUFFIX = ".ignore"
PENDING_MARKER = ".chocolateypending"
ARCHIVE_SUFFIXES = (".zip", ".tar.gz", ".tgz", ".7z")
# `Get-ChocolateyUnzip` records every extracted path in `<downloaded-archive-name>.txt` inside the
# package folder, so a completed install legitimately leaves one manager-owned sidecar behind.
DOWNLOAD_SIDECAR_SUFFIX = ".txt"
ABSENT = "absent"
PATH_SCOPES = ("Process", "User", "Machine")

PAIR_PHASES = ("co-install", "cross-uninstall")
POSITIVE_PHASES = (
    "pack",
    "inspect",
    "install-x64",
    "selected-only",
    "shim",
    "version",
    "help",
    "upgrade",
    "uninstall",
    "install-x86",
    "co-install",
    "cross-uninstall",
    "residue",
)
NEGATIVE_PHASES = (
    "checksum-mismatch-x64",
    "checksum-mismatch-x86",
    "malformed-archive",
    "missing-selected-binary",
    "retained-unselected-binary",
    "extra-shim",
    "interrupted-install",
    "repeated-install",
)
PHASES = POSITIVE_PHASES + NEGATIVE_PHASES
X64_SESSION_PHASES = ("install-x64", "selected-only", "shim", "version", "help")
X86_SESSION_PHASES = ("install-x86",)
# The shim checks an injected foreign product shim must fail, so the `extra-shim` phase names the
# rejection it requires instead of accepting any failure at all.
EXTRA_SHIM_REJECTIONS = ("product-shim-count", "pair-shim-absent")


class ChocolateyAcceptanceError(RuntimeError):
    """A required Chocolatey lifecycle observation could not be proved."""


@dataclass(frozen=True)
class ScenarioMapping:
    """One `chocolatey-distribution` spec scenario and the phases that prove it."""

    scenario: str
    phases: tuple[str, ...]
    covered_elsewhere: str = ""

    def as_json(self) -> dict[str, Any]:
        """Return the report entry for this scenario."""

        return {
            "scenario": self.scenario,
            "phases": list(self.phases),
            "covered_elsewhere": self.covered_elsewhere,
        }


SCENARIO_MAP = (
    ScenarioMapping("x64 host installs either package", ("install-x64", "version")),
    ScenarioMapping("x86 selection is requested", ("install-x86",)),
    ScenarioMapping(
        "Selected checksum differs", ("checksum-mismatch-x64", "checksum-mismatch-x86")
    ),
    ScenarioMapping(
        "Package pair provenance differs",
        ("inspect",),
        "package_publish.reconcile_chocolatey blocks the pair before the API key exists",
    ),
    ScenarioMapping(
        "Descriptive command is selected", ("install-x64", "selected-only", "shim", "version")
    ),
    ScenarioMapping(
        "Short command is selected", ("install-x64", "selected-only", "shim", "version")
    ),
    ScenarioMapping("Both packages are co-installed", ("co-install",)),
    ScenarioMapping(
        "Unselected executable or shim remains",
        ("selected-only", "retained-unselected-binary", "extra-shim", "missing-selected-binary"),
    ),
    ScenarioMapping("Both candidate packages are valid", ("pack", "inspect")),
    ScenarioMapping(
        "Candidate contains unexpected content",
        ("inspect", "malformed-archive", "retained-unselected-binary"),
    ),
    ScenarioMapping("Each package upgrades from the prior release", ("upgrade",)),
    ScenarioMapping("One co-installed package is uninstalled", ("cross-uninstall",)),
    ScenarioMapping("Package is uninstalled alone", ("uninstall", "residue")),
    ScenarioMapping(
        "Public feed has no package entry",
        ("inspect",),
        "package_publish.reconcile_chocolatey treats absence as preflight, not ownership proof",
    ),
    ScenarioMapping(
        "New package upload is accepted",
        ("pack", "inspect"),
        "package_publish.reconcile_chocolatey pushes only validated digests",
    ),
    ScenarioMapping(
        "One package is approved and listed",
        ("install-x64", "selected-only", "shim", "version", "help"),
        "package_publish.reconcile_chocolatey records listing state per package id",
    ),
    ScenarioMapping(
        "Pair members have different moderation states",
        ("inspect",),
        "package_publish.reconcile_chocolatey reports both states separately",
    ),
    ScenarioMapping(
        "Complete Windows package matrix passes",
        (
            "install-x64",
            "install-x86",
            "selected-only",
            "upgrade",
            "uninstall",
            "co-install",
            "cross-uninstall",
            "residue",
            "interrupted-install",
            "repeated-install",
        ),
    ),
    ScenarioMapping(
        "One package or architecture cannot be exercised",
        ("pack", "install-x64", "install-x86"),
        "coverage findings name every phase a narrowed run did not exercise",
    ),
)


def validate_scenario_map(mappings: Sequence[ScenarioMapping] = SCENARIO_MAP) -> None:
    """Require every scenario to name known phases and every phase to be claimed."""

    claimed: set[str] = set()
    for mapping in mappings:
        if not mapping.phases:
            raise ChocolateyAcceptanceError(
                f"scenario {mapping.scenario!r} names no phase; expected at least one of {PHASES!r}"
            )
        unknown = tuple(name for name in mapping.phases if name not in PHASES)
        if unknown:
            raise ChocolateyAcceptanceError(
                f"scenario {mapping.scenario!r} names unknown phases {unknown!r}; "
                f"expected members of {PHASES!r}"
            )
        claimed.update(mapping.phases)
    missing = tuple(name for name in PHASES if name not in claimed)
    if missing:
        raise ChocolateyAcceptanceError(
            f"phases {missing!r} are mapped to no scenario; expected every phase of {PHASES!r}"
        )


@dataclass(frozen=True)
class CommandResult:
    """One completed shell-free command with merged output."""

    arguments: tuple[str, ...]
    returncode: int
    output: str

    @property
    def command(self) -> str:
        """Return a readable rendering of the executed argument vector."""

        return " ".join(self.arguments)


@dataclass(frozen=True)
class Finding:
    """One named assertion outcome recorded inside a phase."""

    check: str
    ok: bool
    detail: str

    def as_json(self) -> dict[str, Any]:
        """Return the report entry for this finding."""

        return {"check": self.check, "ok": self.ok, "detail": self.detail}


@dataclass(frozen=True)
class PhaseResult:
    """Every finding and recorded value one named phase produced."""

    name: str
    package_id: str
    architecture: str
    findings: tuple[Finding, ...]
    evidence: tuple[tuple[str, str], ...] = ()

    @property
    def ok(self) -> bool:
        """Return whether the phase asserted something and every assertion held."""

        return bool(self.findings) and all(item.ok for item in self.findings)

    def as_json(self) -> dict[str, Any]:
        """Return the report entry for this phase."""

        return {
            "phase": self.name,
            "package_id": self.package_id,
            "architecture": self.architecture,
            "status": "passed" if self.ok else "failed",
            "findings": [item.as_json() for item in self.findings],
            "evidence": {name: value for name, value in self.evidence},
        }


@dataclass(frozen=True)
class PackageSelection:
    """The identity facts one Chocolatey package's evidence is judged against."""

    package_id: str
    command: str
    selected_executable: str
    other_command: str
    other_executable: str


def selection_for(identity: package_channels.PackageIdentity) -> PackageSelection:
    """Derive the judged identity facts from the shared selection map."""

    return PackageSelection(
        package_id=identity.package_id,
        command=identity.command,
        selected_executable=identity.windows_executable,
        other_command=identity.other.command,
        other_executable=identity.other.windows_executable,
    )


def load_channels() -> Any:
    """Import the shared package-channel module this harness renders and inspects through."""

    try:
        import package_channels
    except ImportError as error:
        directory = Path(__file__).resolve().parent
        raise ChocolateyAcceptanceError(
            "package_channels is required to render and inspect Chocolatey package sources; "
            f"expected package_channels.py in {directory}"
        ) from error
    return package_channels


def require_opt_in(environment: Mapping[str, str]) -> None:
    """Refuse to touch the host's Chocolatey installation without explicit opt-in."""

    observed = environment.get(ACCEPTANCE_VARIABLE)
    if observed != ACCEPTANCE_VALUE:
        raise ChocolateyAcceptanceError(
            "refusing to run: this harness installs, upgrades, and uninstalls real Chocolatey "
            f"packages on this host, so {ACCEPTANCE_VARIABLE} must be {ACCEPTANCE_VALUE!r}; "
            f"observed {observed!r}"
        )


CHOCO_LIST_PATTERN = re.compile(
    r"^(?P<id>[A-Za-z0-9][A-Za-z0-9._+-]*)\s+"
    r"(?P<version>[0-9]+\.[0-9]+(?:\.[0-9]+)?(?:[.+-][0-9A-Za-z.-]+)?)$"
)


def parse_installed_packages(text: str) -> dict[str, str]:
    """Return installed package ids mapped to versions from `choco list` output."""

    installed: dict[str, str] = {}
    for line in text.splitlines():
        match = CHOCO_LIST_PATTERN.match(line.strip())
        if match is None:
            continue
        installed[match.group("id").casefold()] = match.group("version")
    return installed


def preexisting_installations(text: str, package_ids: Sequence[str]) -> tuple[str, ...]:
    """Return `id==version` for every requested package the host already installed."""

    installed = parse_installed_packages(text)
    return tuple(
        f"{package_id}=={installed[package_id.casefold()]}"
        for package_id in package_ids
        if package_id.casefold() in installed
    )


def require_clean_host(list_output: str, package_ids: Sequence[str]) -> None:
    """Refuse to run when the host already owns either SkillMount command package."""

    conflicts = preexisting_installations(list_output, package_ids)
    if conflicts:
        raise ChocolateyAcceptanceError(
            f"refusing to run: Chocolatey already reports {conflicts!r}; "
            f"expected none of {tuple(package_ids)!r} to be installed"
        )


def pe_machine(header: bytes) -> int:
    """Return the COFF machine type read from a bounded PE header prefix."""

    minimum = PE_OFFSET_FIELD + 4
    if len(header) < minimum:
        raise ChocolateyAcceptanceError(
            f"executable header is {len(header)} bytes; expected at least {minimum} bytes "
            "to read the PE header offset"
        )
    if header[:2] != DOS_SIGNATURE:
        raise ChocolateyAcceptanceError(
            f"expected DOS signature {DOS_SIGNATURE!r}; observed {header[:2]!r}"
        )
    offset = int.from_bytes(header[PE_OFFSET_FIELD : PE_OFFSET_FIELD + 4], "little")
    end = offset + 6
    if end > len(header):
        raise ChocolateyAcceptanceError(
            f"PE header at offset {offset} needs {end} bytes; only {len(header)} were inspected"
        )
    signature = header[offset : offset + 4]
    if signature != PE_SIGNATURE:
        raise ChocolateyAcceptanceError(
            f"expected PE signature {PE_SIGNATURE!r} at offset {offset}; observed {signature!r}"
        )
    return int.from_bytes(header[offset + 4 : offset + 6], "little")


def machine_name(machine: int) -> str:
    """Return the architecture name for a COFF machine type."""

    return MACHINE_ARCHITECTURES.get(machine, "unknown")


def architecture_machine(architecture: str) -> int:
    """Return the COFF machine type one requested architecture must report."""

    try:
        return ARCHITECTURE_MACHINES[architecture]
    except KeyError as error:
        raise ChocolateyAcceptanceError(
            f"unsupported architecture {architecture!r}; expected one of "
            f"{tuple(ARCHITECTURE_MACHINES)!r}"
        ) from error


def other_architecture(architecture: str) -> str:
    """Return the Windows architecture one requested architecture is not."""

    architecture_machine(architecture)
    return "x86" if architecture == "x64" else "x64"


def machine_finding(header: bytes, *, architecture: str, label: str) -> Finding:
    """Judge a retained executable's COFF machine type against the requested architecture."""

    expected = architecture_machine(architecture)
    try:
        observed = pe_machine(header)
    except ChocolateyAcceptanceError as error:
        return Finding("pe-machine", False, f"{label}: {error}")
    return Finding(
        "pe-machine",
        observed == expected,
        f"{label}: expected machine 0x{expected:04x} ({architecture}); "
        f"observed 0x{observed:04x} ({machine_name(observed)})",
    )


def read_header(path: Path, *, limit: int = PE_HEADER_LIMIT) -> bytes:
    """Read a bounded executable header prefix without loading a whole executable."""

    with path.open("rb") as handle:
        return handle.read(limit)


def normalize_member(name: str) -> str:
    """Return one package-folder entry as a comparable relative POSIX path."""

    text = name.strip().strip('"')
    if not text:
        raise ChocolateyAcceptanceError("package folder listing contained an empty entry")
    pure = PureWindowsPath(text)
    if pure.drive or pure.is_absolute():
        raise ChocolateyAcceptanceError(
            f"package folder entry is not relative to the package folder: {name!r}"
        )
    parts = [part for part in pure.parts if part != "."]
    if ".." in parts:
        raise ChocolateyAcceptanceError(f"package folder entry escapes the package: {name!r}")
    if not parts:
        raise ChocolateyAcceptanceError(f"package folder entry names nothing: {name!r}")
    return "/".join(parts)


def listing_index(names: Sequence[str]) -> dict[str, str]:
    """Index a package-folder listing case-insensitively, as Windows compares it."""

    indexed: dict[str, str] = {}
    for name in names:
        normalized = normalize_member(name)
        indexed.setdefault(normalized.casefold(), normalized)
    return indexed


def required_package_files(selection: PackageSelection) -> tuple[str, ...]:
    """Return every file a completed package folder must contain."""

    return (
        f"{selection.package_id}.nuspec",
        "tools/LICENSE-APACHE",
        "tools/LICENSE-MIT",
        "tools/VERIFICATION.txt",
        "tools/VERSION",
        "tools/chocolateyinstall.ps1",
        f"tools/{selection.selected_executable}",
    )


def optional_package_files(selection: PackageSelection, version: str) -> tuple[str, ...]:
    """Return the Chocolatey-owned bookkeeping a completed package folder may also contain."""

    return (
        f"{selection.package_id}.nupkg",
        f"{selection.package_id}.{version}.nupkg",
        "tools/chocolateyuninstall.ps1",
    )


def download_sidecar_names(names: Sequence[str]) -> tuple[str, ...]:
    """Return every `Get-ChocolateyUnzip` extraction sidecar a package folder carries.

    Chocolatey writes `<downloaded-archive-name>.txt` beside the package's own bookkeeping to
    record what it extracted, exactly like `<id>.nupkg`, so the sidecar is manager-owned content
    rather than something the package installed.
    """

    return tuple(
        sorted(
            name
            for name in names
            if "/" not in name
            and name.casefold().endswith(DOWNLOAD_SIDECAR_SUFFIX)
            and PureWindowsPath(name).stem.casefold().endswith(ARCHIVE_SUFFIXES)
        )
    )


def package_folder_findings(
    names: Sequence[str], selection: PackageSelection, *, version: str
) -> tuple[Finding, ...]:
    """Judge one completed package folder listing against the selection contract."""

    indexed = listing_index(names)
    observed = tuple(sorted(indexed.values()))
    required = required_package_files(selection)
    sidecars = download_sidecar_names(tuple(indexed.values()))
    allowed = {
        entry.casefold()
        for entry in (*required, *optional_package_files(selection, version), *sidecars)
    }
    missing = tuple(entry for entry in required if entry.casefold() not in indexed)
    unexpected = tuple(value for folded, value in sorted(indexed.items()) if folded not in allowed)
    findings = [
        Finding(
            "package-file-set",
            not missing and not unexpected,
            f"expected exactly {required!r} plus Chocolatey bookkeeping; missing {missing!r}; "
            f"unexpected {unexpected!r}; observed {observed!r}",
        )
    ]

    unselected = tuple(
        value
        for value in indexed.values()
        if PureWindowsPath(value).name.casefold() == selection.other_executable.casefold()
    )
    findings.append(
        Finding(
            "unselected-executable-absent",
            not unselected,
            f"expected no {selection.other_executable!r} in the {selection.package_id} package; "
            f"observed {unselected!r}",
        )
    )

    markers = tuple(value for value in indexed.values() if value.casefold().endswith(IGNORE_SUFFIX))
    findings.append(
        Finding(
            "ignore-marker-absent",
            not markers,
            f"expected no {IGNORE_SUFFIX!r} marker substituting for executable removal; "
            f"observed {markers!r}",
        )
    )

    selected = f"tools/{selection.selected_executable}".casefold()
    foreign = tuple(
        value
        for value in indexed.values()
        if value.casefold().endswith((".exe", ".dll")) and value.casefold() != selected
    )
    findings.append(
        Finding(
            "foreign-executable-absent",
            not foreign,
            f"expected only tools/{selection.selected_executable} to be executable content; "
            f"observed {foreign!r}",
        )
    )

    residue = tuple(
        value
        for value in indexed.values()
        if value.casefold().endswith(ARCHIVE_SUFFIXES)
        or PureWindowsPath(value).name.casefold() == PENDING_MARKER
    )
    findings.append(
        Finding(
            "extraction-residue-absent",
            not residue,
            "expected the package-owned temporary extraction and the Chocolatey pending marker to "
            f"be removed; observed {residue!r}",
        )
    )
    return tuple(findings)


def download_provenance_findings(
    sidecars: Sequence[tuple[str, str]],
    *,
    architecture: str,
    expected_reference: str,
    other_reference: str,
) -> tuple[Finding, ...]:
    """Judge which architecture's release archive the extraction sidecars say was installed.

    `Get-ChocolateyUnzip` lists every extracted path, so each sidecar names the archive root
    directory the downloaded archive carries. That root is the archive's own name without its
    suffix, which makes the sidecar the manager's independent statement of which architecture the
    install actually downloaded.
    """

    recorded = tuple((name, text) for name, text in sidecars if text.strip())
    if not recorded:
        return (
            Finding(
                "download-provenance",
                True,
                "skipped the download-provenance check: Chocolatey recorded no extraction sidecar "
                f"content, so nothing names the {architecture} archive {expected_reference!r}; "
                f"observed {tuple(name for name, _ in sidecars)!r}",
            ),
        )
    unproven = tuple(
        name for name, text in recorded if expected_reference.casefold() not in text.casefold()
    )
    foreign = tuple(
        name for name, text in recorded if other_reference.casefold() in text.casefold()
    )
    return (
        Finding(
            "download-provenance",
            not unproven,
            f"expected every extraction sidecar to name the {architecture} archive "
            f"{expected_reference!r}; sidecars naming something else {unproven!r}; "
            f"observed {recorded!r}",
        ),
        Finding(
            "download-provenance-architecture",
            not foreign,
            f"expected no extraction sidecar to name the {other_architecture(architecture)} "
            f"archive {other_reference!r}; observed {foreign!r}",
        ),
    )


def staleness_markers(
    *, prior_version: str, prior_tag: str, version: str, tag: str
) -> tuple[str, ...]:
    """Return the prior-release markers an upgraded VERSION file must no longer mention.

    A rehearsal can only prove replaced metadata when the prior release differs from the
    candidate; an identical prior version yields no marker, and the caller records that skip
    instead of asserting a condition the candidate's own metadata contradicts.
    """

    if prior_version == version or prior_tag == tag:
        return ()
    return (prior_version, prior_tag)


def version_file_findings(
    observed: bytes | None, *, expected: bytes, forbidden: Sequence[str] = ()
) -> tuple[Finding, ...]:
    """Judge the retained release metadata file against the packaged identity."""

    if observed is None:
        return (
            Finding(
                "version-metadata",
                False,
                f"expected retained release metadata {expected!r}; observed no VERSION file",
            ),
        )
    findings = [
        Finding(
            "version-metadata",
            observed == expected,
            f"expected VERSION bytes {expected!r}; observed {observed!r}",
        )
    ]
    if forbidden:
        text = observed.decode("utf-8", errors="replace")
        present = tuple(marker for marker in forbidden if marker in text)
        findings.append(
            Finding(
                "version-metadata-not-stale",
                not present,
                f"expected VERSION to mention none of {tuple(forbidden)!r}; observed {present!r}",
            )
        )
    return tuple(findings)


def parse_where_output(text: str) -> tuple[str, ...]:
    """Return the absolute paths `where.exe` resolved, ignoring its INFO diagnostics."""

    resolved: dict[str, str] = {}
    for line in text.splitlines():
        candidate = line.strip().strip('"')
        if not candidate or candidate.upper().startswith("INFO:"):
            continue
        pure = PureWindowsPath(candidate)
        if not pure.drive or not pure.is_absolute():
            raise ChocolateyAcceptanceError(
                f"where.exe emitted a path that is not absolute: {candidate!r}"
            )
        resolved.setdefault(str(pure).casefold(), str(pure))
    return tuple(resolved.values())


def resolved_shims(where_output: str, command: str) -> tuple[str, ...]:
    """Return every resolved path whose file name is exactly one product command's shim."""

    expected = f"{command}.exe".casefold()
    return tuple(
        path
        for path in parse_where_output(where_output)
        if PureWindowsPath(path).name.casefold() == expected
    )


WINDOWS_PATH_PATTERN = re.compile(r"[A-Za-z]:\\[^\r\n\"'<>|]+")


def parse_shim_target(text: str, *, shim_path: str, executable: str) -> str:
    """Extract a shim's target executable from unstructured shim diagnostic output.

    Shim generators do not promise a stable output grammar, so only absolute path tokens are
    trusted: every candidate must name the expected executable and must not be the shim itself.
    """

    expected = executable.casefold()
    excluded = str(PureWindowsPath(shim_path)).casefold()
    candidates: dict[str, str] = {}
    for match in WINDOWS_PATH_PATTERN.finditer(text):
        candidate = match.group(0).rstrip(" \t.,;:)]}'\"")
        pure = PureWindowsPath(candidate)
        if pure.name.casefold() != expected:
            continue
        folded = str(pure).casefold()
        if folded == excluded:
            continue
        candidates.setdefault(folded, str(pure))
    if len(candidates) != 1:
        raise ChocolateyAcceptanceError(
            f"expected exactly one shim target named {executable!r} other than {shim_path!r}; "
            f"observed {tuple(candidates.values())!r}"
        )
    return next(iter(candidates.values()))


def folded_parts(path: str) -> tuple[str, ...]:
    """Return one Windows path's comparable parts, rejecting traversal."""

    pure = PureWindowsPath(path)
    parts = tuple(part for part in pure.parts if part != ".")
    if ".." in parts:
        raise ChocolateyAcceptanceError(f"Windows path contains traversal: {path!r}")
    return tuple(part.casefold().rstrip("\\") for part in parts)


def windows_path_inside(child: str, parent: str) -> bool:
    """Return whether a Windows path lies strictly inside a directory, case-insensitively."""

    child_parts = folded_parts(child)
    parent_parts = folded_parts(parent)
    if not parent_parts or len(child_parts) <= len(parent_parts):
        return False
    return child_parts[: len(parent_parts)] == parent_parts


def product_shims(
    selection: PackageSelection, *, selected_where: str, other_where: str, shim_directory: str
) -> tuple[str, ...]:
    """Return every product-command shim resolved inside the Chocolatey command directory."""

    found: dict[str, str] = {}
    for where_output, command in (
        (selected_where, selection.command),
        (other_where, selection.other_command),
    ):
        for path in resolved_shims(where_output, command):
            if windows_path_inside(path, shim_directory):
                pure = str(PureWindowsPath(path))
                found.setdefault(pure.casefold(), pure)
    return tuple(sorted(found.values(), key=str.casefold))


def shim_findings(
    selection: PackageSelection,
    *,
    selected_where: str,
    other_where: str,
    shim_target: str | None,
    package_folder: str,
    shim_directory: str,
    pair_installed: bool = False,
) -> tuple[Finding, ...]:
    """Judge Chocolatey command-path ownership for one installed package.

    A shim always lives in the Chocolatey command directory rather than inside the package folder,
    so ownership is judged from which product commands the command directory exposes: a lone
    install must expose exactly its own, and only a co-installed pair may expose both.
    """

    selected = resolved_shims(selected_where, selection.command)
    findings = [
        Finding(
            "shim-resolves-once",
            len(selected) == 1,
            f"expected exactly one resolved {selection.command}.exe shim; observed {selected!r}",
        )
    ]
    if selected:
        findings.append(
            Finding(
                "shim-directory",
                all(windows_path_inside(path, shim_directory) for path in selected),
                f"expected every {selection.command} shim under {shim_directory!r}; "
                f"observed {selected!r}",
            )
        )
    if shim_target is None:
        findings.append(
            Finding(
                "shim-target-inside-package",
                False,
                f"expected a resolvable {selection.selected_executable} target inside "
                f"{package_folder!r}; no shim target was observed",
            )
        )
    else:
        findings.append(
            Finding(
                "shim-target-inside-package",
                windows_path_inside(shim_target, package_folder),
                f"expected the shim target inside {package_folder!r}; observed {shim_target!r}",
            )
        )
    exposed = product_shims(
        selection,
        selected_where=selected_where,
        other_where=other_where,
        shim_directory=shim_directory,
    )
    expected_count = 2 if pair_installed else 1
    installed_packages = (
        "both command packages are" if pair_installed else f"only {selection.package_id} is"
    )
    findings.append(
        Finding(
            "product-shim-count",
            len(exposed) == expected_count,
            f"expected exactly {expected_count} product shim(s) under {shim_directory!r} while "
            f"{installed_packages} installed; observed {exposed!r}",
        )
    )
    if not pair_installed:
        expected_pair_shim = f"{selection.other_command}.exe".casefold()
        intruding = tuple(
            path
            for path in exposed
            if PureWindowsPath(path).name.casefold() == expected_pair_shim
        )
        findings.append(
            Finding(
                "pair-shim-absent",
                not intruding,
                f"expected no {selection.other_command} shim under {shim_directory!r} while only "
                f"the {selection.package_id} package is installed; observed {intruding!r}",
            )
        )
    return tuple(findings)


def independent_ownership_findings(
    left: PackageSelection,
    right: PackageSelection,
    *,
    left_target: str | None,
    right_target: str | None,
    left_folder: str,
    right_folder: str,
) -> tuple[Finding, ...]:
    """Judge that two co-installed packages share no installed executable ownership."""

    if left_target is None or right_target is None:
        return (
            Finding(
                "distinct-shim-targets",
                False,
                f"expected resolvable targets for {left.command} and {right.command}; "
                f"observed {left_target!r} and {right_target!r}",
            ),
        )
    return (
        Finding(
            "distinct-shim-targets",
            str(PureWindowsPath(left_target)).casefold()
            != str(PureWindowsPath(right_target)).casefold(),
            f"expected distinct shim targets; observed {left_target!r} and {right_target!r}",
        ),
        Finding(
            "package-owned-targets",
            windows_path_inside(left_target, left_folder)
            and windows_path_inside(right_target, right_folder),
            f"expected {left_target!r} inside {left_folder!r} and {right_target!r} inside "
            f"{right_folder!r}",
        ),
    )


VERSION_PATTERN = re.compile(r"\b(\d+\.\d+\.\d+)\b")


def parse_reported_version(text: str) -> str:
    """Return the single semantic version one product command reported."""

    found = tuple(dict.fromkeys(VERSION_PATTERN.findall(text)))
    if len(found) != 1:
        raise ChocolateyAcceptanceError(
            f"expected exactly one version in command output; observed {found!r}"
        )
    return found[0]


def version_findings(
    result: CommandResult, selection: PackageSelection, *, expected_version: str
) -> tuple[Finding, ...]:
    """Judge one installed command's version output.

    `src/cli.rs` reports the product name, never the invoked file name, so the version line proves
    the version and nothing about which command was invoked. Command identity is proved instead by
    the retained executable and the resolved shim target that `shim_findings` judges.
    """

    findings = [
        Finding(
            "version-status",
            result.returncode == 0,
            f"expected {result.command} to succeed; observed status {result.returncode} with "
            f"output {result.output.strip()!r}",
        )
    ]
    try:
        observed = parse_reported_version(result.output)
    except ChocolateyAcceptanceError as error:
        findings.append(Finding("reported-version", False, str(error)))
        return tuple(findings)
    findings.append(
        Finding(
            "reported-version",
            observed == expected_version,
            f"expected {selection.command} to report the package version {expected_version!r}; "
            f"observed {observed!r} in {result.output.strip()!r}",
        )
    )
    return tuple(findings)


HELP_USAGE_TEMPLATE = "Usage: <{commands}> <COMMAND>"
HELP_COMMANDS_HEADER = "Commands:"


def shared_usage_line(selection: PackageSelection) -> str:
    """Return the name-agnostic usage line both product commands print.

    `src/cli.rs` declares one `bin_name` for both executables, so help names the command pair
    instead of the invoked file name. Help output therefore never distinguishes the packages, and
    the pair's completion scripts, not its help text, are where a leaked name would matter.
    """

    return HELP_USAGE_TEMPLATE.format(
        commands="|".join(sorted((selection.command, selection.other_command)))
    )


def help_commands(text: str) -> tuple[str, ...]:
    """Return the command names one help output lists under its `Commands:` heading."""

    listed: list[str] = []
    inside = False
    for line in text.splitlines():
        stripped = line.strip()
        if not inside:
            inside = stripped == HELP_COMMANDS_HEADER
            continue
        if not stripped:
            continue
        if line[:1] not in (" ", "\t"):
            break
        listed.append(stripped.split()[0])
    return tuple(dict.fromkeys(listed))


def help_findings(
    result: CommandResult, selection: PackageSelection, *, pair_output: str | None = None
) -> tuple[Finding, ...]:
    """Judge one installed command's help output, and its equivalence to the pair member's."""

    usage = shared_usage_line(selection)
    listed = help_commands(result.output)
    findings = [
        Finding(
            "help-status",
            result.returncode == 0,
            f"expected {result.command} to succeed; observed status {result.returncode}",
        ),
        Finding(
            "help-usage-line",
            usage in collapse(result.output),
            f"expected help output to report {usage!r}; "
            f"observed {collapse(result.output)[:200]!r}",
        ),
        Finding(
            "help-command-list",
            bool(listed),
            f"expected help output to list commands under {HELP_COMMANDS_HEADER!r}; "
            f"observed {listed!r} in {collapse(result.output)[:200]!r}",
        ),
    ]
    if pair_output is None:
        findings.append(
            Finding(
                "help-equivalent",
                True,
                f"skipped the pair-equivalence check: no {selection.other_command} help output was "
                f"recorded before {selection.command} ran, so equivalence is judged when the pair "
                "member runs",
            )
        )
    else:
        findings.append(
            Finding(
                "help-equivalent",
                collapse(result.output) == collapse(pair_output),
                f"expected {selection.command} help output to equal {selection.other_command} "
                f"help output; observed {collapse(result.output)[:200]!r} and "
                f"{collapse(pair_output)[:200]!r}",
            )
        )
    return tuple(findings)


def installed_version_finding(
    list_output: str, *, package_id: str, expected_version: str
) -> Finding:
    """Judge the version Chocolatey reports for one installed package."""

    observed = parse_installed_packages(list_output).get(package_id.casefold())
    return Finding(
        "installed-version",
        observed == expected_version,
        f"expected Chocolatey to report {package_id} {expected_version}; observed {observed!r}",
    )


def failure_findings(
    result: CommandResult,
    *,
    expected_markers: Sequence[str],
    forbidden_markers: Sequence[str] = (),
    require_nonzero: bool = True,
) -> tuple[Finding, ...]:
    """Judge one deliberately corrupted lifecycle command's specific failure mode."""

    if not expected_markers:
        raise ChocolateyAcceptanceError(
            "a negative phase must name at least one expected output marker"
        )
    folded = result.output.casefold()
    missing = tuple(marker for marker in expected_markers if marker.casefold() not in folded)
    present = tuple(marker for marker in forbidden_markers if marker.casefold() in folded)
    findings = [
        Finding(
            "failure-message",
            not missing,
            f"expected output to report {tuple(expected_markers)!r}; missing {missing!r}; "
            f"observed {result.output.strip()[-600:]!r}",
        ),
        Finding(
            "no-unexpected-message",
            not present,
            f"expected output to report none of {tuple(forbidden_markers)!r}; observed {present!r}",
        ),
    ]
    if require_nonzero:
        findings.insert(
            0,
            Finding(
                "exit-status",
                result.returncode != 0,
                f"expected {result.command} to fail; observed status {result.returncode}",
            ),
        )
    return tuple(findings)


def cleanup_findings(
    selection: PackageSelection,
    *,
    package_folder_names: Sequence[str] | None,
    where_output: str,
    package_folder: str,
) -> tuple[Finding, ...]:
    """Judge that neither a package folder nor a product shim survived cleanup."""

    survivors = () if package_folder_names is None else tuple(sorted(package_folder_names))
    shims = resolved_shims(where_output, selection.command)
    return (
        Finding(
            "package-folder-absent",
            package_folder_names is None,
            f"expected {package_folder!r} to be absent; observed {survivors!r}",
        ),
        Finding(
            "shim-absent",
            not shims,
            f"expected no resolved {selection.command}.exe shim; observed {shims!r}",
        ),
    )


def survivor_findings(
    selection: PackageSelection,
    *,
    package_folder_names: Sequence[str] | None,
    version_result: CommandResult,
    expected_version: str,
) -> tuple[Finding, ...]:
    """Judge that an untouched co-installed package still works after its pair is removed."""

    findings = [
        Finding(
            "package-folder-retained",
            package_folder_names is not None,
            f"expected the {selection.package_id} package folder to survive its pair's uninstall; "
            "observed an absent folder",
        )
    ]
    findings.extend(version_findings(version_result, selection, expected_version=expected_version))
    return tuple(findings)


def state_directory(environment: Mapping[str, str], *, windows: bool) -> Path:
    """Resolve the SkillMount application-support state directory the product would use."""

    override = environment.get(STATE_OVERRIDE_VARIABLE)
    if override:
        return Path(override)
    if windows:
        local = environment.get("LOCALAPPDATA")
        if not local:
            raise ChocolateyAcceptanceError(
                "LOCALAPPDATA is required to resolve the SkillMount state directory on Windows; "
                f"set it or {STATE_OVERRIDE_VARIABLE}"
            )
        return Path(local) / STATE_DIRECTORY_NAME
    home = environment.get("HOME")
    if not home:
        raise ChocolateyAcceptanceError(
            "HOME is required to resolve the SkillMount state directory; "
            f"set it or {STATE_OVERRIDE_VARIABLE}"
        )
    return Path(home) / "Library/Application Support" / STATE_DIRECTORY_NAME


def chocolatey_root(environment: Mapping[str, str]) -> Path:
    """Resolve the Chocolatey installation root without guessing a drive layout."""

    configured = environment.get(CHOCOLATEY_ROOT_VARIABLE)
    return Path(configured) if configured else Path(DEFAULT_CHOCOLATEY_ROOT)


def package_folder_path(root: Path, package_id: str) -> Path:
    """Return the Chocolatey-managed folder for one package id."""

    return root / "lib" / package_id


def shim_directory_path(root: Path) -> Path:
    """Return the Chocolatey command-path directory."""

    return root / "bin"


def text_digest(text: str) -> str:
    """Return the SHA-256 digest of one text value."""

    return hashlib.sha256(text.encode("utf-8")).hexdigest()


def file_digest(path: Path) -> str:
    """Return the SHA-256 digest of one file, or the absent marker."""

    if path.is_symlink():
        return f"link:{os.readlink(path)}"
    if not path.is_file():
        return ABSENT
    digest = hashlib.sha256()
    with path.open("rb") as handle:
        while chunk := handle.read(DOWNLOAD_CHUNK):
            digest.update(chunk)
    return digest.hexdigest()


def tree_digest(root: Path) -> str:
    """Return a stable digest over one directory tree's names, kinds, and bytes."""

    if root.is_symlink():
        return f"link:{os.readlink(root)}"
    if not root.exists():
        return ABSENT
    if root.is_file():
        return file_digest(root)
    digest = hashlib.sha256()
    for directory, directories, files in os.walk(root, followlinks=False):
        directories.sort()
        base = Path(directory)
        for name in sorted(directories):
            relative = (base / name).relative_to(root).as_posix()
            digest.update(f"{relative}/\0dir\0".encode())
        for name in sorted(files):
            path = base / name
            relative = path.relative_to(root).as_posix()
            digest.update(f"{relative}\0{file_digest(path)}\0".encode())
    return digest.hexdigest()


@dataclass(frozen=True)
class ResidueTargets:
    """Paths and environment scopes whose bytes must survive the whole run unchanged."""

    profiles: tuple[Path, ...]
    project_sentinel: Path
    skill_sentinel: Path
    state_directory: Path
    path_scopes: tuple[str, ...] = PATH_SCOPES


def residue_snapshot(targets: ResidueTargets, *, path_values: Mapping[str, str]) -> dict[str, str]:
    """Digest every residue target so an unrelated change is provable byte-for-byte."""

    snapshot: dict[str, str] = {}
    for profile in targets.profiles:
        snapshot[f"profile:{profile}"] = file_digest(profile)
    snapshot["project-sentinel"] = file_digest(targets.project_sentinel)
    snapshot["skill-source-sentinel"] = file_digest(targets.skill_sentinel)
    snapshot["state-directory"] = tree_digest(targets.state_directory)
    for scope in targets.path_scopes:
        value = path_values.get(scope)
        snapshot[f"path:{scope}"] = ABSENT if value is None else text_digest(value)
    return snapshot


def residue_findings(before: Mapping[str, str], after: Mapping[str, str]) -> tuple[Finding, ...]:
    """Judge that no profile, PATH value, user file, or product state changed."""

    missing = tuple(sorted(set(before) - set(after)))
    extra = tuple(sorted(set(after) - set(before)))
    findings = [
        Finding(
            "residue-labels",
            not missing and not extra,
            f"expected identical residue labels; missing {missing!r}; extra {extra!r}",
        )
    ]
    for label in sorted(set(before) & set(after)):
        findings.append(
            Finding(
                f"unchanged:{label}",
                before[label] == after[label],
                f"expected {label} digest {before[label]!r}; observed {after[label]!r}",
            )
        )
    return tuple(findings)


def coverage_findings(
    *,
    requested_phases: Sequence[str],
    requested_packages: Sequence[str],
    executed: Sequence[str],
    narrowed: bool,
) -> tuple[Finding, ...]:
    """Report every requested phase that produced no evidence, and every narrowing."""

    executed_set = set(executed)
    missing = tuple(name for name in requested_phases if name not in executed_set)
    not_exercised = tuple(name for name in PHASES if name not in executed_set)
    findings = [
        Finding(
            "phase-coverage",
            not missing,
            f"expected evidence for {tuple(requested_phases)!r}; missing {missing!r}",
        )
    ]
    if narrowed:
        findings.append(
            Finding(
                "narrowed-run",
                True,
                f"run narrowed to packages {tuple(requested_packages)!r} and phases "
                f"{tuple(requested_phases)!r}; phases without evidence here: {not_exercised!r}",
            )
        )
    else:
        findings.append(
            Finding(
                "matrix-coverage",
                not not_exercised,
                f"expected every phase of {PHASES!r} to be exercised; missing {not_exercised!r}",
            )
        )
    return tuple(findings)


def report_status(phases: Sequence[PhaseResult], coverage: Sequence[Finding]) -> str:
    """Return the single word that decides the harness's exit status."""

    if phases and all(phase.ok for phase in phases) and all(item.ok for item in coverage):
        return "passed"
    return "failed"


def inputs_summary(inputs: package_channels.PackageInputs) -> dict[str, Any]:
    """Return the provenance a report must record for every archive it verified."""

    return {
        "repository": inputs.repository,
        "version": inputs.version,
        "tag": inputs.tag,
        "commit": inputs.commit,
        "release_url": inputs.release_url,
        "archives": {
            archive.triple: {
                "name": archive.name,
                "url": archive.url,
                "sha256": archive.sha256,
            }
            for archive in inputs.archives
        },
    }


def report_document(
    *,
    status: str,
    environment: Mapping[str, str],
    provenance: Mapping[str, Any],
    packages: Sequence[str],
    nupkg_digests: Mapping[str, str],
    phases: Sequence[PhaseResult],
    coverage: Sequence[Finding],
    narrowed: bool,
) -> dict[str, Any]:
    """Shape the JSON evidence document one acceptance run produces."""

    return {
        "schema": REPORT_SCHEMA,
        "status": status,
        "complete": not narrowed,
        "environment": dict(environment),
        "inputs": dict(provenance),
        "packages": list(packages),
        "nupkg_digests": dict(nupkg_digests),
        "phases": [phase.as_json() for phase in phases],
        "coverage": [item.as_json() for item in coverage],
        "scenarios": [mapping.as_json() for mapping in SCENARIO_MAP],
    }


def write_report(path: Path, document: Mapping[str, Any]) -> None:
    """Write one deterministic JSON evidence document."""

    if path.parent != Path(""):
        path.parent.mkdir(parents=True, exist_ok=True)
    path.write_text(json.dumps(document, sort_keys=True, indent=2) + "\n", encoding="utf-8")


def failed_checks(document: Mapping[str, Any]) -> tuple[str, ...]:
    """Return `phase[package]/check` for every failed assertion in a report document."""

    failures: list[str] = []
    for phase in document.get("phases", []):
        for finding in phase.get("findings", []):
            if not finding.get("ok"):
                failures.append(f"{phase['phase']}[{phase['package_id']}]/{finding['check']}")
    for finding in document.get("coverage", []):
        if not finding.get("ok"):
            failures.append(f"coverage/{finding['check']}")
    return tuple(failures)


def collapse(text: str) -> str:
    """Collapse multi-line command output into one printable line."""

    return " ".join(text.split())


def windows_target(architecture: str) -> release.Target:
    """Return the release target for one Windows architecture without restating triples."""

    name = f"windows-{architecture}"
    for target in release.TARGETS:
        if target.name == name:
            return target
    raise ChocolateyAcceptanceError(
        f"release.TARGETS has no target named {name!r}; observed "
        f"{tuple(target.name for target in release.TARGETS)!r}"
    )


def file_url(path: Path) -> str:
    """Return a `file:///` URL for one locally built artifact."""

    return path.resolve(strict=True).as_uri()


def flip_digest(digest: str) -> str:
    """Return a well-formed SHA-256 value that cannot match the archive bytes."""

    if len(digest) != 64:
        raise ChocolateyAcceptanceError(
            f"expected a 64-character SHA-256 value; observed {len(digest)} characters"
        )
    return digest[:-1] + ("0" if digest[-1] != "0" else "1")


def corrupt_archive(destination: Path) -> None:
    """Write bytes that are a well-formed download but not a valid ZIP archive."""

    destination.parent.mkdir(parents=True, exist_ok=True)
    destination.write_bytes(b"SKILLMOUNT-ACCEPTANCE-NOT-A-ZIP\n" * 128)


def zip_without_member(source: Path, destination: Path, member: str) -> None:
    """Copy one ZIP archive while dropping exactly one member."""

    destination.parent.mkdir(parents=True, exist_ok=True)
    with zipfile.ZipFile(source) as archive:
        names = tuple(archive.namelist())
        if member not in names:
            raise ChocolateyAcceptanceError(
                f"archive {source.name} has no member {member!r}; observed {names!r}"
            )
        with zipfile.ZipFile(destination, "w", zipfile.ZIP_DEFLATED) as output:
            for info in archive.infolist():
                if info.filename == member:
                    continue
                output.writestr(info, archive.read(info.filename))


def extract_executables(
    archive: Path, target: release.Target, *, tag: str, destination: Path
) -> None:
    """Extract only the two product executables from one release archive."""

    destination.mkdir(parents=True, exist_ok=True)
    root = release.asset_stem(tag, target)
    with zipfile.ZipFile(archive) as container:
        names = tuple(container.namelist())
        for name in release.executable_names(target):
            member = f"{root}/{name}"
            if member not in names:
                raise ChocolateyAcceptanceError(
                    f"archive {archive.name} has no member {member!r}; observed {names!r}"
                )
            path = destination / name
            path.write_bytes(container.read(member))
            path.chmod(0o755)


def append_install_script(root: Path, package_id: str, lines: Sequence[str]) -> Path:
    """Append deliberate corruption to one rendered install script."""

    script = root / "tools" / "chocolateyinstall.ps1"
    if not script.is_file():
        raise ChocolateyAcceptanceError(
            f"rendered package {package_id} has no tools/chocolateyinstall.ps1 at {script}"
        )
    with script.open("a", encoding="utf-8", newline="\n") as handle:
        handle.write("\n" + "\n".join(lines) + "\n")
    return script


def download_verified(url: str, destination: Path, *, expected_sha256: str) -> str:
    """Download one archive and require its bytes to match the recorded digest."""

    destination.parent.mkdir(parents=True, exist_ok=True)
    try:
        with urllib.request.urlopen(url) as response, destination.open("wb") as output:
            while chunk := response.read(DOWNLOAD_CHUNK):
                output.write(chunk)
    except OSError as error:
        raise ChocolateyAcceptanceError(f"cannot download {url}: {error}") from error
    observed = release.sha256_file(destination)
    if observed != expected_sha256:
        raise ChocolateyAcceptanceError(
            f"downloaded {url} has SHA-256 {observed}; expected {expected_sha256}"
        )
    return observed


@dataclass(frozen=True)
class LocalArchive:
    """One release archive available on disk and the digest a package must verify."""

    architecture: str
    target: release.Target
    path: Path
    sha256: str
    tag: str


def build_local_archive(
    *,
    repository: Path,
    binary_directory: Path,
    output_directory: Path,
    architecture: str,
    version: str,
    tag: str,
    commit: str,
) -> LocalArchive:
    """Package and digest one Windows archive from already-built product binaries."""

    target = windows_target(architecture)
    archive = release.package_release(
        repository,
        binary_directory,
        output_directory,
        target=target,
        version=version,
        tag=tag,
        commit=commit,
    )
    return LocalArchive(
        architecture=architecture,
        target=target,
        path=archive,
        sha256=release.sha256_file(archive),
        tag=tag,
    )


def local_inputs(
    channels: Any,
    *,
    repository: str,
    tag: str,
    commit: str,
    archives: Sequence[LocalArchive],
) -> package_channels.PackageInputs:
    """Assemble package inputs from locally built release archives and `file:///` URLs."""

    if not archives:
        raise ChocolateyAcceptanceError(
            "at least one locally built Windows archive is required to render package sources"
        )
    identities = tuple(
        sorted(
            (
                channels.ArchiveIdentity(
                    triple=archive.target.triple,
                    name=archive.path.name,
                    url=file_url(archive.path),
                    sha256=archive.sha256,
                )
                for archive in archives
            ),
            key=lambda identity: identity.triple,
        )
    )
    return channels.PackageInputs(
        repository=repository,
        version=release.stable_version_from_tag(tag),
        tag=tag,
        commit=commit,
        release_url=f"https://github.com/{repository}/releases/tag/{tag}",
        archives=identities,
    )


def replace_archive(
    inputs: package_channels.PackageInputs,
    *,
    triple: str,
    url: str | None = None,
    sha256: str | None = None,
) -> package_channels.PackageInputs:
    """Return package inputs with exactly one archive identity deliberately changed."""

    replaced = []
    found = False
    for archive in inputs.archives:
        if archive.triple != triple:
            replaced.append(archive)
            continue
        found = True
        changes: dict[str, str] = {}
        if url is not None:
            changes["url"] = url
        if sha256 is not None:
            changes["sha256"] = sha256
        replaced.append(dataclasses.replace(archive, **changes))
    if not found:
        raise ChocolateyAcceptanceError(
            f"package inputs record no archive for {triple!r}; observed "
            f"{tuple(archive.triple for archive in inputs.archives)!r}"
        )
    return dataclasses.replace(inputs, archives=tuple(replaced))


def inspection_errors(channels: Any) -> tuple[type[BaseException], ...]:
    """Return every error type structural inspection raises for an unacceptable candidate.

    Channel invariants raise the channel error, while the byte-level release helpers the
    inspectors reuse raise their own. A phase must record either as a finding instead of
    aborting the remaining matrix.
    """

    return (channels.ChannelError, release.ReleaseError, zipfile.BadZipFile)


def render_sources(
    channels: Any,
    inputs: package_channels.PackageInputs,
    *,
    template_directory: Path,
    output_directory: Path,
) -> dict[str, Path]:
    """Render both Chocolatey package sources through the shared generator.

    A template or token drift is a channel-level failure, so it is translated into this module's
    error type rather than escaping as a foreign exception.
    """

    if output_directory.exists():
        shutil.rmtree(output_directory)
    try:
        return channels.generate_chocolatey_sources(
            inputs,
            template_directory=template_directory,
            output_directory=output_directory,
        )
    except channels.ChannelError as error:
        raise ChocolateyAcceptanceError(
            f"cannot render Chocolatey package sources from {template_directory}: {error}"
        ) from error


def listing(root: Path) -> tuple[str, ...] | None:
    """List every file below one package folder, or return None when it is absent."""

    if not root.is_dir():
        return None
    entries: list[str] = []
    for path in sorted(root.rglob("*")):
        if path.is_dir() and not path.is_symlink():
            continue
        entries.append(path.relative_to(root).as_posix())
    return tuple(entries)


def parse_profile_paths(text: str) -> tuple[str, ...]:
    """Return the absolute PowerShell profile paths printed by the shell, in order."""

    paths: dict[str, str] = {}
    for line in text.splitlines():
        candidate = line.strip().strip('"')
        if not candidate:
            continue
        pure = PureWindowsPath(candidate)
        if not pure.drive or not pure.is_absolute():
            raise ChocolateyAcceptanceError(
                f"PowerShell emitted a profile path that is not absolute: {candidate!r}"
            )
        paths.setdefault(str(pure).casefold(), str(pure))
    if not paths:
        raise ChocolateyAcceptanceError("PowerShell reported no $PROFILE paths")
    return tuple(paths.values())


class ChocoGateway:
    """Small shell-free adapter around the Chocolatey CLI and Windows path helpers."""

    def __init__(
        self, *, environment: Mapping[str, str] | None = None, timeout: float = 1800.0
    ) -> None:
        """Locate every external executable this harness is allowed to invoke."""

        self.environment = dict(os.environ if environment is None else environment)
        self.timeout = timeout
        self.choco = shutil.which("choco")
        if self.choco is None:
            raise ChocolateyAcceptanceError(
                "choco is required on PATH to run the Chocolatey acceptance harness"
            )
        self.where = shutil.which("where.exe") or shutil.which("where")
        if self.where is None:
            raise ChocolateyAcceptanceError(
                "where.exe is required on PATH to resolve Chocolatey shims"
            )
        self.powershell = shutil.which("pwsh") or shutil.which("powershell")
        if self.powershell is None:
            raise ChocolateyAcceptanceError(
                "pwsh or powershell is required on PATH to read profile and PATH state"
            )
        self.root = chocolatey_root(self.environment)

    def _run(self, arguments: Sequence[str]) -> CommandResult:
        """Run one shell-free command and merge its output."""

        completed = subprocess.run(
            list(arguments),
            stdout=subprocess.PIPE,
            stderr=subprocess.STDOUT,
            check=False,
            timeout=self.timeout,
        )
        return CommandResult(
            arguments=tuple(arguments),
            returncode=completed.returncode,
            output=completed.stdout.decode("utf-8", errors="replace"),
        )

    def choco_version(self) -> str:
        """Return the Chocolatey version string."""

        result = self._run([self.choco, "--version"])
        if result.returncode != 0:
            raise ChocolateyAcceptanceError(
                f"choco --version failed with status {result.returncode}: {collapse(result.output)}"
            )
        return collapse(result.output)

    def list_local(self) -> str:
        """Return locally installed packages.

        Chocolatey v1 requires ``--local-only`` while v2 removed it, so the documented safety
        query is attempted first and the modern form is the fallback. Both failing is fatal.
        """

        legacy = self._run([self.choco, "list", "--local-only"])
        if legacy.returncode == 0:
            return legacy.output
        modern = self._run([self.choco, "list"])
        if modern.returncode == 0:
            return modern.output
        raise ChocolateyAcceptanceError(
            "cannot enumerate installed Chocolatey packages, so the host cannot be proved clean; "
            f"'choco list --local-only' exited {legacy.returncode} "
            f"({collapse(legacy.output)[:200]}) and 'choco list' exited {modern.returncode} "
            f"({collapse(modern.output)[:200]})"
        )

    def pack(self, nuspec: Path, *, output_directory: Path) -> CommandResult:
        """Pack one reviewed package source into a `.nupkg`."""

        output_directory.mkdir(parents=True, exist_ok=True)
        return self._run(
            [self.choco, "pack", str(nuspec), "--output-directory", str(output_directory)]
        )

    def install(
        self,
        package_id: str,
        *,
        version: str,
        source: Path,
        architecture: str,
    ) -> CommandResult:
        """Install one candidate package from a local source directory."""

        arguments = [
            self.choco,
            "install",
            package_id,
            "--version",
            version,
            "--source",
            str(source),
            "--yes",
            "--no-progress",
        ]
        if architecture == "x86":
            arguments.append("--forcex86")
        return self._run(arguments)

    def upgrade(
        self, package_id: str, *, version: str, source: Path, architecture: str
    ) -> CommandResult:
        """Upgrade one installed package to an exact candidate version."""

        arguments = [
            self.choco,
            "upgrade",
            package_id,
            "--version",
            version,
            "--source",
            str(source),
            "--yes",
            "--no-progress",
        ]
        if architecture == "x86":
            arguments.append("--forcex86")
        return self._run(arguments)

    def uninstall(self, package_id: str) -> CommandResult:
        """Uninstall one package, tolerating an already-absent package."""

        return self._run([self.choco, "uninstall", package_id, "--yes", "--no-progress"])

    def resolve(self, command: str) -> CommandResult:
        """Resolve one command name through the process command path."""

        return self._run([self.where, command])

    def shim_metadata(self, shim: Path) -> CommandResult:
        """Ask one Chocolatey shim to describe itself without running its target."""

        return self._run([str(shim), "--shimgen-noop"])

    def command_output(self, executable: Path, arguments: Sequence[str]) -> CommandResult:
        """Run one installed product command through its shim."""

        return self._run([str(executable), *arguments])

    def powershell_script(self, script: str) -> CommandResult:
        """Run one PowerShell expression with no profile and no interactive input."""

        return self._run(
            [self.powershell, "-NoProfile", "-NonInteractive", "-NoLogo", "-Command", script]
        )

    def powershell_version(self) -> str:
        """Return the `$PSVersionTable` evidence line."""

        result = self.powershell_script("$PSVersionTable | Out-String")
        if result.returncode != 0:
            raise ChocolateyAcceptanceError(
                f"cannot read $PSVersionTable: status {result.returncode}: "
                f"{collapse(result.output)}"
            )
        return collapse(result.output)

    def profile_paths(self) -> tuple[Path, ...]:
        """Return every PowerShell profile path whose bytes must not change."""

        result = self.powershell_script(
            "$PROFILE.AllUsersAllHosts; $PROFILE.AllUsersCurrentHost; "
            "$PROFILE.CurrentUserAllHosts; $PROFILE.CurrentUserCurrentHost"
        )
        if result.returncode != 0:
            raise ChocolateyAcceptanceError(
                f"cannot read $PROFILE paths: status {result.returncode}: "
                f"{collapse(result.output)}"
            )
        return tuple(Path(path) for path in parse_profile_paths(result.output))

    def environment_path(self, scope: str) -> str:
        """Return one PATH scope's stored value without modifying it."""

        if scope == "Process":
            return self.environment.get("PATH", "")
        result = self.powershell_script(
            f"[Environment]::GetEnvironmentVariable('Path','{scope}')"
        )
        if result.returncode != 0:
            raise ChocolateyAcceptanceError(
                f"cannot read the {scope} PATH value: status {result.returncode}: "
                f"{collapse(result.output)}"
            )
        return result.output.strip()


def collect_environment(gateway: ChocoGateway) -> dict[str, str]:
    """Record and print the manager, shell, and OS versions the evidence depends on."""

    evidence = {
        "choco": gateway.choco_version(),
        "powershell": gateway.powershell_version(),
        "os": collapse(f"{platform.system()} {platform.release()} {platform.version()}"),
        "machine": platform.machine(),
        "python": platform.python_version(),
        "chocolatey_root": str(gateway.root),
    }
    for name in sorted(evidence):
        print(f"{name}: {evidence[name]}")
    return evidence


def collect_path_values(gateway: ChocoGateway, targets: ResidueTargets) -> dict[str, str]:
    """Read every PATH scope whose stored value must not change."""

    return {scope: gateway.environment_path(scope) for scope in targets.path_scopes}


def create_residue_targets(gateway: ChocoGateway, work: Path) -> ResidueTargets:
    """Create harness-owned sentinels and resolve every residue target."""

    project = work / "project"
    project.mkdir(parents=True, exist_ok=True)
    project_sentinel = project / "skillmount-project-sentinel.json"
    project_sentinel.write_text(
        json.dumps({"owner": "chocolatey-acceptance", "kind": "project"}, sort_keys=True) + "\n",
        encoding="utf-8",
    )
    skills = work / "skills"
    skills.mkdir(parents=True, exist_ok=True)
    skill_sentinel = skills / "acceptance-source.md"
    skill_sentinel.write_text(
        "---\nname: acceptance-source\n---\n\nSkill-source sentinel bytes.\n", encoding="utf-8"
    )
    return ResidueTargets(
        profiles=gateway.profile_paths(),
        project_sentinel=project_sentinel,
        skill_sentinel=skill_sentinel,
        state_directory=state_directory(gateway.environment, windows=os.name == "nt"),
    )


@dataclass
class Context:
    """Everything one acceptance run's phases read, mutate, and record."""

    gateway: ChocoGateway
    channels: Any
    inputs: package_channels.PackageInputs
    identities: dict[str, Any]
    repository_path: Path
    template_directory: Path
    work: Path
    sources: dict[str, Path]
    nupkgs: dict[str, Path]
    nupkg_digests: dict[str, str]
    candidates: Path
    archives: dict[str, LocalArchive]
    prior_version: str
    residue_targets: ResidueTargets
    residue_before: dict[str, str]
    help_outputs: dict[str, str]

    @property
    def version(self) -> str:
        """Return the candidate version every phase asserts."""

        return self.inputs.version

    @property
    def tag(self) -> str:
        """Return the candidate tag every phase asserts."""

        return self.inputs.tag

    def package_folder(self, package_id: str) -> Path:
        """Return the Chocolatey-managed folder for one package id."""

        return package_folder_path(self.gateway.root, package_id)

    def shim_directory(self) -> str:
        """Return the Chocolatey command-path directory as evidence text."""

        return str(shim_directory_path(self.gateway.root))

    def shim(self, command: str) -> Path:
        """Return the expected Chocolatey shim path for one product command."""

        return shim_directory_path(self.gateway.root) / f"{command}.exe"

    def expected_metadata(self, architecture: str, *, tag: str, version: str) -> bytes:
        """Return the VERSION bytes one architecture's package must retain."""

        return release.version_metadata(
            version, tag, windows_target(architecture), self.inputs.commit
        )


@dataclass(frozen=True)
class InstallEvidence:
    """Everything one completed install is judged from, with no decision applied."""

    package_folder: Path
    names: tuple[str, ...] | None
    version_bytes: bytes | None
    executable_header: bytes
    selected_where: str
    other_where: str
    shim_path: Path
    shim_target: str | None
    shim_metadata: str
    sidecars: tuple[tuple[str, str], ...]


def read_download_sidecars(
    folder: Path, names: Sequence[str] | None
) -> tuple[tuple[str, str], ...]:
    """Read every extraction sidecar a package folder carries, in listing order."""

    if names is None:
        return ()
    contents: list[tuple[str, str]] = []
    for name in download_sidecar_names(tuple(listing_index(names).values())):
        path = folder / PureWindowsPath(name)
        if path.is_file():
            contents.append((name, path.read_text(encoding="utf-8", errors="replace")))
    return tuple(contents)


def collect_install_evidence(context: Context, selection: PackageSelection) -> InstallEvidence:
    """Collect every observation the pure install judges need, deciding nothing."""

    folder = context.package_folder(selection.package_id)
    names = listing(folder)
    version_path = folder / "tools" / release.VERSION_FILE
    executable = folder / "tools" / selection.selected_executable
    shim = context.shim(selection.command)
    metadata = ""
    target: str | None = None
    if shim.is_file():
        metadata = context.gateway.shim_metadata(shim).output
        try:
            target = parse_shim_target(
                metadata, shim_path=str(shim), executable=selection.selected_executable
            )
        except ChocolateyAcceptanceError:
            target = None
    return InstallEvidence(
        package_folder=folder,
        names=names,
        version_bytes=version_path.read_bytes() if version_path.is_file() else None,
        executable_header=read_header(executable) if executable.is_file() else b"",
        selected_where=context.gateway.resolve(selection.command).output,
        other_where=context.gateway.resolve(selection.other_command).output,
        shim_path=shim,
        shim_target=target,
        shim_metadata=metadata,
        sidecars=read_download_sidecars(folder, names),
    )


def install_evidence_pairs(evidence: InstallEvidence) -> tuple[tuple[str, str], ...]:
    """Return the recorded values one install-backed phase reports."""

    return (
        ("package_folder", str(evidence.package_folder)),
        ("shim", str(evidence.shim_path)),
        ("shim_target", "" if evidence.shim_target is None else evidence.shim_target),
        ("resolved", collapse(evidence.selected_where)),
    )


def archive_reference(context: Context, architecture: str) -> str:
    """Return the archive-root name one architecture's release archive carries.

    The root is the archive's own name without its suffix, so it identifies the archive both in a
    download URL and in an extraction listing.
    """

    return release.asset_stem(context.tag, windows_target(architecture))


def folder_findings_or_absence(
    context: Context,
    selection: PackageSelection,
    evidence: InstallEvidence,
    *,
    moment: str,
    architecture: str,
) -> tuple[Finding, ...]:
    """Judge a package folder's contents, or report that it does not exist at all."""

    if evidence.names is None:
        return (
            Finding(
                "package-folder-present",
                False,
                f"expected {evidence.package_folder} to exist after {moment}",
            ),
        )
    findings = list(package_folder_findings(evidence.names, selection, version=context.version))
    findings.extend(
        download_provenance_findings(
            evidence.sidecars,
            architecture=architecture,
            expected_reference=archive_reference(context, architecture),
            other_reference=archive_reference(context, other_architecture(architecture)),
        )
    )
    return tuple(findings)


@contextlib.contextmanager
def installed(
    context: Context,
    package_id: str,
    *,
    version: str,
    source: Path,
    architecture: str,
) -> Iterator[CommandResult]:
    """Install one package and guarantee its uninstall, including on failure."""

    result = context.gateway.install(
        package_id, version=version, source=source, architecture=architecture
    )
    try:
        yield result
    finally:
        context.gateway.uninstall(package_id)


def pack_phase(context: Context, selection: PackageSelection) -> PhaseResult:
    """Pack one reviewed package source and require the exact candidate filename."""

    identity = context.identities[selection.package_id]
    nuspec = context.sources[selection.package_id] / f"{selection.package_id}.nuspec"
    result = context.gateway.pack(nuspec, output_directory=context.candidates)
    expected = context.channels.nupkg_name(identity, context.version)
    candidate = context.candidates / expected
    findings = [
        Finding(
            "pack-status",
            result.returncode == 0,
            f"expected {result.command} to succeed; observed status {result.returncode} with "
            f"output {collapse(result.output)[-400:]!r}",
        ),
        Finding(
            "nupkg-name",
            candidate.is_file(),
            f"expected {expected!r} in {context.candidates}; observed "
            f"{tuple(sorted(path.name for path in context.candidates.glob('*.nupkg')))!r}",
        ),
    ]
    if candidate.is_file():
        context.nupkgs[selection.package_id] = candidate
        findings.append(
            Finding(
                "nupkg-is-zip",
                zipfile.is_zipfile(candidate),
                f"expected {expected!r} to be a readable ZIP container",
            )
        )
    return PhaseResult(
        name="pack",
        package_id=selection.package_id,
        architecture="",
        findings=tuple(findings),
        evidence=(("nupkg", str(candidate)),),
    )


def mismatched_pair_finding(
    context: Context, selections: Sequence[PackageSelection]
) -> Finding:
    """Prove pair inspection blocks a candidate whose provenance differs from its partner."""

    channels = context.channels
    first, second = selections[0], selections[1]
    triple = windows_target("x64").triple
    tampered = replace_archive(
        context.inputs, triple=triple, sha256=flip_digest(context.inputs.archive(triple).sha256)
    )
    rendered = render_sources(
        channels,
        tampered,
        template_directory=context.template_directory,
        output_directory=context.work / "mismatched-pair" / "sources",
    )
    output = context.work / "mismatched-pair" / "nupkgs"
    result = context.gateway.pack(
        rendered[first.package_id] / f"{first.package_id}.nuspec", output_directory=output
    )
    candidate = output / channels.nupkg_name(
        context.identities[first.package_id], tampered.version
    )
    if result.returncode != 0 or not candidate.is_file():
        return Finding(
            "mismatched-pair-blocked",
            False,
            f"could not pack a deliberately mismatched candidate: status {result.returncode}; "
            f"output {collapse(result.output)[-300:]!r}",
        )
    pair = {first.package_id: candidate, second.package_id: context.nupkgs[second.package_id]}
    try:
        channels.inspect_nupkg_pair(pair, context.inputs)
    except inspection_errors(channels) as error:
        return Finding(
            "mismatched-pair-blocked",
            True,
            f"pair inspection rejected a mismatched x64 digest: {error}",
        )
    return Finding(
        "mismatched-pair-blocked",
        False,
        "expected pair inspection to reject a candidate whose x64 digest differs from preflight; "
        "inspection accepted it",
    )


def inspect_phase(context: Context, selections: Sequence[PackageSelection]) -> PhaseResult:
    """Inspect both rendered sources and both candidates, and prove a bad pair is blocked."""

    channels = context.channels
    findings: list[Finding] = []
    try:
        channels.inspect_chocolatey_sources(context.sources, context.inputs)
        findings.append(
            Finding("sources-valid", True, "both rendered package sources satisfied inspection")
        )
    except inspection_errors(channels) as error:
        findings.append(Finding("sources-valid", False, f"source inspection failed: {error}"))

    missing = tuple(
        selection.package_id
        for selection in selections
        if selection.package_id not in context.nupkgs
    )
    if missing:
        findings.append(
            Finding(
                "nupkg-pair-valid",
                False,
                f"expected packed candidates for both ids; missing {missing!r}",
            )
        )
        return PhaseResult("inspect", "pair", "", tuple(findings))

    try:
        digests = channels.inspect_nupkg_pair(context.nupkgs, context.inputs)
        context.nupkg_digests.update(digests)
        findings.append(
            Finding(
                "nupkg-pair-valid",
                True,
                f"both candidates satisfied pair inspection with digests {digests!r}",
            )
        )
        findings.append(
            Finding(
                "nupkg-digests-recorded",
                set(digests) == {selection.package_id for selection in selections},
                f"expected digests for both ids; observed {tuple(sorted(digests))!r}",
            )
        )
    except inspection_errors(channels) as error:
        findings.append(Finding("nupkg-pair-valid", False, f"pair inspection failed: {error}"))

    findings.append(mismatched_pair_finding(context, selections))
    return PhaseResult(
        name="inspect",
        package_id="pair",
        architecture="",
        findings=tuple(findings),
        evidence=tuple(sorted(context.nupkg_digests.items())),
    )


def install_phase(
    context: Context,
    selection: PackageSelection,
    *,
    architecture: str,
    result: CommandResult,
    evidence: InstallEvidence,
) -> PhaseResult:
    """Judge one architecture's install: status, selection, retained bytes, and machine type."""

    other = windows_target(other_architecture(architecture))
    other_url = context.inputs.archive(other.triple).url
    findings = [
        Finding(
            "install-status",
            result.returncode == 0,
            f"expected {result.command} to succeed; observed status {result.returncode} with "
            f"output {collapse(result.output)[-500:]!r}",
        ),
        Finding(
            "selected-architecture-only",
            other_url.casefold() not in result.output.casefold(),
            f"expected the install not to reference the {other.triple} archive {other_url!r}",
        ),
        installed_version_finding(
            context.gateway.list_local(),
            package_id=selection.package_id,
            expected_version=context.version,
        ),
    ]
    findings.extend(
        version_file_findings(
            evidence.version_bytes,
            expected=context.expected_metadata(
                architecture, tag=context.tag, version=context.version
            ),
        )
    )
    findings.append(
        machine_finding(
            evidence.executable_header,
            architecture=architecture,
            label=f"{selection.package_id} tools/{selection.selected_executable}",
        )
    )
    findings.extend(
        folder_findings_or_absence(
            context, selection, evidence, moment="install", architecture=architecture
        )
    )
    return PhaseResult(
        name=f"install-{architecture}",
        package_id=selection.package_id,
        architecture=architecture,
        findings=tuple(findings),
        evidence=install_evidence_pairs(evidence),
    )


def selected_only_phase(
    context: Context,
    selection: PackageSelection,
    *,
    architecture: str,
    evidence: InstallEvidence,
) -> PhaseResult:
    """Judge that the package retains and exposes exactly its selected executable."""

    findings = list(
        folder_findings_or_absence(
            context, selection, evidence, moment="install", architecture=architecture
        )
    )
    findings.extend(
        version_file_findings(
            evidence.version_bytes,
            expected=context.expected_metadata(
                architecture, tag=context.tag, version=context.version
            ),
        )
    )
    findings.append(
        machine_finding(
            evidence.executable_header,
            architecture=architecture,
            label=f"{selection.package_id} tools/{selection.selected_executable}",
        )
    )
    findings.extend(
        shim_findings(
            selection,
            selected_where=evidence.selected_where,
            other_where=evidence.other_where,
            shim_target=evidence.shim_target,
            package_folder=str(evidence.package_folder),
            shim_directory=context.shim_directory(),
        )
    )
    return PhaseResult(
        name="selected-only",
        package_id=selection.package_id,
        architecture=architecture,
        findings=tuple(findings),
        evidence=install_evidence_pairs(evidence),
    )


def shim_phase(
    context: Context,
    selection: PackageSelection,
    *,
    architecture: str,
    evidence: InstallEvidence,
) -> PhaseResult:
    """Judge Chocolatey command-path ownership for one installed package."""

    return PhaseResult(
        name="shim",
        package_id=selection.package_id,
        architecture=architecture,
        findings=shim_findings(
            selection,
            selected_where=evidence.selected_where,
            other_where=evidence.other_where,
            shim_target=evidence.shim_target,
            package_folder=str(evidence.package_folder),
            shim_directory=context.shim_directory(),
        ),
        evidence=install_evidence_pairs(evidence),
    )


def version_phase(
    context: Context, selection: PackageSelection, *, architecture: str
) -> PhaseResult:
    """Judge the installed command's reported version."""

    result = context.gateway.command_output(context.shim(selection.command), ["--version"])
    return PhaseResult(
        name="version",
        package_id=selection.package_id,
        architecture=architecture,
        findings=version_findings(result, selection, expected_version=context.version),
        evidence=(("output", collapse(result.output)),),
    )


def help_phase(context: Context, selection: PackageSelection, *, architecture: str) -> PhaseResult:
    """Judge the installed command's help output against the pair member's, when observed."""

    result = context.gateway.command_output(context.shim(selection.command), ["--help"])
    findings = help_findings(
        result, selection, pair_output=context.help_outputs.get(selection.other_command)
    )
    context.help_outputs[selection.command] = result.output
    return PhaseResult(
        name="help",
        package_id=selection.package_id,
        architecture=architecture,
        findings=findings,
        evidence=(("output", collapse(result.output)[:400]),),
    )


def uninstall_phase(
    context: Context, selection: PackageSelection, *, architecture: str
) -> PhaseResult:
    """Judge that a lone uninstall removes exactly the package folder and its shim."""

    folder = context.package_folder(selection.package_id)
    return PhaseResult(
        name="uninstall",
        package_id=selection.package_id,
        architecture=architecture,
        findings=cleanup_findings(
            selection,
            package_folder_names=listing(folder),
            where_output=context.gateway.resolve(selection.command).output,
            package_folder=str(folder),
        ),
        evidence=(("package_folder", str(folder)),),
    )


def run_install_session(
    context: Context,
    selection: PackageSelection,
    *,
    architecture: str,
    requested: Sequence[str],
) -> list[PhaseResult]:
    """Install once, judge every requested phase that needs that install, then uninstall."""

    session = X64_SESSION_PHASES if architecture == "x64" else X86_SESSION_PHASES
    wanted = [name for name in session if name in requested]
    if not wanted and not ("uninstall" in requested and architecture == "x64"):
        return []
    results: list[PhaseResult] = []
    with installed(
        context,
        selection.package_id,
        version=context.version,
        source=context.candidates,
        architecture=architecture,
    ) as result:
        evidence = collect_install_evidence(context, selection)
        if f"install-{architecture}" in wanted:
            results.append(
                install_phase(
                    context,
                    selection,
                    architecture=architecture,
                    result=result,
                    evidence=evidence,
                )
            )
        if "selected-only" in wanted:
            results.append(
                selected_only_phase(
                    context, selection, architecture=architecture, evidence=evidence
                )
            )
        if "shim" in wanted:
            results.append(
                shim_phase(context, selection, architecture=architecture, evidence=evidence)
            )
        if "version" in wanted:
            results.append(version_phase(context, selection, architecture=architecture))
        if "help" in wanted:
            results.append(help_phase(context, selection, architecture=architecture))
    if "uninstall" in requested and architecture == "x64":
        results.append(uninstall_phase(context, selection, architecture=architecture))
    return results


def rehearse_prior_package(
    context: Context, selection: PackageSelection
) -> tuple[Path, str, str] | str:
    """Build a prior-version package the candidate can upgrade over.

    The payload is the candidate's own verified archives, because a prior release's binaries are
    not reproducible from this checkout. Only the declared package version, tag, and retained
    release metadata differ, which is exactly what the upgrade lifecycle must replace.
    """

    prior_version = context.prior_version
    prior_tag = f"v{prior_version}"
    root = context.work / "prior"
    archives: list[LocalArchive] = []
    for architecture, archive in sorted(context.archives.items()):
        binaries = root / "binaries" / architecture
        try:
            extract_executables(
                archive.path, archive.target, tag=archive.tag, destination=binaries
            )
        except (ChocolateyAcceptanceError, OSError, zipfile.BadZipFile) as error:
            return f"cannot extract {archive.path.name} to rehearse {prior_tag}: {error}"
        archives.append(
            build_local_archive(
                repository=context.repository_path,
                binary_directory=binaries,
                output_directory=root / "archives",
                architecture=architecture,
                version=prior_version,
                tag=prior_tag,
                commit=context.inputs.commit,
            )
        )
    if not archives:
        return "no verified archive is available to rehearse a prior package"
    inputs = local_inputs(
        context.channels,
        repository=context.inputs.repository,
        tag=prior_tag,
        commit=context.inputs.commit,
        archives=archives,
    )
    rendered = render_sources(
        context.channels,
        inputs,
        template_directory=context.template_directory,
        output_directory=root / "sources",
    )
    output = root / "nupkgs"
    result = context.gateway.pack(
        rendered[selection.package_id] / f"{selection.package_id}.nuspec",
        output_directory=output,
    )
    if result.returncode != 0:
        return f"cannot pack the rehearsed {prior_tag} package: {collapse(result.output)[-300:]}"
    return output, prior_version, prior_tag


def upgrade_phase(context: Context, selection: PackageSelection) -> PhaseResult:
    """Judge an upgrade from a rehearsed prior package to the candidate version."""

    prepared = rehearse_prior_package(context, selection)
    if isinstance(prepared, str):
        return PhaseResult(
            name="upgrade",
            package_id=selection.package_id,
            architecture="x64",
            findings=(Finding("prior-package-available", False, prepared),),
        )
    prior_source, prior_version, prior_tag = prepared
    findings: list[Finding] = [
        Finding(
            "prior-package-available",
            True,
            f"rehearsed {prior_tag} package built at {prior_source}",
        )
    ]
    recorded: tuple[tuple[str, str], ...] = ()
    with installed(
        context,
        selection.package_id,
        version=prior_version,
        source=prior_source,
        architecture="x64",
    ) as prior_result:
        findings.append(
            Finding(
                "prior-install-status",
                prior_result.returncode == 0,
                f"expected the rehearsed {prior_tag} install to succeed; observed status "
                f"{prior_result.returncode} with {collapse(prior_result.output)[-400:]!r}",
            )
        )
        findings.append(
            installed_version_finding(
                context.gateway.list_local(),
                package_id=selection.package_id,
                expected_version=prior_version,
            )
        )
        upgraded = context.gateway.upgrade(
            selection.package_id,
            version=context.version,
            source=context.candidates,
            architecture="x64",
        )
        findings.append(
            Finding(
                "upgrade-status",
                upgraded.returncode == 0,
                f"expected {upgraded.command} to succeed; observed status {upgraded.returncode} "
                f"with {collapse(upgraded.output)[-400:]!r}",
            )
        )
        findings.append(
            installed_version_finding(
                context.gateway.list_local(),
                package_id=selection.package_id,
                expected_version=context.version,
            )
        )
        evidence = collect_install_evidence(context, selection)
        findings.extend(
            folder_findings_or_absence(
                context, selection, evidence, moment="upgrade", architecture="x64"
            )
        )
        markers = staleness_markers(
            prior_version=prior_version,
            prior_tag=prior_tag,
            version=context.version,
            tag=context.tag,
        )
        findings.extend(
            version_file_findings(
                evidence.version_bytes,
                expected=context.expected_metadata(
                    "x64", tag=context.tag, version=context.version
                ),
                forbidden=markers,
            )
        )
        if not markers:
            findings.append(
                Finding(
                    "version-metadata-staleness-skipped",
                    True,
                    "skipped the stale-metadata check: the rehearsed prior release "
                    f"{prior_version!r} (tag {prior_tag!r}) equals the candidate release "
                    f"{context.version!r} (tag {context.tag!r}), so no marker can distinguish "
                    "replaced metadata from retained metadata",
                )
            )
        findings.append(
            machine_finding(
                evidence.executable_header,
                architecture="x64",
                label=f"{selection.package_id} upgraded tools/{selection.selected_executable}",
            )
        )
        findings.extend(
            shim_findings(
                selection,
                selected_where=evidence.selected_where,
                other_where=evidence.other_where,
                shim_target=evidence.shim_target,
                package_folder=str(evidence.package_folder),
                shim_directory=context.shim_directory(),
            )
        )
        findings.extend(
            version_findings(
                context.gateway.command_output(context.shim(selection.command), ["--version"]),
                selection,
                expected_version=context.version,
            )
        )
        recorded = install_evidence_pairs(evidence) + (
            ("rehearsed_prior", f"{prior_tag} package containing the candidate archives"),
        )
    return PhaseResult(
        name="upgrade",
        package_id=selection.package_id,
        architecture="x64",
        findings=tuple(findings),
        evidence=recorded,
    )


def co_install_phase(
    context: Context,
    first: PackageSelection,
    second: PackageSelection,
    *,
    results: tuple[CommandResult, CommandResult],
    evidence: tuple[InstallEvidence, InstallEvidence],
) -> PhaseResult:
    """Judge that both packages coexist with independently owned shims."""

    findings: list[Finding] = []
    for selection, result in zip((first, second), results):
        findings.append(
            Finding(
                f"install-status:{selection.package_id}",
                result.returncode == 0,
                f"expected {result.command} to succeed; observed status {result.returncode}",
            )
        )
    for selection, item in zip((first, second), evidence):
        findings.extend(
            shim_findings(
                selection,
                selected_where=item.selected_where,
                other_where=item.other_where,
                shim_target=item.shim_target,
                package_folder=str(item.package_folder),
                shim_directory=context.shim_directory(),
                pair_installed=True,
            )
        )
        findings.extend(
            folder_findings_or_absence(
                context, selection, item, moment="co-installation", architecture="x64"
            )
        )
        findings.extend(
            version_findings(
                context.gateway.command_output(context.shim(selection.command), ["--version"]),
                selection,
                expected_version=context.version,
            )
        )
    findings.extend(
        independent_ownership_findings(
            first,
            second,
            left_target=evidence[0].shim_target,
            right_target=evidence[1].shim_target,
            left_folder=str(evidence[0].package_folder),
            right_folder=str(evidence[1].package_folder),
        )
    )
    return PhaseResult(
        name="co-install",
        package_id=f"{first.package_id},{second.package_id}",
        architecture="x64",
        findings=tuple(findings),
        evidence=(
            (f"shim_target:{first.package_id}", str(evidence[0].shim_target)),
            (f"shim_target:{second.package_id}", str(evidence[1].shim_target)),
        ),
    )


def cross_uninstall_phase(
    context: Context, removed: PackageSelection, retained: PackageSelection
) -> PhaseResult:
    """Judge that uninstalling one co-installed package leaves the other functional."""

    context.gateway.uninstall(removed.package_id)
    removed_folder = context.package_folder(removed.package_id)
    findings = list(
        cleanup_findings(
            removed,
            package_folder_names=listing(removed_folder),
            where_output=context.gateway.resolve(removed.command).output,
            package_folder=str(removed_folder),
        )
    )
    retained_folder = context.package_folder(retained.package_id)
    findings.extend(
        survivor_findings(
            retained,
            package_folder_names=listing(retained_folder),
            version_result=context.gateway.command_output(
                context.shim(retained.command), ["--version"]
            ),
            expected_version=context.version,
        )
    )
    findings.append(
        installed_version_finding(
            context.gateway.list_local(),
            package_id=retained.package_id,
            expected_version=context.version,
        )
    )
    return PhaseResult(
        name="cross-uninstall",
        package_id=f"{removed.package_id},{retained.package_id}",
        architecture="x64",
        findings=tuple(findings),
        evidence=(("removed", str(removed_folder)), ("retained", str(retained_folder))),
    )


def pair_phases(
    context: Context, selections: Sequence[PackageSelection], *, requested: Sequence[str]
) -> list[PhaseResult]:
    """Co-install both packages, then uninstall one and judge the survivor."""

    wanted = [name for name in PAIR_PHASES if name in requested]
    if not wanted:
        return []
    first, second = selections[0], selections[1]
    results: list[PhaseResult] = []
    with installed(
        context,
        first.package_id,
        version=context.version,
        source=context.candidates,
        architecture="x64",
    ) as first_result:
        with installed(
            context,
            second.package_id,
            version=context.version,
            source=context.candidates,
            architecture="x64",
        ) as second_result:
            first_evidence = collect_install_evidence(context, first)
            second_evidence = collect_install_evidence(context, second)
            if "co-install" in wanted:
                results.append(
                    co_install_phase(
                        context,
                        first,
                        second,
                        results=(first_result, second_result),
                        evidence=(first_evidence, second_evidence),
                    )
                )
            if "cross-uninstall" in wanted:
                results.append(cross_uninstall_phase(context, first, second))
    return results


def residue_phase(context: Context) -> PhaseResult:
    """Judge that no profile, PATH value, user file, or product state changed."""

    after = residue_snapshot(
        context.residue_targets,
        path_values=collect_path_values(context.gateway, context.residue_targets),
    )
    return PhaseResult(
        name="residue",
        package_id="pair",
        architecture="",
        findings=residue_findings(context.residue_before, after),
        evidence=(
            ("project_sentinel", str(context.residue_targets.project_sentinel)),
            ("skill_sentinel", str(context.residue_targets.skill_sentinel)),
            ("state_directory", str(context.residue_targets.state_directory)),
            ("profiles", ", ".join(str(path) for path in context.residue_targets.profiles)),
        ),
    )


@dataclass(frozen=True)
class NegativeCase:
    """One deliberate corruption and the failure mode it must produce."""

    phase: str
    architecture: str
    source: Path
    version: str
    mode: str
    expected_markers: tuple[str, ...] = ()
    forbidden_markers: tuple[str, ...] = ()


def negative_case(context: Context, selection: PackageSelection, phase: str) -> NegativeCase | str:
    """Build one negative case's corrupted candidate, or explain why it is unavailable."""

    channels = context.channels
    identity = context.identities[selection.package_id]
    root = context.work / "negative" / f"{selection.package_id}-{phase}"
    architecture = "x86" if phase.endswith("x86") else "x64"
    target = windows_target(architecture)
    inputs = context.inputs
    mode = "install-fails"
    expected: tuple[str, ...] = ()
    forbidden: tuple[str, ...] = (SUCCESS_MARKER,)
    appended: tuple[str, ...] = ()
    selected_path = f"Join-Path $PSScriptRoot '{selection.selected_executable}'"
    other_path = f"Join-Path $PSScriptRoot '{selection.other_executable}'"

    if phase in ("checksum-mismatch-x64", "checksum-mismatch-x86"):
        wrong = flip_digest(inputs.archive(target.triple).sha256)
        inputs = replace_archive(inputs, triple=target.triple, sha256=wrong)
        other = windows_target("x86" if architecture == "x64" else "x64")
        expected = ("checksum", wrong)
        forbidden = (SUCCESS_MARKER, inputs.archive(other.triple).url)
    elif phase in ("malformed-archive", "missing-selected-binary"):
        archive = context.archives.get(architecture)
        if archive is None:
            return (
                f"{phase} rewrites archive bytes, so a verified {architecture} archive is "
                "required; none is available in this run"
            )
        mutated = root / "archives" / archive.path.name
        if phase == "malformed-archive":
            corrupt_archive(mutated)
            expected = (archive.path.name,)
        else:
            member = f"{release.asset_stem(archive.tag, target)}/{selection.selected_executable}"
            try:
                zip_without_member(archive.path, mutated, member)
            except (ChocolateyAcceptanceError, OSError, zipfile.BadZipFile) as error:
                return f"cannot build the {phase} archive: {error}"
            expected = (selection.selected_executable,)
        inputs = replace_archive(
            inputs,
            triple=target.triple,
            url=file_url(mutated),
            sha256=release.sha256_file(mutated),
        )
    elif phase == "retained-unselected-binary":
        mode = "install-succeeds-invalid"
        forbidden = ()
        appended = (
            f"$retained = {other_path}",
            f"Copy-Item -LiteralPath ({selected_path}) -Destination $retained -Force",
            "Set-Content -LiteralPath ($retained + '"
            + IGNORE_SUFFIX
            + "') -Value 'acceptance' -Encoding ASCII",
        )
    elif phase == "extra-shim":
        mode = "install-succeeds-invalid"
        forbidden = ()
        appended = (
            f"Copy-Item -LiteralPath ({selected_path}) -Destination ({other_path}) -Force",
        )
    elif phase == "interrupted-install":
        expected = (INTERRUPTION_MARKER,)
        appended = (f"throw '{INTERRUPTION_MARKER}'",)
    elif phase == "repeated-install":
        mode = "repeat-rejected"
        forbidden = ()
        expected = ("already installed",)
    else:
        raise ChocolateyAcceptanceError(
            f"unknown negative phase {phase!r}; expected one of {NEGATIVE_PHASES!r}"
        )

    rendered = render_sources(
        channels,
        inputs,
        template_directory=context.template_directory,
        output_directory=root / "sources",
    )
    source_root = rendered[selection.package_id]
    if appended:
        append_install_script(source_root, selection.package_id, appended)
    output = root / "nupkgs"
    result = context.gateway.pack(
        source_root / f"{selection.package_id}.nuspec", output_directory=output
    )
    candidate = output / channels.nupkg_name(identity, inputs.version)
    if result.returncode != 0 or not candidate.is_file():
        return (
            f"cannot pack the {phase} candidate: status {result.returncode}; "
            f"output {collapse(result.output)[-300:]}"
        )
    return NegativeCase(
        phase=phase,
        architecture=architecture,
        source=output,
        version=inputs.version,
        mode=mode,
        expected_markers=expected,
        forbidden_markers=forbidden,
    )


def invalid_install_findings(
    context: Context, selection: PackageSelection, phase: str
) -> tuple[Finding, ...]:
    """Require this harness's own validators to reject a corrupted but installable package."""

    evidence = collect_install_evidence(context, selection)
    if phase == "retained-unselected-binary":
        if evidence.names is None:
            return (
                Finding(
                    "corruption-detected",
                    False,
                    f"expected {evidence.package_folder} to exist so its contents can be judged",
                ),
            )
        judged = package_folder_findings(evidence.names, selection, version=context.version)
        rejected = {item.check for item in judged if not item.ok}
        return (
            Finding(
                "corruption-detected",
                {"unselected-executable-absent", "ignore-marker-absent"} <= rejected,
                "expected package-folder validation to reject the retained "
                f"{selection.other_executable} and its {IGNORE_SUFFIX} marker; "
                f"failed checks {tuple(sorted(rejected))!r}",
            ),
        )
    judged = shim_findings(
        selection,
        selected_where=evidence.selected_where,
        other_where=evidence.other_where,
        shim_target=evidence.shim_target,
        package_folder=str(evidence.package_folder),
        shim_directory=context.shim_directory(),
    )
    rejected = {item.check for item in judged if not item.ok}
    required = set(EXTRA_SHIM_REJECTIONS)
    return (
        Finding(
            "corruption-detected",
            required <= rejected,
            f"expected shim validation to reject the extra {selection.other_command} shim the "
            f"{selection.package_id} package installed; expected failed checks "
            f"{tuple(sorted(required))!r}; observed failed checks {tuple(sorted(rejected))!r}",
        ),
    )


def repeated_install_findings(
    context: Context, selection: PackageSelection, case: NegativeCase, first: CommandResult
) -> tuple[Finding, ...]:
    """Judge that a second install of the same version changes nothing and duplicates nothing."""

    findings = [
        Finding(
            "first-install-status",
            first.returncode == 0,
            f"expected the first {selection.package_id} install to succeed; observed status "
            f"{first.returncode} with {collapse(first.output)[-400:]!r}",
        )
    ]
    before = listing(context.package_folder(selection.package_id))
    repeated = context.gateway.install(
        selection.package_id,
        version=case.version,
        source=case.source,
        architecture=case.architecture,
    )
    findings.extend(
        failure_findings(
            repeated,
            expected_markers=case.expected_markers,
            forbidden_markers=case.forbidden_markers,
            require_nonzero=False,
        )
    )
    after = listing(context.package_folder(selection.package_id))
    findings.append(
        Finding(
            "package-folder-unchanged",
            before is not None and after == before,
            f"expected the repeated install to change nothing; observed {before!r} then {after!r}",
        )
    )
    evidence = collect_install_evidence(context, selection)
    findings.extend(
        folder_findings_or_absence(
            context, selection, evidence, moment="reinstall", architecture=case.architecture
        )
    )
    findings.extend(
        shim_findings(
            selection,
            selected_where=evidence.selected_where,
            other_where=evidence.other_where,
            shim_target=evidence.shim_target,
            package_folder=str(evidence.package_folder),
            shim_directory=context.shim_directory(),
        )
    )
    return tuple(findings)


def negative_phase(context: Context, selection: PackageSelection, phase: str) -> PhaseResult:
    """Run one negative case and require its specific failure mode plus complete cleanup."""

    prepared = negative_case(context, selection, phase)
    if isinstance(prepared, str):
        return PhaseResult(
            name=phase,
            package_id=selection.package_id,
            architecture="",
            findings=(Finding("candidate-available", False, prepared),),
        )
    findings: list[Finding] = []
    with installed(
        context,
        selection.package_id,
        version=prepared.version,
        source=prepared.source,
        architecture=prepared.architecture,
    ) as result:
        if prepared.mode == "install-fails":
            findings.extend(
                failure_findings(
                    result,
                    expected_markers=prepared.expected_markers,
                    forbidden_markers=prepared.forbidden_markers,
                )
            )
        elif prepared.mode == "install-succeeds-invalid":
            findings.append(
                Finding(
                    "install-status",
                    result.returncode == 0,
                    f"expected the corrupted {phase} install to complete so its result can be "
                    f"judged; observed status {result.returncode} with "
                    f"{collapse(result.output)[-400:]!r}",
                )
            )
            findings.extend(invalid_install_findings(context, selection, phase))
        else:
            findings.extend(repeated_install_findings(context, selection, prepared, result))
    folder = context.package_folder(selection.package_id)
    findings.extend(
        cleanup_findings(
            selection,
            package_folder_names=listing(folder),
            where_output=context.gateway.resolve(selection.command).output,
            package_folder=str(folder),
        )
    )
    return PhaseResult(
        name=phase,
        package_id=selection.package_id,
        architecture=prepared.architecture,
        findings=tuple(findings),
        evidence=(("candidate_source", str(prepared.source)),),
    )


def cargo_version_from_manifest(text: str) -> str:
    """Return the `[package]` version declared in a Cargo manifest."""

    start = text.find(CARGO_PACKAGE_HEADER)
    if start < 0:
        raise ChocolateyAcceptanceError(
            f"Cargo manifest has no {CARGO_PACKAGE_HEADER} table; expected exactly one"
        )
    body = text[start + len(CARGO_PACKAGE_HEADER) :]
    next_section = CARGO_SECTION_PATTERN.search(body)
    if next_section is not None:
        body = body[: next_section.start()]
    matches = CARGO_VERSION_PATTERN.findall(body)
    if len(matches) != 1:
        raise ChocolateyAcceptanceError(
            f"Cargo {CARGO_PACKAGE_HEADER} table declares {len(matches)} version keys ({matches}); "
            "expected exactly 1"
        )
    return release.validate_stable_version(matches[0])


def manifest_version(repository: Path) -> str:
    """Return the root package version the locally built executables report."""

    manifest = repository / CARGO_MANIFEST_NAME
    if not manifest.is_file():
        raise ChocolateyAcceptanceError(
            f"{manifest} is missing; expected a Cargo manifest because without --inputs the "
            "candidate version comes from the manifest the local binaries were built from"
        )
    return cargo_version_from_manifest(manifest.read_text(encoding="utf-8"))


def local_release_identity(repository: Path, *, tag: str | None) -> tuple[str, str]:
    """Derive the version and tag a locally built candidate must declare."""

    version = manifest_version(repository)
    if tag is None:
        return version, f"v{version}"
    described = release.stable_version_from_tag(tag)
    if described != version:
        raise ChocolateyAcceptanceError(
            f"--tag {tag!r} describes version {described!r}; expected version {version!r} "
            f"because {repository / CARGO_MANIFEST_NAME} declares it and the locally built "
            "binaries report it"
        )
    return version, tag


def require_single_identity_source(options: argparse.Namespace) -> None:
    """Refuse to mix a trusted preflight artifact with an operator-supplied identity."""

    conflicting = sorted(
        name
        for name, value in (
            ("--tag", options.tag),
            ("--commit", options.commit),
            ("--binary-directory-x64", options.binary_directory_x64),
            ("--binary-directory-x86", options.binary_directory_x86),
        )
        if value is not None
    )
    if conflicting:
        raise ChocolateyAcceptanceError(
            f"--inputs was passed with {conflicting}; expected --inputs to be the only source "
            "of the candidate's release identity and archive bytes"
        )


def resolve_inputs(
    channels: Any, options: argparse.Namespace, work: Path
) -> tuple[package_channels.PackageInputs, dict[str, LocalArchive]]:
    """Resolve candidate provenance from a real preflight result or from local builds."""

    archives: dict[str, LocalArchive] = {}
    if options.inputs is not None:
        require_single_identity_source(options)
        try:
            inputs = channels.PackageInputs.from_json(
                options.inputs.read_text(encoding="utf-8")
            )
        except channels.ChannelError as error:
            raise ChocolateyAcceptanceError(
                f"cannot trust the preflight inputs at {options.inputs}: {error}"
            ) from error
        for architecture in sorted(ARCHITECTURE_MACHINES):
            target = windows_target(architecture)
            identity = inputs.archive(target.triple)
            destination = work / "archives" / identity.name
            download_verified(identity.url, destination, expected_sha256=identity.sha256)
            archives[architecture] = LocalArchive(
                architecture=architecture,
                target=target,
                path=destination,
                sha256=identity.sha256,
                tag=inputs.tag,
            )
        return inputs, archives

    directories = {
        "x64": options.binary_directory_x64,
        "x86": options.binary_directory_x86,
    }
    missing = tuple(name for name, path in sorted(directories.items()) if path is None)
    if missing:
        raise ChocolateyAcceptanceError(
            "without --inputs every Windows architecture must supply already-built binaries; "
            f"missing {tuple(f'--binary-directory-{name}' for name in missing)!r}"
        )
    version, tag = local_release_identity(options.repository_path, tag=options.tag)
    commit = release.validate_commit(
        options.commit or release.git_output(options.repository_path, "rev-parse", "HEAD")
    )
    for architecture, directory in sorted(directories.items()):
        archives[architecture] = build_local_archive(
            repository=options.repository_path,
            binary_directory=directory,
            output_directory=work / "archives",
            architecture=architecture,
            version=version,
            tag=tag,
            commit=commit,
        )
    inputs = local_inputs(
        channels,
        repository=options.repository,
        tag=tag,
        commit=commit,
        archives=tuple(archives.values()),
    )
    return inputs, archives


def selected_packages(channels: Any, requested: Sequence[str]) -> tuple[PackageSelection, ...]:
    """Resolve the ordered package selections one run exercises."""

    known = {identity.package_id: identity for identity in channels.PACKAGES}
    unknown = tuple(name for name in requested if name not in known)
    if unknown:
        raise ChocolateyAcceptanceError(
            f"unknown package ids {unknown!r}; expected members of {tuple(known)!r}"
        )
    chosen = set(requested) if requested else set(known)
    return tuple(
        selection_for(identity)
        for identity in channels.PACKAGES
        if identity.package_id in chosen
    )


def requested_phases(options: argparse.Namespace) -> tuple[str, ...]:
    """Return the ordered phases one run must produce evidence for."""

    if not options.phase:
        return PHASES
    unknown = tuple(name for name in options.phase if name not in PHASES)
    if unknown:
        raise ChocolateyAcceptanceError(
            f"unknown phases {unknown!r}; expected members of {PHASES!r}"
        )
    return tuple(name for name in PHASES if name in set(options.phase))


def run_acceptance(options: argparse.Namespace, work: Path) -> dict[str, Any]:
    """Run every requested phase and return the JSON evidence document.

    The opt-in refusal is the first statement, before the Chocolatey CLI is located, so a host
    that did not consent is never touched.
    """

    require_opt_in(dict(os.environ))
    validate_scenario_map()
    channels = load_channels()
    gateway = ChocoGateway()
    package_ids = tuple(identity.package_id for identity in channels.PACKAGES)
    require_clean_host(gateway.list_local(), package_ids)
    environment = collect_environment(gateway)

    selections = selected_packages(channels, tuple(options.package))
    requested = requested_phases(options)
    narrowed = requested != PHASES or len(selections) != len(package_ids)

    inputs, archives = resolve_inputs(channels, options, work)
    sources = render_sources(
        channels,
        inputs,
        template_directory=options.template_directory,
        output_directory=work / "sources",
    )
    residue_targets = create_residue_targets(gateway, work / "residue")
    context = Context(
        gateway=gateway,
        channels=channels,
        inputs=inputs,
        identities={identity.package_id: identity for identity in channels.PACKAGES},
        repository_path=options.repository_path,
        template_directory=options.template_directory,
        work=work,
        sources=sources,
        nupkgs={},
        nupkg_digests={},
        candidates=work / "candidates",
        archives=archives,
        prior_version=options.prior_version,
        residue_targets=residue_targets,
        residue_before=residue_snapshot(
            residue_targets, path_values=collect_path_values(gateway, residue_targets)
        ),
        help_outputs={},
    )

    phases: list[PhaseResult] = []
    packed = [pack_phase(context, selection) for selection in selections]
    if "pack" in requested:
        phases.extend(packed)
    if "inspect" in requested and len(selections) == 2:
        phases.append(inspect_phase(context, selections))
    for selection in selections:
        phases.extend(
            run_install_session(context, selection, architecture="x64", requested=requested)
        )
        if "upgrade" in requested:
            phases.append(upgrade_phase(context, selection))
        phases.extend(
            run_install_session(context, selection, architecture="x86", requested=requested)
        )
    if len(selections) == 2:
        phases.extend(pair_phases(context, selections, requested=requested))
    for selection in selections:
        for phase in NEGATIVE_PHASES:
            if phase in requested:
                phases.append(negative_phase(context, selection, phase))
    if "residue" in requested:
        phases.append(residue_phase(context))

    coverage = coverage_findings(
        requested_phases=requested,
        requested_packages=tuple(selection.package_id for selection in selections),
        executed=tuple(phase.name for phase in phases),
        narrowed=narrowed,
    )
    return report_document(
        status=report_status(phases, coverage),
        environment=environment,
        provenance=inputs_summary(inputs),
        packages=tuple(selection.package_id for selection in selections),
        nupkg_digests=context.nupkg_digests,
        phases=phases,
        coverage=coverage,
        narrowed=narrowed,
    )


def argument_parser() -> argparse.ArgumentParser:
    """Build the Chocolatey acceptance harness command-line parser."""

    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--repository-path", type=Path, default=Path.cwd())
    parser.add_argument("--repository", default=DEFAULT_REPOSITORY)
    parser.add_argument(
        "--template-directory", type=Path, default=DEFAULT_TEMPLATE_DIRECTORY
    )
    parser.add_argument("--inputs", type=Path)
    parser.add_argument("--binary-directory-x64", type=Path)
    parser.add_argument("--binary-directory-x86", type=Path)
    parser.add_argument("--tag")
    parser.add_argument("--commit")
    parser.add_argument("--prior-version", default=PRIOR_VERSION)
    parser.add_argument("--package", action="append", default=[])
    parser.add_argument("--phase", action="append", default=[], choices=PHASES)
    parser.add_argument("--report", type=Path)
    parser.add_argument("--work-directory", type=Path)
    return parser


def run(arguments: Sequence[str]) -> int:
    """Run the harness in an owned working directory and report every failed assertion."""

    options = argument_parser().parse_args(arguments)
    require_opt_in(dict(os.environ))
    if options.work_directory is not None:
        options.work_directory.mkdir(parents=True, exist_ok=True)
        document = run_acceptance(options, options.work_directory.resolve())
    else:
        with tempfile.TemporaryDirectory(prefix="skillmount-choco-acceptance-") as temporary:
            document = run_acceptance(options, Path(temporary))
    if options.report is not None:
        write_report(options.report, document)
    print(json.dumps(document, sort_keys=True, indent=2))
    failures = failed_checks(document)
    if failures:
        print(
            f"chocolatey acceptance failed {len(failures)} checks: {failures}",
            file=sys.stderr,
        )
        return 1
    return 0


def main(arguments: Sequence[str] | None = None) -> int:
    """Convert unproved Chocolatey observations into a stable nonzero status."""

    try:
        return run(sys.argv[1:] if arguments is None else arguments)
    except (
        ChocolateyAcceptanceError,
        OSError,
        UnicodeError,
        json.JSONDecodeError,
        release.ReleaseError,
        subprocess.SubprocessError,
        zipfile.BadZipFile,
    ) as error:
        print(f"chocolatey acceptance failed: {error}", file=sys.stderr)
        return 1


if __name__ == "__main__":
    raise SystemExit(main())
