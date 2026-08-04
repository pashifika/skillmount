#!/usr/bin/env python3
"""Model, verify, and render the paired SkillMount Homebrew and Chocolatey candidates."""

from __future__ import annotations

import argparse
import json
import os
import re
import shutil
import subprocess
import sys
import zipfile
from dataclasses import dataclass
from pathlib import Path, PurePosixPath
from typing import Any, Mapping, Protocol, Sequence
from urllib.parse import quote

import release

PRODUCT_NAME = "SkillMount"
DEFAULT_REPOSITORY = "pashifika/skillmount"
HOMEPAGE = "https://github.com/pashifika/skillmount"
LICENSE_EXPRESSION = "MIT OR Apache-2.0"
HOMEBREW_LICENSE_EXPRESSION = 'any_of: ["MIT", "Apache-2.0"]'
INPUTS_SCHEMA = 2
RELEASE_WORKFLOW_NAME = "Release"
RELEASE_WORKFLOW_PATH = ".github/workflows/release.yml"
TEMPLATE_SUFFIX = ".in"
TOKEN_PATTERN = re.compile(r"@([A-Z][A-Z0-9_]*)@")

API_VERSION = "2026-03-10"
CARGO_PACKAGE_NAME = "skillmount"
REPOSITORY_ROOT = Path(__file__).resolve().parents[2]
REPOSITORY_PATTERN = re.compile(r"[A-Za-z0-9][A-Za-z0-9._-]*/[A-Za-z0-9][A-Za-z0-9._-]*")
DIGEST_PATTERN = re.compile(r"[0-9a-f]{64}")
DIGEST_PREFIX = "sha256:"
MACOS_ARM64_TARGET_NAME = "macos-arm64"
WINDOWS_X64_TARGET_NAME = "windows-x64"
WINDOWS_X86_TARGET_NAME = "windows-x86"
APACHE_LICENSE_FILE, MIT_LICENSE_FILE = release.LICENSE_FILES

TOOLS_DIRECTORY = "tools"
NUSPEC_SUFFIX = ".nuspec"
NUPKG_SUFFIX = ".nupkg"
FORMULA_SUFFIX = ".rb"
INSTALL_SCRIPT = f"{TOOLS_DIRECTORY}/chocolateyinstall.ps1"
UNINSTALL_SCRIPT = f"{TOOLS_DIRECTORY}/chocolateyuninstall.ps1"
VERIFICATION_FILE = f"{TOOLS_DIRECTORY}/VERIFICATION.txt"
CONTENT_TYPES_MEMBER = "[Content_Types].xml"
RELS_MEMBER = "_rels/.rels"
PSMDCP_PATTERN = re.compile(r"package/services/metadata/core-properties/[0-9A-Za-z]+\.psmdcp")
FORBIDDEN_MEMBER_SUFFIXES = (".exe", ".dll", ".zip", ".tar.gz", ".tgz", ".7z", ".msi")

FORMULA_CLASS_PATTERN = re.compile(r"^class ([A-Za-z0-9_]+) < Formula$", re.MULTILINE)
FORMULA_SCALAR_PATTERNS = {
    "desc": re.compile(r'^\s*desc "([^"]*)"\s*$', re.MULTILINE),
    "homepage": re.compile(r'^\s*homepage "([^"]*)"\s*$', re.MULTILINE),
    "url": re.compile(r'^\s*url "([^"]*)"\s*$', re.MULTILINE),
    "sha256": re.compile(r'^\s*sha256 "([^"]*)"\s*$', re.MULTILINE),
    "license": re.compile(r"^\s*license (.+?)\s*$", re.MULTILINE),
}
FORMULA_SHARED_FACTS = ("dependencies", "homepage", "license", "sha256", "url")
FORMULA_DEPENDENCIES = tuple(sorted(("depends_on :macos", "depends_on arch: :arm64")))
DEPENDS_ON_PATTERN = re.compile(r"^\s*(depends_on .+?)\s*$", re.MULTILINE)
BIN_INSTALL_PATTERN = re.compile(r'^\s*bin\.install "([^"]+)"\s*$', re.MULTILINE)
PKGSHARE_INSTALL_PATTERN = re.compile(r"^\s*pkgshare\.install (.+?)\s*$", re.MULTILINE)
TEST_BLOCK_PATTERN = re.compile(r"^\s*test do\s*$", re.MULTILINE)
ERROR_PREFERENCE_PATTERN = re.compile(r"\$ErrorActionPreference\s*=\s*['\"]Stop['\"]")
STRICT_MODE_PATTERN = re.compile(r"Set-StrictMode\s+-Version\s+2")
FORMULA_SELECTION_ALIASES = {
    "PACKAGE_ID": "SELECTION",
    "COMMAND": "SELECTION",
    "FORMULA_CLASS": "SELECTION_CLASS",
    "OTHER_COMMAND": "OTHER",
}
CHOCOLATEY_SELECTION_ALIASES = {"PACKAGE_ID": "SELECTION", "COMMAND": "SELECTION"}
FORBIDDEN_INSTALL_MARKERS = {
    "Install-ChocolateyPath": "a permanent PATH edit",
    "Install-ChocolateyZipPackage": "an extraction into the shim-discovered tools directory",
    "$PROFILE": "a PowerShell profile edit",
    ".ignore": "an ignore marker instead of removing the unselected executable",
}


class ChannelError(RuntimeError):
    """A package-channel invariant was not satisfied."""


def target_named(name: str) -> release.Target:
    """Return the one release target registered under a build-matrix name."""

    matches = [target for target in release.TARGETS if target.name == name]
    if len(matches) != 1:
        known = ", ".join(target.name for target in release.TARGETS)
        raise ChannelError(
            f"release target {name!r} is not uniquely defined; known targets are {known}"
        )
    return matches[0]


MACOS_ARM64 = target_named(MACOS_ARM64_TARGET_NAME)
WINDOWS_X64 = target_named(WINDOWS_X64_TARGET_NAME)
WINDOWS_X86 = target_named(WINDOWS_X86_TARGET_NAME)


@dataclass(frozen=True)
class PackageIdentity:
    """One publishable package identity and the product executable it selects."""

    package_id: str
    command: str
    formula_class: str
    title: str
    summary: str
    description: str

    @property
    def formula_path(self) -> str:
        """Return the tap-relative Formula path this package owns."""

        return f"Formula/{self.package_id}{FORMULA_SUFFIX}"

    @property
    def windows_executable(self) -> str:
        """Return the single Windows executable this package retains."""

        return f"{self.command}{WINDOWS_X64.executable_suffix}"

    @property
    def other(self) -> "PackageIdentity":
        """Return the pair member selecting the other product executable."""

        others = [package for package in PACKAGES if package.package_id != self.package_id]
        if len(others) != 1:
            raise ChannelError(
                f"package {self.package_id!r} has {len(others)} pair members; expected exactly one"
            )
        return others[0]


PACKAGES: tuple[PackageIdentity, PackageIdentity] = (
    PackageIdentity(
        package_id="skillmount",
        command="skillmount",
        formula_class="Skillmount",
        title="SkillMount",
        summary="Portable skill mounting for coding agents, installing the skillmount command.",
        description="Portable skill mounting for coding agents (skillmount command)",
    ),
    PackageIdentity(
        package_id="skillmount-asm",
        command="asm",
        formula_class="SkillmountAsm",
        title="SkillMount (asm)",
        summary="Portable skill mounting for coding agents, installing the asm command.",
        description="Portable skill mounting for coding agents (asm command)",
    ),
)
PACKAGE_BY_ID = {package.package_id: package for package in PACKAGES}


def package_for(package_id: str) -> PackageIdentity:
    """Return the package identity published under *package_id*."""

    try:
        return PACKAGE_BY_ID[package_id]
    except KeyError as error:
        known = ", ".join(PACKAGE_BY_ID)
        raise ChannelError(
            f"unsupported package id {package_id!r}; expected one of {known}"
        ) from error


def require_release_value(operation: Any, *arguments: Any) -> Any:
    """Apply one release helper and report its failure as a package-channel error."""

    try:
        return operation(*arguments)
    except release.ReleaseError as error:
        raise ChannelError(str(error)) from error


def validate_repository(repository: str) -> str:
    """Validate an `owner/name` GitHub repository identity."""

    if not isinstance(repository, str) or REPOSITORY_PATTERN.fullmatch(repository) is None:
        raise ChannelError(f"repository is {repository!r}; expected an owner/name identity")
    return repository


def validate_digest(value: str, label: str) -> str:
    """Validate one lowercase hexadecimal SHA-256 digest."""

    if not isinstance(value, str) or DIGEST_PATTERN.fullmatch(value) is None:
        raise ChannelError(f"{label} is {value!r}; expected 64 lowercase hexadecimal characters")
    return value


def validate_url(value: str, label: str) -> str:
    """Validate one non-empty whitespace-free URL value."""

    if not isinstance(value, str) or value.split() != [value]:
        raise ChannelError(f"{label} is {value!r}; expected one non-empty whitespace-free URL")
    return value


def require_stable_tag(tag: Any, label: str) -> str:
    """Require an exact stable release tag rather than a branch, commit, or prerelease."""

    if not isinstance(tag, str) or release.TAG_PATTERN.fullmatch(tag) is None:
        raise ChannelError(
            f"{label} is {tag!r}; expected an exact stable release tag such as v0.2.0"
        )
    return tag


def asset_download_url(repository: str, tag: str, name: str) -> str:
    """Immutable GitHub Release asset download URL for one asset name."""

    validate_repository(repository)
    require_stable_tag(tag, "release tag")
    if not name or name.split() != [name] or "/" in name:
        raise ChannelError(f"release asset name is {name!r}; expected one path-free file name")
    return f"https://github.com/{repository}/releases/download/{tag}/{name}"


def license_url(repository: str, tag: str) -> str:
    """Permanent MIT license URL pinned to one exact tag."""

    validate_repository(repository)
    require_stable_tag(tag, "license tag")
    return f"https://github.com/{repository}/blob/{tag}/{MIT_LICENSE_FILE}"


@dataclass(frozen=True)
class ArchiveIdentity:
    """One immutable release archive a package installer may download."""

    triple: str
    name: str
    url: str
    sha256: str


@dataclass(frozen=True)
class PackageInputs:
    """The validated release identity every package candidate is generated from."""

    repository: str
    version: str
    tag: str
    commit: str
    release_url: str
    archives: tuple[ArchiveIdentity, ...]

    def __post_init__(self) -> None:
        """Validate the structure of every recorded value.

        Completeness of the archive set and the `https://github.com/<repository>/` URL policy
        are enforced by `from_json` and `preflight`, so a native acceptance harness may build
        a partial local release-archive identity in process while a downstream artifact cannot.
        """

        validate_repository(self.repository)
        require_release_value(release.validate_stable_version, self.version)
        tag_version = require_release_value(release.stable_version_from_tag, self.tag)
        if tag_version != self.version:
            raise ChannelError(
                f"tag {self.tag!r} yields version {tag_version!r}; expected {self.version!r}"
            )
        require_release_value(release.validate_commit, self.commit)
        validate_url(self.release_url, "release URL")
        triples = [archive.triple for archive in self.archives]
        if triples != sorted(set(triples)):
            raise ChannelError(
                f"archive triples are {triples!r}; expected unique values sorted by triple"
            )
        for archive in self.archives:
            target = require_release_value(release.target_for, archive.triple)
            expected_name = require_release_value(release.asset_name, self.tag, target)
            if archive.name != expected_name:
                raise ChannelError(
                    f"archive for {archive.triple} is named {archive.name!r}; "
                    f"expected {expected_name!r}"
                )
            validate_url(archive.url, f"archive {archive.name} URL")
            validate_digest(archive.sha256, f"archive {archive.name} digest")

    def archive(self, triple: str) -> ArchiveIdentity:
        """Return the recorded archive identity for one target triple."""

        for archive in self.archives:
            if archive.triple == triple:
                return archive
        recorded = ", ".join(item.triple for item in self.archives) or "none"
        raise ChannelError(
            f"no release archive is recorded for {triple!r}; recorded targets are {recorded}"
        )

    def to_json(self) -> str:
        """Return the deterministic artifact downstream jobs consume."""

        document = {
            "schema": INPUTS_SCHEMA,
            "repository": self.repository,
            "version": self.version,
            "tag": self.tag,
            "commit": self.commit,
            "release_url": self.release_url,
            "archives": [
                {
                    "triple": archive.triple,
                    "name": archive.name,
                    "url": archive.url,
                    "sha256": archive.sha256,
                }
                for archive in self.archives
            ],
        }
        return json.dumps(document, indent=2, sort_keys=True) + "\n"

    @classmethod
    def from_json(cls, text: str) -> "PackageInputs":
        """Rebuild inputs from an untrusted artifact, revalidating every field."""

        try:
            document = json.loads(text)
        except json.JSONDecodeError as error:
            raise ChannelError(f"package inputs are not valid JSON: {error}") from error
        if not isinstance(document, dict):
            raise ChannelError(
                f"package inputs are a {type(document).__name__}; expected a JSON object"
            )
        schema = document.get("schema")
        if schema != INPUTS_SCHEMA:
            raise ChannelError(f"package inputs schema is {schema!r}; expected {INPUTS_SCHEMA}")
        scalar_names = ("repository", "version", "tag", "commit", "release_url")
        expected_keys = {"schema", "archives", *scalar_names}
        if set(document) != expected_keys:
            raise ChannelError(
                f"package inputs declare keys {sorted(document)!r}; "
                f"expected {sorted(expected_keys)!r}"
            )
        scalars: dict[str, str] = {}
        for name in scalar_names:
            value = document[name]
            if not isinstance(value, str):
                raise ChannelError(
                    f"package inputs field {name!r} is {value!r}; expected a string"
                )
            scalars[name] = value
        entries = document["archives"]
        if not isinstance(entries, list):
            raise ChannelError(f"package inputs archives are {entries!r}; expected a JSON array")
        archives: list[ArchiveIdentity] = []
        for entry in entries:
            if not isinstance(entry, dict) or set(entry) != {"triple", "name", "url", "sha256"}:
                raise ChannelError(
                    f"archive entry {entry!r} must declare exactly triple, name, url, and sha256"
                )
            for key, value in sorted(entry.items()):
                if not isinstance(value, str):
                    raise ChannelError(f"archive field {key!r} is {value!r}; expected a string")
            archives.append(
                ArchiveIdentity(
                    triple=entry["triple"],
                    name=entry["name"],
                    url=entry["url"],
                    sha256=entry["sha256"],
                )
            )
        inputs = cls(
            repository=scalars["repository"],
            version=scalars["version"],
            tag=scalars["tag"],
            commit=scalars["commit"],
            release_url=scalars["release_url"],
            archives=tuple(archives),
        )
        expected_triples = tuple(sorted(target.triple for target in release.TARGETS))
        observed_triples = tuple(archive.triple for archive in inputs.archives)
        if observed_triples != expected_triples:
            raise ChannelError(
                f"package inputs cover archives {observed_triples!r}; "
                f"expected {expected_triples!r}"
            )
        release_prefix = f"https://github.com/{inputs.repository}/releases/"
        if not inputs.release_url.startswith(release_prefix):
            raise ChannelError(
                f"release URL is {inputs.release_url!r}; expected a URL under {release_prefix!r}"
            )
        for archive in inputs.archives:
            expected_url = asset_download_url(inputs.repository, inputs.tag, archive.name)
            if archive.url != expected_url:
                raise ChannelError(
                    f"archive {archive.name} URL is {archive.url!r}; expected {expected_url!r}"
                )
        return inputs


class ReleaseGateway(Protocol):
    """Read-only GitHub boundary preflight observes release state through."""

    def workflow_run(self, repository: str, run_id: int) -> dict[str, Any]:
        """Return the triggering workflow-run payload."""

    def dereference_tag(self, repository: str, tag: str) -> str:
        """Return the commit `refs/tags/<tag>` resolves to."""

    def release_for_tag(self, repository: str, tag: str) -> dict[str, Any]:
        """Return the published release payload for an exact tag."""

    def commit_contained_in_default_branch(self, repository: str, commit: str) -> bool:
        """Return whether the default branch contains *commit*."""

    def file_at_commit(self, repository: str, commit: str, path: str) -> bytes:
        """Return one repository file at an exact commit as untrusted data."""

    def download(self, url: str, destination: Path) -> None:
        """Stream one release artifact to an isolated destination."""


class GhReleaseGateway:
    """Small shell-free adapter around the authenticated GitHub CLI."""

    def __init__(self, working_directory: Path) -> None:
        """Bind read-only GitHub CLI calls to a repository checkout."""

        self.working_directory = working_directory.resolve()
        if not os.environ.get("GH_TOKEN"):
            raise ChannelError("GH_TOKEN is required to observe release state")
        self._default_branches: dict[str, str] = {}

    def _run(self, arguments: Sequence[str], *, output_file: Any | None = None) -> bytes:
        completed = subprocess.run(
            arguments,
            cwd=self.working_directory,
            stdout=output_file if output_file is not None else subprocess.PIPE,
            stderr=subprocess.PIPE,
            check=False,
        )
        if completed.returncode != 0:
            stderr = completed.stderr.decode(errors="replace").strip()
            command = " ".join(arguments[:3])
            raise ChannelError(f"{command} failed with status {completed.returncode}: {stderr}")
        return b"" if output_file is not None else completed.stdout

    def _raw(self, endpoint: str, *, accept: str) -> bytes:
        return self._run(
            (
                "gh",
                "api",
                endpoint,
                "--header",
                f"Accept: {accept}",
                "--header",
                f"X-GitHub-Api-Version: {API_VERSION}",
            )
        )

    def _api(self, endpoint: str) -> Any:
        output = self._raw(endpoint, accept="application/vnd.github+json")
        if not output:
            return None
        try:
            return json.loads(output)
        except (UnicodeDecodeError, json.JSONDecodeError) as error:
            raise ChannelError(f"GitHub returned invalid JSON for {endpoint}") from error

    def workflow_run(self, repository: str, run_id: int) -> dict[str, Any]:
        """Return the triggering workflow-run payload."""

        value = self._api(f"repos/{repository}/actions/runs/{int(run_id)}")
        if not isinstance(value, dict):
            raise ChannelError(f"GitHub did not return workflow run {run_id} of {repository}")
        return value

    def dereference_tag(self, repository: str, tag: str) -> str:
        """Peel a lightweight or annotated tag reference to exactly one commit."""

        reference = self._api(f"repos/{repository}/git/ref/tags/{quote(tag, safe='')}")
        if not isinstance(reference, dict) or not isinstance(reference.get("object"), dict):
            raise ChannelError(f"GitHub did not return tag object metadata for {tag}")
        target = reference["object"]
        for _ in range(8):
            object_type = target.get("type")
            object_sha = target.get("sha")
            if not isinstance(object_sha, str):
                raise ChannelError(f"tag {tag} object declares no SHA")
            if object_type == "commit":
                return require_release_value(release.validate_commit, object_sha)
            if object_type != "tag":
                raise ChannelError(
                    f"tag {tag} points at object type {object_type!r}; expected 'commit' or 'tag'"
                )
            annotated = self._api(f"repos/{repository}/git/tags/{object_sha}")
            if not isinstance(annotated, dict) or not isinstance(annotated.get("object"), dict):
                raise ChannelError(f"annotated tag object {object_sha} is malformed")
            target = annotated["object"]
        raise ChannelError(f"tag {tag} exceeds the bounded annotated-tag chain")

    def release_for_tag(self, repository: str, tag: str) -> dict[str, Any]:
        """Return the published release payload for an exact tag."""

        value = self._api(f"repos/{repository}/releases/tags/{quote(tag, safe='')}")
        if not isinstance(value, dict):
            raise ChannelError(f"GitHub did not return a release for {tag} of {repository}")
        return value

    def default_branch(self, repository: str) -> str:
        """Return the repository's current default branch name."""

        cached = self._default_branches.get(repository)
        if cached is not None:
            return cached
        value = self._api(f"repos/{repository}")
        branch = value.get("default_branch") if isinstance(value, dict) else None
        if not isinstance(branch, str) or not branch:
            raise ChannelError(f"GitHub did not report a default branch for {repository}")
        self._default_branches[repository] = branch
        return branch

    def commit_contained_in_default_branch(self, repository: str, commit: str) -> bool:
        """Return whether the default branch contains *commit*."""

        branch = self.default_branch(repository)
        comparison = self._api(f"repos/{repository}/compare/{quote(branch, safe='')}...{commit}")
        status = comparison.get("status") if isinstance(comparison, dict) else None
        if not isinstance(status, str):
            raise ChannelError(
                f"GitHub did not report a comparison status for {branch}...{commit}"
            )
        return status in ("identical", "behind")

    def file_at_commit(self, repository: str, commit: str, path: str) -> bytes:
        """Return one repository file at an exact commit as untrusted data."""

        encoded = quote(path, safe="/")
        return self._raw(
            f"repos/{repository}/contents/{encoded}?ref={commit}",
            accept="application/vnd.github.raw",
        )

    def download(self, url: str, destination: Path) -> None:
        """Stream one release artifact to an isolated destination."""

        prefix = "https://github.com/"
        if not url.startswith(prefix):
            raise ChannelError(f"refusing to download {url!r}; expected a URL under {prefix!r}")
        destination.parent.mkdir(parents=True, exist_ok=True)
        with destination.open("xb") as output:
            self._run(
                (
                    "gh",
                    "api",
                    url,
                    "--header",
                    "Accept: application/octet-stream",
                    "--header",
                    f"X-GitHub-Api-Version: {API_VERSION}",
                ),
                output_file=output,
            )


@dataclass(frozen=True)
class TriggerDecision:
    """The single tag and mode one observed workflow trigger authorizes."""

    tag: str
    verification_only: bool
    reason: str


def evaluate_trigger(
    *,
    event_name: str,
    repository: str,
    workflow_run: dict[str, Any] | None,
    dispatch_tag: str | None,
    dispatch_verification_only: bool,
) -> TriggerDecision:
    """Authorize exactly one tag and publication mode from an untrusted trigger."""

    validate_repository(repository)
    if event_name == "workflow_run":
        if not isinstance(workflow_run, Mapping):
            raise ChannelError(
                f"workflow_run payload is {workflow_run!r}; expected the run object"
            )
        expectations = (
            ("name", RELEASE_WORKFLOW_NAME),
            ("path", RELEASE_WORKFLOW_PATH),
            ("event", "push"),
            ("status", "completed"),
            ("conclusion", "success"),
        )
        for field, expected in expectations:
            observed = workflow_run.get(field)
            if observed != expected:
                raise ChannelError(
                    f"triggering run {field} is {observed!r}; expected {expected!r}"
                )
        head_repository = workflow_run.get("head_repository")
        full_name = (
            head_repository.get("full_name") if isinstance(head_repository, Mapping) else None
        )
        if full_name != repository:
            raise ChannelError(
                f"triggering run head repository is {full_name!r}; expected {repository!r}"
            )
        tag = require_stable_tag(workflow_run.get("head_branch"), "triggering run head_branch")
        return TriggerDecision(
            tag=tag,
            verification_only=False,
            reason=f"successful {RELEASE_WORKFLOW_NAME} run {workflow_run.get('id')!r} for {tag}",
        )
    if event_name == "workflow_dispatch":
        tag = require_stable_tag(dispatch_tag, "dispatch tag")
        verification_only = bool(dispatch_verification_only)
        return TriggerDecision(
            tag=tag,
            verification_only=verification_only,
            reason=(
                f"manual dispatch for {tag} with verification_only="
                f"{str(verification_only).lower()}"
            ),
        )
    raise ChannelError(
        f"event {event_name!r} cannot trigger package publication; expected "
        f"'workflow_run' or 'workflow_dispatch'"
    )


def decode_text(data: Any, label: str) -> str:
    """Decode one externally supplied file read as untrusted data."""

    if not isinstance(data, bytes):
        raise ChannelError(f"{label} was read as {type(data).__name__}; expected bytes")
    try:
        return data.decode("utf-8")
    except UnicodeDecodeError as error:
        raise ChannelError(f"{label} is not valid UTF-8: {error}") from error


def cargo_manifest_version(text: str) -> str:
    """Return the `[package]` version a Cargo manifest declares."""

    table: str | None = None
    versions: list[str] = []
    for line in text.splitlines():
        stripped = line.strip()
        header = re.fullmatch(r"\[\[?([^\[\]]+)\]\]?", stripped)
        if header is not None:
            table = header.group(1)
            continue
        if table != "package":
            continue
        match = re.fullmatch(r'version = "([^"]+)"', stripped)
        if match is not None:
            versions.append(match.group(1))
    if len(versions) != 1:
        raise ChannelError(
            f"Cargo.toml declares {len(versions)} [package] version entries; expected exactly one"
        )
    return versions[0]


def cargo_lock_version(text: str, package_name: str) -> str:
    """Return the version a Cargo lockfile records for one package."""

    versions: list[str] = []
    header = ""
    fields: dict[str, str] = {}
    for line in (*text.splitlines(), "[end]"):
        stripped = line.strip()
        if stripped.startswith("["):
            if header == "[[package]]" and fields.get("name") == package_name:
                locked = fields.get("version")
                if locked is None:
                    raise ChannelError(
                        f"Cargo.lock entry for {package_name!r} declares no version"
                    )
                versions.append(locked)
            header = stripped
            fields = {}
            continue
        match = re.fullmatch(r'(name|version) = "([^"]+)"', stripped)
        if match is not None:
            fields[match.group(1)] = match.group(2)
    if len(versions) != 1:
        raise ChannelError(
            f"Cargo.lock records {len(versions)} entries for package {package_name!r}; "
            "expected exactly one"
        )
    return versions[0]


def verify_cargo_metadata(
    gateway: ReleaseGateway, *, repository: str, commit: str, version: str
) -> None:
    """Prove the tagged commit declares and locks exactly the released version."""

    manifest = decode_text(gateway.file_at_commit(repository, commit, "Cargo.toml"), "Cargo.toml")
    declared = cargo_manifest_version(manifest)
    if declared != version:
        raise ChannelError(
            f"Cargo.toml at {commit} declares version {declared!r}; expected {version!r}"
        )
    lock = decode_text(gateway.file_at_commit(repository, commit, "Cargo.lock"), "Cargo.lock")
    locked = cargo_lock_version(lock, CARGO_PACKAGE_NAME)
    if locked != version:
        raise ChannelError(
            f"Cargo.lock at {commit} locks {CARGO_PACKAGE_NAME} {locked!r}; expected {version!r}"
        )


def require_mapping(value: Any, label: str) -> Mapping[str, Any]:
    """Require one JSON object at an untrusted boundary."""

    if not isinstance(value, Mapping):
        raise ChannelError(f"{label} is {value!r}; expected an object")
    return value


def reported_asset_digest(asset: Mapping[str, Any]) -> str | None:
    """Return the SHA-256 GitHub reports for one asset, when it reports one."""

    digest = asset.get("digest")
    name = asset.get("name")
    if digest is None:
        return None
    if not isinstance(digest, str) or not digest.startswith(DIGEST_PREFIX):
        raise ChannelError(
            f"release asset {name!r} digest is {digest!r}; expected a {DIGEST_PREFIX!r} value"
        )
    return validate_digest(
        digest[len(DIGEST_PREFIX) :], f"release asset {name!r} reported digest"
    )


def release_html_url(payload: Mapping[str, Any], *, repository: str, tag: str) -> str:
    """Return the release's own permanent URL after validating its origin."""

    url = payload.get("html_url")
    prefix = f"https://github.com/{repository}/releases/"
    if not isinstance(url, str) or not url.startswith(prefix):
        raise ChannelError(f"release {tag} reports URL {url!r}; expected a URL under {prefix!r}")
    return validate_url(url, f"release {tag} URL")


def verify_release_identity(
    payload: Mapping[str, Any], *, repository: str, tag: str, commit: str
) -> dict[str, Mapping[str, Any]]:
    """Require a published, non-prerelease release with exactly the expected assets."""

    for field in ("draft", "prerelease"):
        observed = payload.get(field)
        if observed is not False:
            raise ChannelError(f"release {tag} reports {field}={observed!r}; expected False")
    observed_tag = payload.get("tag_name")
    if observed_tag != tag:
        raise ChannelError(f"release names tag {observed_tag!r}; expected {tag!r}")
    commitish = payload.get("target_commitish")
    if not isinstance(commitish, str) or not commitish:
        raise ChannelError(
            f"release {tag} target_commitish is {commitish!r}; expected a commit or branch name"
        )
    if release.COMMIT_PATTERN.fullmatch(commitish) is not None and commitish != commit:
        raise ChannelError(f"release {tag} targets commit {commitish}; expected {commit}")
    assets = payload.get("assets")
    if not isinstance(assets, list):
        raise ChannelError(f"release {tag} assets are {assets!r}; expected a JSON array")
    indexed: dict[str, Mapping[str, Any]] = {}
    for asset in assets:
        entry = require_mapping(asset, f"release asset of {tag}")
        name = entry.get("name")
        if not isinstance(name, str) or not name:
            raise ChannelError(f"release {tag} declares an asset named {name!r}")
        if name in indexed:
            raise ChannelError(f"release {tag} declares duplicate asset {name!r}")
        indexed[name] = entry
    expected = set(release.expected_archive_names(tag)) | {release.CHECKSUM_FILE}
    if set(indexed) != expected:
        missing = sorted(expected - set(indexed))
        unexpected = sorted(set(indexed) - expected)
        raise ChannelError(
            f"release {tag} publishes assets {sorted(indexed)!r}; expected {sorted(expected)!r} "
            f"(missing {missing!r}, unexpected {unexpected!r})"
        )
    for name in sorted(indexed):
        entry = indexed[name]
        state = entry.get("state")
        if state is not None and state != "uploaded":
            raise ChannelError(
                f"release asset {name} reports state {state!r}; expected 'uploaded'"
            )
        url = entry.get("browser_download_url")
        expected_url = asset_download_url(repository, tag, name)
        if url != expected_url:
            raise ChannelError(
                f"release asset {name} reports URL {url!r}; expected {expected_url!r}"
            )
    return indexed


def verify_release_assets(
    gateway: ReleaseGateway,
    assets: Mapping[str, Mapping[str, Any]],
    *,
    tag: str,
    version: str,
    commit: str,
    work_directory: Path,
) -> dict[str, str]:
    """Download, checksum, and structurally inspect the complete release asset set."""

    local: dict[str, Path] = {}
    for name in sorted(assets):
        destination = work_directory / name
        gateway.download(str(assets[name]["browser_download_url"]), destination)
        local[name] = destination
    digests = {name: release.sha256_file(path) for name, path in sorted(local.items())}
    for name, digest in sorted(digests.items()):
        reported = reported_asset_digest(assets[name])
        if reported is not None and reported != digest:
            raise ChannelError(
                f"release asset {name} reports digest {reported}; "
                f"downloaded bytes hash to {digest}"
            )
    checksums = release.parse_checksum_file(local[release.CHECKSUM_FILE])
    expected_names = release.expected_archive_names(tag)
    if tuple(sorted(checksums)) != expected_names:
        raise ChannelError(
            f"{release.CHECKSUM_FILE} covers {sorted(checksums)!r}; "
            f"expected {list(expected_names)!r}"
        )
    for name, expected_digest in sorted(checksums.items()):
        if digests[name] != expected_digest:
            raise ChannelError(
                f"{release.CHECKSUM_FILE} records {expected_digest} for {name}; "
                f"downloaded bytes hash to {digests[name]}"
            )
    for target in release.TARGETS:
        release.inspect_archive(
            local[release.asset_name(tag, target)],
            target=target,
            version=version,
            tag=tag,
            commit=commit,
        )
    return {name: digest for name, digest in sorted(digests.items()) if name in checksums}


def preflight(
    gateway: ReleaseGateway,
    *,
    repository: str,
    tag: str,
    work_directory: Path,
) -> PackageInputs:
    """Prove tag, ancestry, Cargo, release, checksum, and archive layout from observed data."""

    validate_repository(repository)
    version = require_release_value(release.stable_version_from_tag, tag)
    commit = require_release_value(
        release.validate_commit, gateway.dereference_tag(repository, tag)
    )
    if not gateway.commit_contained_in_default_branch(repository, commit):
        raise ChannelError(
            f"commit {commit} for tag {tag} is not contained in the default branch of {repository}"
        )
    verify_cargo_metadata(gateway, repository=repository, commit=commit, version=version)
    payload = require_mapping(gateway.release_for_tag(repository, tag), f"release for {tag}")
    assets = verify_release_identity(payload, repository=repository, tag=tag, commit=commit)
    work_directory.mkdir(parents=True, exist_ok=True)
    digests = verify_release_assets(
        gateway,
        assets,
        tag=tag,
        version=version,
        commit=commit,
        work_directory=work_directory,
    )
    archives = tuple(
        ArchiveIdentity(
            triple=target.triple,
            name=release.asset_name(tag, target),
            url=asset_download_url(repository, tag, release.asset_name(tag, target)),
            sha256=digests[release.asset_name(tag, target)],
        )
        for target in sorted(release.TARGETS, key=lambda item: item.triple)
    )
    return PackageInputs(
        repository=repository,
        version=version,
        tag=tag,
        commit=commit,
        release_url=release_html_url(payload, repository=repository, tag=tag),
        archives=archives,
    )


def render_template(text: str, values: Mapping[str, str]) -> str:
    """Substitute every `@TOKEN@` and treat any template or value drift as a failure."""

    tokens = set(TOKEN_PATTERN.findall(text))
    provided = set(values)
    unknown = sorted(tokens - provided)
    if unknown:
        raise ChannelError(
            f"template uses undefined tokens {unknown!r}; defined tokens are {sorted(provided)!r}"
        )
    unused = sorted(provided - tokens)
    if unused:
        raise ChannelError(f"template omits required tokens {unused!r}; it uses {sorted(tokens)!r}")
    for name in sorted(tokens):
        value = values[name]
        if not isinstance(value, str) or not value:
            raise ChannelError(f"token {name!r} value is {value!r}; expected a non-empty string")
        if "\n" in value or "\r" in value:
            raise ChannelError(f"token {name!r} value {value!r} unexpectedly contains a newline")
    return TOKEN_PATTERN.sub(lambda match: values[match.group(1)], text)


def formula_tokens(inputs: PackageInputs, identity: PackageIdentity) -> dict[str, str]:
    """Return the exact token set the Homebrew Formula template requires."""

    archive = inputs.archive(MACOS_ARM64.triple)
    return {
        "FORMULA_CLASS": identity.formula_class,
        "PACKAGE_ID": identity.package_id,
        "DESCRIPTION": identity.description,
        "HOMEPAGE": HOMEPAGE,
        "ARCHIVE_URL": archive.url,
        "ARCHIVE_SHA256": archive.sha256,
        "VERSION": inputs.version,
        "LICENSE": HOMEBREW_LICENSE_EXPRESSION,
        "COMMAND": identity.command,
        "OTHER_COMMAND": identity.other.command,
        "TAG": inputs.tag,
        "COMMIT": inputs.commit,
    }


def nuspec_tokens(inputs: PackageInputs, identity: PackageIdentity) -> dict[str, str]:
    """Return the exact token set the Chocolatey nuspec template requires."""

    return {
        "PACKAGE_ID": identity.package_id,
        "VERSION": inputs.version,
        "TITLE": identity.title,
        "SUMMARY": identity.summary,
        "DESCRIPTION": identity.description,
        "PROJECT_URL": HOMEPAGE,
        "PROJECT_SOURCE_URL": HOMEPAGE,
        "LICENSE_URL": license_url(inputs.repository, inputs.tag),
        "RELEASE_NOTES_URL": inputs.release_url,
        "COMMAND": identity.command,
        "TAG": inputs.tag,
    }


def install_script_tokens(inputs: PackageInputs, identity: PackageIdentity) -> dict[str, str]:
    """Return the exact token set the Chocolatey install script requires."""

    x86 = inputs.archive(WINDOWS_X86.triple)
    x64 = inputs.archive(WINDOWS_X64.triple)
    return {
        "PACKAGE_ID": identity.package_id,
        "VERSION": inputs.version,
        "TAG": inputs.tag,
        "COMMAND": identity.command,
        "SELECTED_EXECUTABLE": identity.windows_executable,
        "OTHER_EXECUTABLE": identity.other.windows_executable,
        "URL_X86": x86.url,
        "SHA256_X86": x86.sha256,
        "URL_X64": x64.url,
        "SHA256_X64": x64.sha256,
        "ARCHIVE_ROOT_X86": require_release_value(release.asset_stem, inputs.tag, WINDOWS_X86),
        "ARCHIVE_ROOT_X64": require_release_value(release.asset_stem, inputs.tag, WINDOWS_X64),
    }


def uninstall_script_tokens(inputs: PackageInputs, identity: PackageIdentity) -> dict[str, str]:
    """Return the exact token set the optional Chocolatey uninstall script requires."""

    require_release_value(release.validate_stable_version, inputs.version)
    return {
        "PACKAGE_ID": identity.package_id,
        "COMMAND": identity.command,
        "SELECTED_EXECUTABLE": identity.windows_executable,
    }


def verification_text(inputs: PackageInputs, identity: PackageIdentity) -> str:
    """Return the operator-auditable provenance record shipped inside each package."""

    x86 = inputs.archive(WINDOWS_X86.triple)
    x64 = inputs.archive(WINDOWS_X64.triple)
    return (
        "VERIFICATION\n"
        "\n"
        f"{PRODUCT_NAME} {identity.package_id} {inputs.version} retains only "
        f"{identity.windows_executable}, taken from an immutable {PRODUCT_NAME} GitHub Release "
        "archive that this package downloads and checksums during installation.\n"
        "\n"
        f"repository: {inputs.repository}\n"
        f"tag: {inputs.tag}\n"
        f"commit: {inputs.commit}\n"
        f"release: {inputs.release_url}\n"
        f"selected executable: {identity.windows_executable}\n"
        f"{WINDOWS_X86.triple} url: {x86.url}\n"
        f"{WINDOWS_X86.triple} sha256: {x86.sha256}\n"
        f"{WINDOWS_X64.triple} url: {x64.url}\n"
        f"{WINDOWS_X64.triple} sha256: {x64.sha256}\n"
        "\n"
        "Verify a downloaded archive with `Get-FileHash -Algorithm SHA256 <archive>` and compare "
        f"the result with the value above or with the {release.CHECKSUM_FILE} asset published "
        "beside the archives.\n"
        "\n"
        f"License: {LICENSE_EXPRESSION} ({APACHE_LICENSE_FILE} and {MIT_LICENSE_FILE} in this "
        "directory).\n"
    )


def read_template(path: Path) -> str:
    """Read one reviewed package template as UTF-8 text."""

    if not path.is_file():
        raise ChannelError(f"package template does not exist: {path}")
    try:
        return path.read_text(encoding="utf-8")
    except (OSError, UnicodeError) as error:
        raise ChannelError(f"cannot read package template {path}: {error}") from error


def write_generated(path: Path, text: str) -> None:
    """Write one generated package file with normalized line endings."""

    path.parent.mkdir(parents=True, exist_ok=True)
    with path.open("w", encoding="utf-8", newline="\n") as output:
        output.write(text if text.endswith("\n") else f"{text}\n")


def require_substituted(text: str, label: str) -> str:
    """Require a rendered artifact to be non-empty and free of template tokens."""

    if not text.strip():
        raise ChannelError(f"generated package file is empty: {label}")
    remaining = sorted(set(TOKEN_PATTERN.findall(text)))
    if remaining:
        raise ChannelError(f"{label} still contains unsubstituted tokens {remaining!r}")
    return text


def read_generated(path: Path) -> str:
    """Read one generated package artifact and reject unsubstituted content."""

    if not path.is_file():
        raise ChannelError(f"generated package file does not exist: {path}")
    try:
        text = path.read_text(encoding="utf-8")
    except (OSError, UnicodeError) as error:
        raise ChannelError(f"cannot read generated package file {path}: {error}") from error
    return require_substituted(text, str(path))


def generate_formulae(
    inputs: PackageInputs, *, template_directory: Path, output_directory: Path
) -> dict[str, Path]:
    """Render both Formulae from reviewed templates into a tap-shaped tree."""

    generated: dict[str, Path] = {}
    for identity in PACKAGES:
        template = template_directory / f"{identity.package_id}{FORMULA_SUFFIX}{TEMPLATE_SUFFIX}"
        rendered = render_template(read_template(template), formula_tokens(inputs, identity))
        destination = output_directory / identity.formula_path
        write_generated(destination, require_substituted(rendered, str(template)))
        generated[identity.package_id] = destination
    return generated


def generate_chocolatey_sources(
    inputs: PackageInputs,
    *,
    template_directory: Path,
    output_directory: Path,
    license_directory: Path | None = None,
) -> dict[str, Path]:
    """Render both Chocolatey package sources with pinned URLs and license evidence."""

    licenses = REPOSITORY_ROOT if license_directory is None else license_directory
    generated: dict[str, Path] = {}
    for identity in PACKAGES:
        source = template_directory / identity.package_id
        root = output_directory / identity.package_id
        nuspec_template = source / f"{identity.package_id}{NUSPEC_SUFFIX}{TEMPLATE_SUFFIX}"
        write_generated(
            root / f"{identity.package_id}{NUSPEC_SUFFIX}",
            require_substituted(
                render_template(read_template(nuspec_template), nuspec_tokens(inputs, identity)),
                str(nuspec_template),
            ),
        )
        install_template = source / f"{INSTALL_SCRIPT}{TEMPLATE_SUFFIX}"
        write_generated(
            root / INSTALL_SCRIPT,
            require_substituted(
                render_template(
                    read_template(install_template), install_script_tokens(inputs, identity)
                ),
                str(install_template),
            ),
        )
        uninstall_template = source / f"{UNINSTALL_SCRIPT}{TEMPLATE_SUFFIX}"
        if uninstall_template.is_file():
            write_generated(
                root / UNINSTALL_SCRIPT,
                require_substituted(
                    render_template(
                        read_template(uninstall_template),
                        uninstall_script_tokens(inputs, identity),
                    ),
                    str(uninstall_template),
                ),
            )
        tools = root / TOOLS_DIRECTORY
        tools.mkdir(parents=True, exist_ok=True)
        for name in release.LICENSE_FILES:
            license_path = licenses / name
            if not license_path.is_file():
                raise ChannelError(f"package license file does not exist: {license_path}")
            shutil.copyfile(license_path, tools / name)
        write_generated(root / VERIFICATION_FILE, verification_text(inputs, identity))
        generated[identity.package_id] = root
    return generated


def require_pair(entries: Mapping[str, Any], label: str) -> None:
    """Require exactly the two ordered package identities."""

    expected = sorted(package.package_id for package in PACKAGES)
    if sorted(entries) != expected:
        raise ChannelError(f"{label} set is {sorted(entries)!r}; expected {expected!r}")


def mask_shared_identity(text: str, inputs: PackageInputs) -> str:
    """Blank every shared repository, product, and package name from a probe."""

    names = [
        inputs.release_url,
        HOMEPAGE,
        inputs.repository,
        PRODUCT_NAME,
        *(package.package_id for package in PACKAGES),
        *(package.formula_class for package in PACKAGES),
        *(archive.url for archive in inputs.archives),
        *(archive.name for archive in inputs.archives),
    ]
    masked = text
    for name in sorted(set(names), key=len, reverse=True):
        masked = masked.replace(name, " " * len(name))
    return masked


def unrender(text: str, values: Mapping[str, str], aliases: Mapping[str, str]) -> str:
    """Replace every substituted token value with its canonical placeholder."""

    masked = text
    for name, value in sorted(values.items(), key=lambda item: len(item[1]), reverse=True):
        masked = masked.replace(value, f"@{aliases.get(name, name)}@")
    return masked


def require_selected_command_only(
    text: str, path: Path, inputs: PackageInputs, identity: PackageIdentity
) -> None:
    """Require the pair member's command to appear only inside the Formula test block."""

    if f'bin/"{identity.command}"' not in text:
        raise ChannelError(f"{path} never installs or exercises bin/{identity.command!r}")
    marker = TEST_BLOCK_PATTERN.search(text)
    if marker is None:
        raise ChannelError(
            f"{path} declares no `test do` block; expected one asserting its own command"
        )
    probe = mask_shared_identity(text[: marker.start()], inputs)
    other = re.compile(rf"\b{re.escape(identity.other.command)}\b")
    match = other.search(probe)
    if match is not None:
        line = probe[: match.start()].count("\n") + 1
        raise ChannelError(
            f"{path} line {line} names the pair member command {identity.other.command!r} "
            "outside its test block"
        )


def formula_facts(
    text: str, path: Path, inputs: PackageInputs, identity: PackageIdentity
) -> dict[str, str]:
    """Extract and validate one Formula's observable identity and selection."""

    match = FORMULA_CLASS_PATTERN.search(text)
    if match is None or match.group(1) != identity.formula_class:
        observed = None if match is None else match.group(1)
        raise ChannelError(
            f"{path} declares Formula class {observed!r}; expected {identity.formula_class!r}"
        )
    scalars: dict[str, str] = {}
    for name, pattern in sorted(FORMULA_SCALAR_PATTERNS.items()):
        found = pattern.findall(text)
        if len(found) != 1:
            raise ChannelError(
                f"{path} declares {len(found)} {name!r} values; expected exactly one"
            )
        scalars[name] = found[0]
    archive = inputs.archive(MACOS_ARM64.triple)
    for name, expected in (
        ("desc", identity.description),
        ("homepage", HOMEPAGE),
        ("url", archive.url),
        ("sha256", archive.sha256),
        ("license", HOMEBREW_LICENSE_EXPRESSION),
    ):
        if scalars[name] != expected:
            raise ChannelError(f"{path} declares {name} {scalars[name]!r}; expected {expected!r}")
    for label, expected in (
        ("version", inputs.version),
        ("tag", inputs.tag),
        ("commit", inputs.commit),
    ):
        if expected not in text:
            raise ChannelError(f"{path} does not record the released {label} {expected!r}")
    dependencies = tuple(sorted(set(DEPENDS_ON_PATTERN.findall(text))))
    if dependencies != FORMULA_DEPENDENCIES:
        raise ChannelError(
            f"{path} declares dependencies {list(dependencies)!r}; "
            f"expected {list(FORMULA_DEPENDENCIES)!r}"
        )
    if "conflicts_with" in text:
        raise ChannelError(
            f"{path} declares conflicts_with; independently selectable Formulae must not conflict"
        )
    binaries = tuple(BIN_INSTALL_PATTERN.findall(text))
    if binaries != (identity.command,):
        raise ChannelError(
            f"{path} installs binaries {list(binaries)!r}; expected [{identity.command!r}]"
        )
    package_data = PKGSHARE_INSTALL_PATTERN.findall(text)
    expected_data = (*release.LICENSE_FILES, release.VERSION_FILE)
    observed_data = () if len(package_data) != 1 else tuple(re.findall(r'"([^"]+)"', package_data[0]))
    if observed_data != expected_data:
        raise ChannelError(
            f"{path} installs package data {list(observed_data)!r}; "
            f"expected {list(expected_data)!r}"
        )
    for forbidden in ('system "cargo"', "std_cargo_args"):
        if forbidden in text:
            raise ChannelError(
                f"{path} invokes Cargo through {forbidden!r}; "
                "binary Formulae must install only validated release members"
            )
    require_selected_command_only(text, path, inputs, identity)
    scalars["dependencies"] = " | ".join(dependencies)
    return scalars


def require_identical_formulae(probes: Mapping[str, str]) -> None:
    """Require both Formulae to differ only in their selection tokens."""

    first, second = PACKAGES
    if probes[first.package_id] != probes[second.package_id]:
        raise ChannelError(
            f"Formulae for {first.package_id} and {second.package_id} differ outside the "
            "command-selection tokens"
        )


def inspect_formulae(paths: Mapping[str, Path], inputs: PackageInputs) -> None:
    """Require a complete, provenance-identical, selection-only Formula pair."""

    require_pair(paths, "generated Formula")
    facts: dict[str, dict[str, str]] = {}
    probes: dict[str, str] = {}
    for identity in PACKAGES:
        path = paths[identity.package_id]
        text = read_generated(path)
        facts[identity.package_id] = formula_facts(text, path, inputs, identity)
        probes[identity.package_id] = unrender(
            text, formula_tokens(inputs, identity), FORMULA_SELECTION_ALIASES
        )
    first, second = PACKAGES
    for key in FORMULA_SHARED_FACTS:
        left = facts[first.package_id][key]
        right = facts[second.package_id][key]
        if left != right:
            raise ChannelError(
                f"Formula pair disagrees on {key}: {first.package_id} reports {left!r} while "
                f"{second.package_id} reports {right!r}"
            )
    require_identical_formulae(probes)


def nuspec_field(text: str, name: str, label: str) -> str:
    """Return the single value one nuspec element declares."""

    pattern = re.compile(rf"<{name}>(.*?)</{name}>", re.DOTALL)
    found = pattern.findall(text)
    if len(found) != 1:
        raise ChannelError(
            f"{label} declares {len(found)} <{name}> elements; expected exactly one"
        )
    return found[0].strip()


def verify_nuspec(
    text: str, inputs: PackageInputs, identity: PackageIdentity, *, label: str
) -> None:
    """Require every nuspec identity, URL, and prose field to match the validated inputs."""

    tokens = nuspec_tokens(inputs, identity)
    for element, token in (
        ("id", "PACKAGE_ID"),
        ("version", "VERSION"),
        ("projectUrl", "PROJECT_URL"),
        ("projectSourceUrl", "PROJECT_SOURCE_URL"),
        ("licenseUrl", "LICENSE_URL"),
        ("releaseNotes", "RELEASE_NOTES_URL"),
    ):
        observed = nuspec_field(text, element, label)
        if observed != tokens[token]:
            raise ChannelError(
                f"{label} declares <{element}> {observed!r}; expected {tokens[token]!r}"
            )
    for element, token in (
        ("title", "TITLE"),
        ("summary", "SUMMARY"),
        ("description", "DESCRIPTION"),
    ):
        observed = nuspec_field(text, element, label)
        if tokens[token] not in observed:
            raise ChannelError(
                f"{label} declares <{element}> {observed!r}; expected it to state "
                f"{tokens[token]!r}"
            )


def verify_install_script(
    text: str, inputs: PackageInputs, identity: PackageIdentity, *, label: str
) -> None:
    """Require pinned architecture downloads and profile-free selection behavior."""

    tokens = install_script_tokens(inputs, identity)
    required = {
        "package version": tokens["VERSION"],
        "selected executable": tokens["SELECTED_EXECUTABLE"],
        "unselected executable": tokens["OTHER_EXECUTABLE"],
        "x64 archive URL": tokens["URL_X64"],
        "x64 archive digest": tokens["SHA256_X64"],
        "x64 archive root": tokens["ARCHIVE_ROOT_X64"],
        "x86 archive URL": tokens["URL_X86"],
        "x86 archive digest": tokens["SHA256_X86"],
        "x86 archive root": tokens["ARCHIVE_ROOT_X86"],
    }
    for description, value in sorted(required.items()):
        if value not in text:
            raise ChannelError(f"{label} install script omits the {description} {value!r}")
    if ERROR_PREFERENCE_PATTERN.search(text) is None:
        raise ChannelError(f"{label} install script does not set $ErrorActionPreference to 'Stop'")
    if STRICT_MODE_PATTERN.search(text) is None:
        raise ChannelError(f"{label} install script does not set Set-StrictMode -Version 2")
    for marker, description in sorted(FORBIDDEN_INSTALL_MARKERS.items()):
        if marker in text:
            raise ChannelError(f"{label} install script performs {description} via {marker!r}")


def verify_verification_text(
    text: str, inputs: PackageInputs, identity: PackageIdentity, *, label: str
) -> None:
    """Require the shipped provenance record to name every verifiable value."""

    x86 = inputs.archive(WINDOWS_X86.triple)
    x64 = inputs.archive(WINDOWS_X64.triple)
    required = {
        "commit": inputs.commit,
        "repository": inputs.repository,
        "selected executable": identity.windows_executable,
        "tag": inputs.tag,
        "x64 archive URL": x64.url,
        "x64 archive digest": x64.sha256,
        "x86 archive URL": x86.url,
        "x86 archive digest": x86.sha256,
    }
    for description, value in sorted(required.items()):
        if value not in text:
            raise ChannelError(f"{label} VERIFICATION.txt omits the {description} {value!r}")


def chocolatey_member_names(root: Path) -> tuple[str, ...]:
    """List every generated package member relative to its source root."""

    if not root.is_dir():
        raise ChannelError(f"generated Chocolatey package root does not exist: {root}")
    names: list[str] = []
    for path in sorted(root.rglob("*")):
        if path.is_symlink():
            raise ChannelError(f"generated Chocolatey package contains a link: {path}")
        if path.is_dir():
            continue
        if not path.is_file():
            raise ChannelError(
                f"generated Chocolatey package member is not a regular file: {path}"
            )
        names.append(path.relative_to(root).as_posix())
    return tuple(sorted(names))


def expected_chocolatey_members(identity: PackageIdentity, *, uninstall: bool) -> tuple[str, ...]:
    """Return the exact member set one generated package source may contain."""

    names = [
        f"{identity.package_id}{NUSPEC_SUFFIX}",
        INSTALL_SCRIPT,
        VERIFICATION_FILE,
        *(f"{TOOLS_DIRECTORY}/{name}" for name in release.LICENSE_FILES),
    ]
    if uninstall:
        names.append(UNINSTALL_SCRIPT)
    return tuple(sorted(names))


def require_identical_install_scripts(probes: Mapping[str, str]) -> None:
    """Require both install scripts to differ only in their selection tokens."""

    first, second = PACKAGES
    if probes[first.package_id] != probes[second.package_id]:
        raise ChannelError(
            f"install scripts for {first.package_id} and {second.package_id} differ outside the "
            "command-selection tokens"
        )


def inspect_chocolatey_sources(roots: Mapping[str, Path], inputs: PackageInputs) -> None:
    """Require a complete, provenance-identical, selection-only Chocolatey pair."""

    require_pair(roots, "generated Chocolatey package")
    probes: dict[str, str] = {}
    for identity in PACKAGES:
        root = roots[identity.package_id]
        label = str(root)
        members = chocolatey_member_names(root)
        expected = expected_chocolatey_members(identity, uninstall=UNINSTALL_SCRIPT in members)
        if members != expected:
            raise ChannelError(f"{label} contains {list(members)!r}; expected {list(expected)!r}")
        verify_nuspec(
            read_generated(root / f"{identity.package_id}{NUSPEC_SUFFIX}"),
            inputs,
            identity,
            label=label,
        )
        script = read_generated(root / INSTALL_SCRIPT)
        verify_install_script(script, inputs, identity, label=label)
        verify_verification_text(
            read_generated(root / VERIFICATION_FILE), inputs, identity, label=label
        )
        probes[identity.package_id] = unrender(
            script, install_script_tokens(inputs, identity), CHOCOLATEY_SELECTION_ALIASES
        )
    require_identical_install_scripts(probes)


def nupkg_name(identity: PackageIdentity, version: str) -> str:
    """Return the exact packed candidate filename for one package identity."""

    require_release_value(release.validate_stable_version, version)
    return f"{identity.package_id}.{version}{NUPKG_SUFFIX}"


def validate_nupkg_member(name: str, label: str) -> None:
    """Reject unsafe, absolute, traversing, or executable package members."""

    if "\\" in name:
        raise ChannelError(f"{label} member uses a backslash: {name!r}")
    if re.match(r"[A-Za-z]:", name) is not None:
        raise ChannelError(f"{label} member names a drive letter: {name!r}")
    path = PurePosixPath(name)
    if path.is_absolute() or any(part in ("", ".", "..") for part in path.parts):
        raise ChannelError(f"{label} member is not a safe relative path: {name!r}")
    lowered = name.lower()
    for suffix in FORBIDDEN_MEMBER_SUFFIXES:
        if lowered.endswith(suffix):
            raise ChannelError(f"{label} member {name!r} is an executable or archive payload")


def decode_member(package: zipfile.ZipFile, name: str, label: str) -> str:
    """Read one package member as text without extracting it."""

    try:
        data = package.read(name)
    except KeyError as error:
        raise ChannelError(f"{label} does not contain {name}") from error
    try:
        return data.decode("utf-8-sig")
    except UnicodeDecodeError as error:
        raise ChannelError(f"{label} member {name} is not valid UTF-8: {error}") from error


def inspect_nupkg(path: Path, inputs: PackageInputs, identity: PackageIdentity) -> None:
    """Verify one packed candidate as data, without extracting any member."""

    expected_name = nupkg_name(identity, inputs.version)
    if path.name != expected_name:
        raise ChannelError(f"packed candidate is named {path.name!r}; expected {expected_name!r}")
    if not path.is_file():
        raise ChannelError(f"packed candidate does not exist: {path}")
    label = str(path)
    try:
        with zipfile.ZipFile(path) as package:
            names = [member.filename for member in package.infolist()]
            if len(names) != len(set(names)):
                raise ChannelError(f"{label} contains duplicate members: {sorted(names)!r}")
            for name in names:
                validate_nupkg_member(name.rstrip("/") if name.endswith("/") else name, label)
            observed = {name for name in names if not name.endswith("/")}
            metadata = sorted(name for name in observed if PSMDCP_PATTERN.fullmatch(name))
            if len(metadata) != 1:
                raise ChannelError(
                    f"{label} declares {len(metadata)} NuGet core-properties members; "
                    "expected exactly one"
                )
            expected = {
                f"{identity.package_id}{NUSPEC_SUFFIX}",
                INSTALL_SCRIPT,
                VERIFICATION_FILE,
                *(f"{TOOLS_DIRECTORY}/{name}" for name in release.LICENSE_FILES),
                CONTENT_TYPES_MEMBER,
                RELS_MEMBER,
                metadata[0],
            }
            optional = {UNINSTALL_SCRIPT}
            unexpected = sorted(observed - expected - optional)
            missing = sorted(expected - observed)
            if unexpected or missing:
                raise ChannelError(
                    f"{label} contains members {sorted(observed)!r}; expected "
                    f"{sorted(expected)!r} plus optional {sorted(optional)!r} "
                    f"(missing {missing!r}, unexpected {unexpected!r})"
                )
            nuspec = decode_member(package, f"{identity.package_id}{NUSPEC_SUFFIX}", label)
            declared_id = nuspec_field(nuspec, "id", label)
            if declared_id != identity.package_id:
                raise ChannelError(
                    f"{label} declares package id {declared_id!r}; "
                    f"expected {identity.package_id!r}"
                )
            declared_version = nuspec_field(nuspec, "version", label)
            if declared_version != inputs.version:
                raise ChannelError(
                    f"{label} declares version {declared_version!r}; expected {inputs.version!r}"
                )
            script = decode_member(package, INSTALL_SCRIPT, label)
            for target in (WINDOWS_X86, WINDOWS_X64):
                archive = inputs.archive(target.triple)
                if archive.url not in script:
                    raise ChannelError(
                        f"{label} install script omits the {target.triple} URL {archive.url!r}"
                    )
                if archive.sha256 not in script:
                    raise ChannelError(
                        f"{label} install script omits the {target.triple} digest "
                        f"{archive.sha256!r}"
                    )
    except (OSError, zipfile.BadZipFile) as error:
        raise ChannelError(f"cannot inspect packed candidate {path}: {error}") from error


def inspect_nupkg_pair(paths: Mapping[str, Path], inputs: PackageInputs) -> dict[str, str]:
    """Verify both packed candidates and return their digests."""

    require_pair(paths, "packed Chocolatey candidate")
    digests: dict[str, str] = {}
    probes: dict[str, str] = {}
    for identity in PACKAGES:
        path = paths[identity.package_id]
        inspect_nupkg(path, inputs, identity)
        digests[identity.package_id] = release.sha256_file(path)
        try:
            with zipfile.ZipFile(path) as package:
                script = decode_member(package, INSTALL_SCRIPT, str(path))
        except (OSError, zipfile.BadZipFile) as error:
            raise ChannelError(f"cannot read packed candidate {path}: {error}") from error
        probes[identity.package_id] = unrender(
            script, install_script_tokens(inputs, identity), CHOCOLATEY_SELECTION_ALIASES
        )
    require_identical_install_scripts(probes)
    return digests


def selection_map_lines() -> tuple[str, ...]:
    """Return the immutable package selection map in publication order."""

    return tuple(
        f"{identity.package_id} command={identity.command} "
        f"windows-executable={identity.windows_executable} formula={identity.formula_path} "
        f"formula-class={identity.formula_class}"
        for identity in PACKAGES
    )


def read_workflow_run(path: Path | None) -> dict[str, Any] | None:
    """Read the triggering workflow-run payload as untrusted data."""

    if path is None:
        return None
    if not path.is_file():
        raise ChannelError(f"workflow-run payload does not exist: {path}")
    try:
        text = path.read_text(encoding="utf-8")
    except (OSError, UnicodeError) as error:
        raise ChannelError(f"cannot read workflow-run payload {path}: {error}") from error
    document = json.loads(text)
    if not isinstance(document, dict):
        raise ChannelError(
            f"workflow-run payload is a {type(document).__name__}; expected a JSON object"
        )
    return document


def load_inputs(path: Path) -> PackageInputs:
    """Load and revalidate the preflight artifact from disk."""

    if not path.is_file():
        raise ChannelError(f"package inputs artifact does not exist: {path}")
    try:
        text = path.read_text(encoding="utf-8")
    except (OSError, UnicodeError) as error:
        raise ChannelError(f"cannot read package inputs {path}: {error}") from error
    return PackageInputs.from_json(text)


def argument_parser() -> argparse.ArgumentParser:
    """Build the package-channel command-line parser."""

    parser = argparse.ArgumentParser(description=__doc__)
    subparsers = parser.add_subparsers(dest="command", required=True)

    preflight_parser = subparsers.add_parser("preflight")
    preflight_parser.add_argument("--repository", default=DEFAULT_REPOSITORY)
    preflight_parser.add_argument("--event-name", required=True)
    preflight_parser.add_argument("--workflow-run-json", type=Path)
    preflight_parser.add_argument("--dispatch-tag")
    preflight_parser.add_argument(
        "--verification-only", choices=("true", "false"), default="false"
    )
    preflight_parser.add_argument("--work-directory", type=Path, required=True)
    preflight_parser.add_argument("--inputs-path", type=Path, required=True)
    preflight_parser.add_argument("--github-output", type=Path, required=True)

    homebrew_parser = subparsers.add_parser("generate-homebrew")
    homebrew_parser.add_argument("--inputs", type=Path, required=True)
    homebrew_parser.add_argument("--template-directory", type=Path, required=True)
    homebrew_parser.add_argument("--output-directory", type=Path, required=True)

    chocolatey_parser = subparsers.add_parser("generate-chocolatey")
    chocolatey_parser.add_argument("--inputs", type=Path, required=True)
    chocolatey_parser.add_argument("--template-directory", type=Path, required=True)
    chocolatey_parser.add_argument("--output-directory", type=Path, required=True)
    chocolatey_parser.add_argument("--license-directory", type=Path)

    inspect_homebrew_parser = subparsers.add_parser("inspect-homebrew")
    inspect_homebrew_parser.add_argument("--inputs", type=Path, required=True)
    inspect_homebrew_parser.add_argument("--directory", type=Path, required=True)

    inspect_chocolatey_parser = subparsers.add_parser("inspect-chocolatey")
    inspect_chocolatey_parser.add_argument("--inputs", type=Path, required=True)
    inspect_chocolatey_parser.add_argument("--directory", type=Path, required=True)

    inspect_nupkg_parser = subparsers.add_parser("inspect-nupkg")
    inspect_nupkg_parser.add_argument("--inputs", type=Path, required=True)
    inspect_nupkg_parser.add_argument("--directory", type=Path, required=True)

    subparsers.add_parser("selection-map")
    return parser


def run(arguments: Sequence[str]) -> int:
    """Run one package-channel subcommand."""

    options = argument_parser().parse_args(arguments)
    if options.command == "preflight":
        decision = evaluate_trigger(
            event_name=options.event_name,
            repository=options.repository,
            workflow_run=read_workflow_run(options.workflow_run_json),
            dispatch_tag=options.dispatch_tag,
            dispatch_verification_only=options.verification_only == "true",
        )
        inputs = preflight(
            GhReleaseGateway(Path.cwd()),
            repository=options.repository,
            tag=decision.tag,
            work_directory=options.work_directory,
        )
        options.inputs_path.parent.mkdir(parents=True, exist_ok=True)
        options.inputs_path.write_text(inputs.to_json(), encoding="utf-8", newline="\n")
        release.append_github_outputs(
            options.github_output,
            {
                "version": inputs.version,
                "tag": inputs.tag,
                "commit": inputs.commit,
                "verification_only": str(decision.verification_only).lower(),
                "inputs_sha256": release.sha256_file(options.inputs_path),
            },
        )
        print(f"Validated {inputs.tag} at {inputs.commit} from {decision.reason}")
        return 0
    if options.command == "generate-homebrew":
        formulae = generate_formulae(
            load_inputs(options.inputs),
            template_directory=options.template_directory,
            output_directory=options.output_directory,
        )
        for package_id in sorted(formulae):
            print(f"{package_id} {formulae[package_id]}")
        return 0
    if options.command == "generate-chocolatey":
        sources = generate_chocolatey_sources(
            load_inputs(options.inputs),
            template_directory=options.template_directory,
            output_directory=options.output_directory,
            license_directory=options.license_directory,
        )
        for package_id in sorted(sources):
            print(f"{package_id} {sources[package_id]}")
        return 0
    if options.command == "inspect-homebrew":
        inputs = load_inputs(options.inputs)
        inspect_formulae(
            {
                identity.package_id: options.directory / identity.formula_path
                for identity in PACKAGES
            },
            inputs,
        )
        print(f"Verified both {inputs.version} Formulae in {options.directory}")
        return 0
    if options.command == "inspect-chocolatey":
        inputs = load_inputs(options.inputs)
        inspect_chocolatey_sources(
            {
                identity.package_id: options.directory / identity.package_id
                for identity in PACKAGES
            },
            inputs,
        )
        print(f"Verified both {inputs.version} Chocolatey sources in {options.directory}")
        return 0
    if options.command == "inspect-nupkg":
        inputs = load_inputs(options.inputs)
        digests = inspect_nupkg_pair(
            {
                identity.package_id: options.directory / nupkg_name(identity, inputs.version)
                for identity in PACKAGES
            },
            inputs,
        )
        for package_id in sorted(digests):
            print(f"{package_id} {digests[package_id]}")
        return 0
    if options.command == "selection-map":
        for line in selection_map_lines():
            print(line)
        return 0
    raise ChannelError(f"unhandled package-channel command {options.command!r}")


def main(arguments: Sequence[str] | None = None) -> int:
    """Convert package-channel invariant failures into a stable nonzero status."""

    try:
        return run(sys.argv[1:] if arguments is None else arguments)
    except (
        OSError,
        ChannelError,
        release.ReleaseError,
        UnicodeError,
        zipfile.BadZipFile,
        json.JSONDecodeError,
    ) as error:
        print(f"package channel validation failed: {error}", file=sys.stderr)
        return 1


if __name__ == "__main__":
    raise SystemExit(main())
