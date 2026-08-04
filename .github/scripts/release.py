#!/usr/bin/env python3
"""Validate, package, and aggregate SkillMount release artifacts."""

from __future__ import annotations

import argparse
import gzip
import hashlib
import json
import os
import re
import shutil
import stat
import subprocess
import sys
import tarfile
import tempfile
import zipfile
from dataclasses import dataclass
from pathlib import Path, PurePosixPath
from typing import Sequence

PRODUCT_NAME = "SkillMount"
LICENSE_FILES = ("LICENSE-APACHE", "LICENSE-MIT")
VERSION_FILE = "VERSION"
CHECKSUM_FILE = "SHA256SUMS"
ZIP_TIMESTAMP = (1980, 1, 1, 0, 0, 0)
TAG_PATTERN = re.compile(
    r"v(0|[1-9][0-9]*)\.(0|[1-9][0-9]*)\.(0|[1-9][0-9]*)"
)
COMMIT_PATTERN = re.compile(r"[0-9a-f]{40}")


class ReleaseError(RuntimeError):
    """A release invariant was not satisfied."""


@dataclass(frozen=True)
class Target:
    """One supported native release target."""

    name: str
    runner: str
    runner_arch: str
    host: str
    triple: str
    extension: str
    executable_suffix: str

    def matrix_entry(self) -> dict[str, str]:
        """Return the GitHub Actions matrix row for this target."""

        return {
            "name": self.name,
            "runner": self.runner,
            "runner_arch": self.runner_arch,
            "host": self.host,
            "target": self.triple,
        }


TARGETS = (
    Target(
        name="windows-x64",
        runner="windows-2025",
        runner_arch="X64",
        host="x86_64-pc-windows-msvc",
        triple="x86_64-pc-windows-msvc",
        extension=".zip",
        executable_suffix=".exe",
    ),
    Target(
        name="windows-x86",
        runner="windows-2025",
        runner_arch="X64",
        host="x86_64-pc-windows-msvc",
        triple="i686-pc-windows-msvc",
        extension=".zip",
        executable_suffix=".exe",
    ),
    Target(
        name="macos-arm64",
        runner="macos-15",
        runner_arch="ARM64",
        host="aarch64-apple-darwin",
        triple="aarch64-apple-darwin",
        extension=".tar.gz",
        executable_suffix="",
    ),
)
TARGET_BY_TRIPLE = {target.triple: target for target in TARGETS}


@dataclass(frozen=True)
class PreflightResult:
    """Validated values safe to expose as workflow outputs."""

    version: str
    tag: str
    commit: str
    publish: bool


def stable_version_from_tag(tag: str) -> str:
    """Return a stable semantic version from an exact release tag."""

    match = TAG_PATTERN.fullmatch(tag)
    if match is None:
        raise ReleaseError(
            f"release tag {tag!r} must match vMAJOR.MINOR.PATCH without "
            "prerelease metadata or leading zeroes"
        )
    return ".".join(match.groups())


def validate_stable_version(version: str) -> str:
    """Validate an unprefixed stable semantic version."""

    stable_version_from_tag(f"v{version}")
    return version


def validate_commit(commit: str) -> str:
    """Validate a full lowercase Git commit object ID."""

    if COMMIT_PATTERN.fullmatch(commit) is None:
        raise ReleaseError(f"commit {commit!r} is not a full lowercase SHA-1 object ID")
    return commit


def target_for(triple: str) -> Target:
    """Return the supported target definition for *triple*."""

    try:
        return TARGET_BY_TRIPLE[triple]
    except KeyError as error:
        supported = ", ".join(target.triple for target in TARGETS)
        raise ReleaseError(f"unsupported release target {triple!r}; expected {supported}") from error


def asset_stem(tag: str, target: Target) -> str:
    """Return the archive's top-level directory and filename stem."""

    stable_version_from_tag(tag)
    return f"skillmount-{tag}-{target.triple}"


def asset_name(tag: str, target: Target) -> str:
    """Return the deterministic archive name for one target."""

    return f"{asset_stem(tag, target)}{target.extension}"


def expected_archive_names(tag: str) -> tuple[str, ...]:
    """Return every archive name in deterministic order."""

    return tuple(sorted(asset_name(tag, target) for target in TARGETS))


def executable_names(target: Target) -> tuple[str, str]:
    """Return the two product executable names for a target."""

    return (
        f"asm{target.executable_suffix}",
        f"skillmount{target.executable_suffix}",
    )


def expected_file_names(target: Target) -> tuple[str, ...]:
    """Return the exact package file set in archive order."""

    return tuple(sorted((*executable_names(target), *LICENSE_FILES, VERSION_FILE)))


def expected_member_names(tag: str, target: Target) -> tuple[str, ...]:
    """Return the exact normalized archive member order."""

    root = asset_stem(tag, target)
    return (f"{root}/", *(f"{root}/{name}" for name in expected_file_names(target)))


def version_metadata(version: str, tag: str, target: Target, commit: str) -> bytes:
    """Return deterministic release metadata stored in every package."""

    validate_stable_version(version)
    if stable_version_from_tag(tag) != version:
        raise ReleaseError(f"tag {tag!r} does not match package version {version!r}")
    validate_commit(commit)
    return (
        f"name={PRODUCT_NAME}\n"
        f"version={version}\n"
        f"tag={tag}\n"
        f"target={target.triple}\n"
        f"commit={commit}\n"
    ).encode()


def evaluate_preflight(
    *,
    event_name: str,
    ref_name: str,
    commit: str,
    package_version: str,
    tag_commit: str | None,
    main_contains_commit: bool | None,
    workflow_files_match_main: bool | None,
) -> PreflightResult:
    """Apply event, tag, version, and provenance policy to observed values."""

    validate_commit(commit)
    validate_stable_version(package_version)
    if event_name == "push":
        tag_version = stable_version_from_tag(ref_name)
        if tag_version != package_version:
            raise ReleaseError(
                f"tag version {tag_version!r} does not match Cargo package version "
                f"{package_version!r}"
            )
        if tag_commit != commit:
            raise ReleaseError(
                f"tag {ref_name!r} resolves to {tag_commit!r}, not checked-out commit {commit}"
            )
        if main_contains_commit is not True:
            raise ReleaseError(
                f"tag commit {commit} is not an ancestor of the fetched origin/main"
            )
        if workflow_files_match_main is not True:
            raise ReleaseError(
                "tag commit changes .github/workflows relative to origin/main; "
                "GitHub Releases rejects the Actions GITHUB_TOKEN in this state"
            )
        return PreflightResult(
            version=package_version,
            tag=ref_name,
            commit=commit,
            publish=True,
        )

    if event_name == "workflow_dispatch":
        if not ref_name or any(character.isspace() for character in ref_name):
            raise ReleaseError("manual verification requires one non-whitespace ref input")
        return PreflightResult(
            version=package_version,
            tag=f"v{package_version}",
            commit=commit,
            publish=False,
        )

    raise ReleaseError(
        f"unsupported release event {event_name!r}; expected push or workflow_dispatch"
    )


def run_checked(
    arguments: Sequence[str], *, cwd: Path, input_bytes: bytes | None = None
) -> subprocess.CompletedProcess[bytes]:
    """Run one shell-free command and retain bounded diagnostic output."""

    completed = subprocess.run(
        arguments,
        cwd=cwd,
        input=input_bytes,
        stdout=subprocess.PIPE,
        stderr=subprocess.PIPE,
        check=False,
    )
    if completed.returncode != 0:
        stderr = completed.stderr.decode(errors="replace").strip()
        command = " ".join(arguments[:3])
        raise ReleaseError(
            f"{command} failed with status {completed.returncode}: {stderr}"
        )
    return completed


def git_output(repository: Path, *arguments: str) -> str:
    """Run Git and return trimmed UTF-8 output."""

    completed = run_checked(("git", *arguments), cwd=repository)
    try:
        return completed.stdout.decode().strip()
    except UnicodeDecodeError as error:
        raise ReleaseError("git output was not valid UTF-8") from error


def cargo_package_version(repository: Path) -> str:
    """Read the root package version through locked Cargo metadata."""

    completed = run_checked(
        ("cargo", "metadata", "--locked", "--no-deps", "--format-version", "1"),
        cwd=repository,
    )
    try:
        metadata = json.loads(completed.stdout)
    except (UnicodeDecodeError, json.JSONDecodeError) as error:
        raise ReleaseError("cargo metadata did not return valid UTF-8 JSON") from error

    manifest = str((repository / "Cargo.toml").resolve())
    packages = [
        package
        for package in metadata.get("packages", [])
        if package.get("name") == "skillmount"
        and str(Path(package.get("manifest_path", "")).resolve()) == manifest
    ]
    if len(packages) != 1:
        raise ReleaseError(
            f"expected exactly one root skillmount package in Cargo metadata, found {len(packages)}"
        )
    version = packages[0].get("version")
    if not isinstance(version, str):
        raise ReleaseError("Cargo package version is missing or not a string")
    return validate_stable_version(version)


def git_is_ancestor(repository: Path, ancestor: str, descendant: str) -> bool:
    """Return whether Git proves *ancestor* is reachable from *descendant*."""

    completed = subprocess.run(
        ("git", "merge-base", "--is-ancestor", ancestor, descendant),
        cwd=repository,
        stdout=subprocess.PIPE,
        stderr=subprocess.PIPE,
        check=False,
    )
    if completed.returncode == 0:
        return True
    if completed.returncode == 1:
        return False
    stderr = completed.stderr.decode(errors="replace").strip()
    raise ReleaseError(f"git merge-base failed with status {completed.returncode}: {stderr}")


def git_paths_match(
    repository: Path, left: str, right: str, *paths: str
) -> bool:
    """Return whether Git proves selected paths have identical content."""

    completed = subprocess.run(
        ("git", "diff", "--quiet", left, right, "--", *paths),
        cwd=repository,
        stdout=subprocess.PIPE,
        stderr=subprocess.PIPE,
        check=False,
    )
    if completed.returncode == 0:
        return True
    if completed.returncode == 1:
        return False
    stderr = completed.stderr.decode(errors="replace").strip()
    raise ReleaseError(f"git diff failed with status {completed.returncode}: {stderr}")


def verify_publication_source(repository: Path, commit: str) -> None:
    """Revalidate the checked-out commit against the current remote main."""

    repository = repository.resolve()
    commit = validate_commit(commit)
    checked_out = validate_commit(
        git_output(repository, "rev-parse", "HEAD^{commit}")
    )
    if checked_out != commit:
        raise ReleaseError(
            f"checked-out commit {checked_out} does not match validated commit {commit}"
        )
    run_checked(
        (
            "git",
            "fetch",
            "--no-tags",
            "--force",
            "origin",
            "+refs/heads/main:refs/remotes/origin/main",
        ),
        cwd=repository,
    )
    if not git_is_ancestor(repository, commit, "origin/main"):
        raise ReleaseError(
            f"tag commit {commit} is not an ancestor of the fetched origin/main"
        )
    if not git_paths_match(
        repository, commit, "origin/main", ".github/workflows"
    ):
        raise ReleaseError(
            "tag commit changes .github/workflows relative to origin/main; "
            "GitHub Releases rejects the Actions GITHUB_TOKEN in this state"
        )


def preflight(
    repository: Path,
    *,
    event_name: str,
    ref_name: str,
    event_sha: str,
) -> PreflightResult:
    """Collect and validate release provenance from the checked-out repository."""

    repository = repository.resolve()
    commit = validate_commit(git_output(repository, "rev-parse", "HEAD^{commit}"))
    package_version = cargo_package_version(repository)

    tag_commit: str | None = None
    main_contains_commit: bool | None = None
    workflow_files_match_main: bool | None = None
    if event_name == "push":
        stable_version_from_tag(ref_name)
        event_commit = validate_commit(
            git_output(repository, "rev-parse", f"{event_sha}^{{commit}}")
        )
        if event_commit != commit:
            raise ReleaseError(
                f"event commit {event_commit} does not match checked-out commit {commit}"
            )
        tag_commit = validate_commit(
            git_output(repository, "rev-parse", f"refs/tags/{ref_name}^{{commit}}")
        )
        verify_publication_source(repository, commit)
        main_contains_commit = True
        workflow_files_match_main = True

    return evaluate_preflight(
        event_name=event_name,
        ref_name=ref_name,
        commit=commit,
        package_version=package_version,
        tag_commit=tag_commit,
        main_contains_commit=main_contains_commit,
        workflow_files_match_main=workflow_files_match_main,
    )


def workflow_outputs(result: PreflightResult) -> dict[str, str]:
    """Return validated preflight values for GitHub Actions."""

    matrix = {"include": [target.matrix_entry() for target in TARGETS]}
    return {
        "version": result.version,
        "tag": result.tag,
        "commit": result.commit,
        "publish": str(result.publish).lower(),
        "matrix": json.dumps(matrix, separators=(",", ":"), sort_keys=True),
    }


def append_github_outputs(path: Path, outputs: dict[str, str]) -> None:
    """Append single-line validated values to a GitHub output file."""

    for name, value in outputs.items():
        if "\n" in value or "\r" in value:
            raise ReleaseError(f"workflow output {name!r} unexpectedly contains a newline")
    with path.open("a", encoding="utf-8", newline="\n") as output_file:
        for name, value in outputs.items():
            output_file.write(f"{name}={value}\n")


def rustc_runner_evidence(
    repository: Path,
    *,
    toolchain: str,
    runner_arch: str,
    expected_runner_arch: str,
    expected_host: str,
    target: str,
) -> str:
    """Validate runner architecture, rustc host, and target availability."""

    target_for(target)
    if runner_arch != expected_runner_arch:
        raise ReleaseError(
            f"RUNNER_ARCH is {runner_arch!r}; expected {expected_runner_arch!r}"
        )
    verbose = run_checked(("rustc", f"+{toolchain}", "-vV"), cwd=repository)
    try:
        evidence = verbose.stdout.decode()
    except UnicodeDecodeError as error:
        raise ReleaseError("rustc -vV output was not valid UTF-8") from error
    hosts = [line.removeprefix("host: ") for line in evidence.splitlines() if line.startswith("host: ")]
    if hosts != [expected_host]:
        raise ReleaseError(f"rustc host evidence is {hosts!r}; expected [{expected_host!r}]")

    target_list = run_checked(
        ("rustc", f"+{toolchain}", "--print", "target-list"), cwd=repository
    )
    supported_targets = set(target_list.stdout.decode().splitlines())
    if target not in supported_targets:
        raise ReleaseError(f"rustc toolchain {toolchain!r} does not support target {target!r}")
    return evidence


def smoke_check(executable: Path, version: str) -> bytes:
    """Run one packaged executable's version command and return stdout."""

    validate_stable_version(version)
    if not executable.is_file():
        raise ReleaseError(f"release executable is missing: {executable}")
    completed = subprocess.run(
        (str(executable), "--version"),
        stdin=subprocess.DEVNULL,
        stdout=subprocess.PIPE,
        stderr=subprocess.PIPE,
        check=False,
    )
    if completed.returncode != 0:
        raise ReleaseError(
            f"{executable.name} --version failed with status {completed.returncode}"
        )
    expected = f"{PRODUCT_NAME} {version}\n".encode()
    if completed.stdout != expected:
        raise ReleaseError(
            f"{executable.name} --version returned {completed.stdout!r}; expected {expected!r}"
        )
    if completed.stderr:
        raise ReleaseError(
            f"{executable.name} --version unexpectedly wrote stderr: {completed.stderr!r}"
        )
    return completed.stdout


def smoke_pair(binary_directory: Path, target: Target, version: str) -> None:
    """Prove both executable names report the same package version."""

    outputs = [
        smoke_check(binary_directory / executable, version)
        for executable in executable_names(target)
    ]
    if outputs[0] != outputs[1]:
        raise ReleaseError("asm and skillmount version output did not match")


def require_regular_file(path: Path, label: str) -> os.stat_result:
    """Return lstat evidence for a regular, non-link file."""

    try:
        metadata = path.lstat()
    except OSError as error:
        raise ReleaseError(f"cannot inspect {label} at {path}: {error}") from error
    if not stat.S_ISREG(metadata.st_mode):
        raise ReleaseError(f"{label} is not a regular file: {path}")
    return metadata


def stage_package(
    repository: Path,
    binary_directory: Path,
    stage_root: Path,
    target: Target,
    metadata: bytes,
) -> None:
    """Stage only the exact package members under one top-level directory."""

    stage_root.mkdir()
    for executable in executable_names(target):
        source = binary_directory / executable
        source_metadata = require_regular_file(source, f"{executable} binary")
        if target.triple == "aarch64-apple-darwin" and source_metadata.st_mode & 0o111 == 0:
            raise ReleaseError(f"macOS release binary is not executable: {source}")
        destination = stage_root / executable
        shutil.copyfile(source, destination)
        destination.chmod(0o755)

    for license_name in LICENSE_FILES:
        source = repository / license_name
        require_regular_file(source, license_name)
        destination = stage_root / license_name
        shutil.copyfile(source, destination)
        destination.chmod(0o644)

    version_path = stage_root / VERSION_FILE
    version_path.write_bytes(metadata)
    version_path.chmod(0o644)


def zip_info(name: str, mode: int, *, directory: bool) -> zipfile.ZipInfo:
    """Create normalized ZIP metadata for one member."""

    info = zipfile.ZipInfo(name, ZIP_TIMESTAMP)
    info.create_system = 3
    info.compress_type = zipfile.ZIP_DEFLATED
    file_type = stat.S_IFDIR if directory else stat.S_IFREG
    info.external_attr = ((file_type | mode) << 16) | (0x10 if directory else 0)
    return info


def write_zip(stage_root: Path, archive: Path, tag: str, target: Target) -> None:
    """Write one deterministic Windows ZIP package."""

    root_name = asset_stem(tag, target)
    with zipfile.ZipFile(
        archive,
        mode="w",
        compression=zipfile.ZIP_DEFLATED,
        compresslevel=9,
        strict_timestamps=True,
    ) as output:
        output.writestr(zip_info(f"{root_name}/", 0o755, directory=True), b"")
        for filename in expected_file_names(target):
            mode = 0o755 if filename in executable_names(target) else 0o644
            info = zip_info(f"{root_name}/{filename}", mode, directory=False)
            with (stage_root / filename).open("rb") as source, output.open(info, "w") as destination:
                shutil.copyfileobj(source, destination, length=1024 * 1024)


def tar_info(name: str, mode: int, *, directory: bool, size: int = 0) -> tarfile.TarInfo:
    """Create normalized tar metadata for one member."""

    info = tarfile.TarInfo(name)
    info.type = tarfile.DIRTYPE if directory else tarfile.REGTYPE
    info.mode = mode
    info.size = size
    info.mtime = 0
    info.uid = 0
    info.gid = 0
    info.uname = ""
    info.gname = ""
    return info


def write_tar_gz(stage_root: Path, archive: Path, tag: str, target: Target) -> None:
    """Write one deterministic Apple Silicon tar.gz package."""

    root_name = asset_stem(tag, target)
    with archive.open("wb") as raw_output:
        with gzip.GzipFile(
            filename="", mode="wb", fileobj=raw_output, compresslevel=9, mtime=0
        ) as compressed:
            with tarfile.open(
                fileobj=compressed, mode="w", format=tarfile.USTAR_FORMAT
            ) as output:
                output.addfile(
                    tar_info(f"{root_name}/", 0o755, directory=True)
                )
                for filename in expected_file_names(target):
                    source_path = stage_root / filename
                    source_metadata = require_regular_file(source_path, filename)
                    mode = 0o755 if filename in executable_names(target) else 0o644
                    info = tar_info(
                        f"{root_name}/{filename}",
                        mode,
                        directory=False,
                        size=source_metadata.st_size,
                    )
                    with source_path.open("rb") as source:
                        output.addfile(info, source)


def validate_member_name(name: str) -> None:
    """Reject absolute, traversal, backslash, or malformed archive member names."""

    if "\\" in name:
        raise ReleaseError(f"archive member uses a backslash: {name!r}")
    path = PurePosixPath(name)
    if path.is_absolute() or any(part in ("", ".", "..") for part in path.parts):
        raise ReleaseError(f"archive member is not a safe relative path: {name!r}")


def inspect_zip(
    archive: Path, target: Target, tag: str, expected_metadata: bytes
) -> None:
    """Verify ZIP layout, kinds, timestamps, permissions, and metadata."""

    try:
        with zipfile.ZipFile(archive) as package:
            members = package.infolist()
            names = tuple(member.filename for member in members)
            if len(names) != len(set(names)):
                raise ReleaseError(f"ZIP contains duplicate members: {archive}")
            if names != expected_member_names(tag, target):
                raise ReleaseError(
                    f"ZIP member set/order is {names!r}; expected {expected_member_names(tag, target)!r}"
                )
            for index, member in enumerate(members):
                validate_member_name(member.filename)
                expected_directory = index == 0
                if member.is_dir() != expected_directory:
                    raise ReleaseError(f"ZIP member has unexpected kind: {member.filename}")
                if member.date_time != ZIP_TIMESTAMP:
                    raise ReleaseError(f"ZIP member has non-normalized time: {member.filename}")
                mode = (member.external_attr >> 16) & 0o777
                filename = PurePosixPath(member.filename).name
                expected_mode = (
                    0o755
                    if expected_directory or filename in executable_names(target)
                    else 0o644
                )
                if mode != expected_mode:
                    raise ReleaseError(
                        f"ZIP member {member.filename} has mode {mode:o}; expected {expected_mode:o}"
                    )
            root = asset_stem(tag, target)
            if package.read(f"{root}/{VERSION_FILE}") != expected_metadata:
                raise ReleaseError("ZIP VERSION metadata does not match validated release values")
            for executable in executable_names(target):
                if package.getinfo(f"{root}/{executable}").file_size == 0:
                    raise ReleaseError(f"ZIP executable is empty: {executable}")
    except (OSError, zipfile.BadZipFile) as error:
        raise ReleaseError(f"cannot inspect ZIP archive {archive}: {error}") from error


def inspect_tar_gz(
    archive: Path, target: Target, tag: str, expected_metadata: bytes
) -> None:
    """Verify tar.gz layout, kinds, timestamps, permissions, and metadata."""

    try:
        with archive.open("rb") as compressed:
            header = compressed.read(10)
        if len(header) != 10 or header[:3] != b"\x1f\x8b\x08" or header[4:8] != b"\0\0\0\0":
            raise ReleaseError(f"gzip header is not normalized: {archive}")
        with tarfile.open(archive, mode="r:gz") as package:
            members = package.getmembers()
            names = tuple(
                f"{member.name.rstrip('/')}/" if member.isdir() else member.name
                for member in members
            )
            if len(names) != len(set(names)):
                raise ReleaseError(f"tar archive contains duplicate members: {archive}")
            if names != expected_member_names(tag, target):
                raise ReleaseError(
                    f"tar member set/order is {names!r}; expected {expected_member_names(tag, target)!r}"
                )
            for index, member in enumerate(members):
                validate_member_name(member.name)
                expected_directory = index == 0
                if member.isdir() != expected_directory:
                    raise ReleaseError(f"tar member has unexpected kind: {member.name}")
                if not expected_directory and not member.isreg():
                    raise ReleaseError(f"tar member is not a regular file: {member.name}")
                filename = PurePosixPath(member.name).name
                expected_mode = (
                    0o755
                    if expected_directory or filename in executable_names(target)
                    else 0o644
                )
                if member.mode != expected_mode:
                    raise ReleaseError(
                        f"tar member {member.name} has mode {member.mode:o}; expected {expected_mode:o}"
                    )
                if (
                    member.mtime != 0
                    or member.uid != 0
                    or member.gid != 0
                    or member.uname != ""
                    or member.gname != ""
                ):
                    raise ReleaseError(f"tar metadata is not normalized: {member.name}")
            root = asset_stem(tag, target)
            metadata_member = package.extractfile(f"{root}/{VERSION_FILE}")
            if metadata_member is None or metadata_member.read() != expected_metadata:
                raise ReleaseError("tar VERSION metadata does not match validated release values")
            for executable in executable_names(target):
                member = package.getmember(f"{root}/{executable}")
                if member.size == 0:
                    raise ReleaseError(f"tar executable is empty: {executable}")
    except (OSError, tarfile.TarError) as error:
        raise ReleaseError(f"cannot inspect tar.gz archive {archive}: {error}") from error


def inspect_archive(
    archive: Path,
    *,
    target: Target,
    version: str,
    tag: str,
    commit: str,
) -> None:
    """Verify one archive against the complete deterministic package contract."""

    expected_name = asset_name(tag, target)
    if archive.name != expected_name:
        raise ReleaseError(f"archive is named {archive.name!r}; expected {expected_name!r}")
    require_regular_file(archive, "release archive")
    metadata = version_metadata(version, tag, target, commit)
    if target.extension == ".zip":
        inspect_zip(archive, target, tag, metadata)
    elif target.extension == ".tar.gz":
        inspect_tar_gz(archive, target, tag, metadata)
    else:
        raise ReleaseError(f"unsupported archive extension {target.extension!r}")


def package_release(
    repository: Path,
    binary_directory: Path,
    output_directory: Path,
    *,
    target: Target,
    version: str,
    tag: str,
    commit: str,
) -> Path:
    """Stage, package, and reinspect one target archive."""

    repository = repository.resolve()
    binary_directory = binary_directory.resolve()
    output_directory.mkdir(parents=True, exist_ok=True)
    output_directory = output_directory.resolve()
    metadata = version_metadata(version, tag, target, commit)
    final_archive = output_directory / asset_name(tag, target)

    with tempfile.TemporaryDirectory(prefix=".release-stage-", dir=output_directory) as temporary:
        temporary_path = Path(temporary)
        stage_root = temporary_path / asset_stem(tag, target)
        stage_package(repository, binary_directory, stage_root, target, metadata)
        temporary_archive = temporary_path / final_archive.name
        if target.extension == ".zip":
            write_zip(stage_root, temporary_archive, tag, target)
        else:
            write_tar_gz(stage_root, temporary_archive, tag, target)
        inspect_archive(
            temporary_archive,
            target=target,
            version=version,
            tag=tag,
            commit=commit,
        )
        os.replace(temporary_archive, final_archive)

    inspect_archive(
        final_archive,
        target=target,
        version=version,
        tag=tag,
        commit=commit,
    )
    return final_archive


def sha256_file(path: Path) -> str:
    """Return a streaming SHA-256 digest for one regular file."""

    require_regular_file(path, path.name)
    digest = hashlib.sha256()
    with path.open("rb") as input_file:
        while chunk := input_file.read(1024 * 1024):
            digest.update(chunk)
    return digest.hexdigest()


def regular_files_below(root: Path) -> list[Path]:
    """List regular files below *root* without following links."""

    if not root.is_dir():
        raise ReleaseError(f"release input directory does not exist: {root}")
    files: list[Path] = []
    for directory, directory_names, file_names in os.walk(root, followlinks=False):
        directory_path = Path(directory)
        for name in directory_names:
            candidate = directory_path / name
            if candidate.is_symlink():
                raise ReleaseError(f"release input contains a directory link: {candidate}")
        for name in file_names:
            candidate = directory_path / name
            require_regular_file(candidate, "release input")
            files.append(candidate)
    return files


def checksum_text(archives: Sequence[Path]) -> str:
    """Return deterministic GNU-compatible SHA-256 checksum lines."""

    ordered = sorted(archives, key=lambda archive: archive.name)
    return "".join(f"{sha256_file(archive)}  {archive.name}\n" for archive in ordered)


def parse_checksum_file(path: Path) -> dict[str, str]:
    """Parse the exact checksum syntax produced by this release helper."""

    require_regular_file(path, CHECKSUM_FILE)
    try:
        lines = path.read_text(encoding="ascii").splitlines()
    except (OSError, UnicodeError) as error:
        raise ReleaseError(f"cannot read {CHECKSUM_FILE}: {error}") from error
    checksums: dict[str, str] = {}
    for line in lines:
        match = re.fullmatch(r"([0-9a-f]{64})  ([^/\\]+)", line)
        if match is None:
            raise ReleaseError(f"invalid {CHECKSUM_FILE} line: {line!r}")
        digest, filename = match.groups()
        if filename in checksums:
            raise ReleaseError(f"duplicate {CHECKSUM_FILE} entry: {filename}")
        checksums[filename] = digest
    if list(checksums) != sorted(checksums):
        raise ReleaseError(f"{CHECKSUM_FILE} entries are not sorted")
    return checksums


def aggregate_release(
    input_directory: Path,
    output_directory: Path,
    *,
    version: str,
    tag: str,
    commit: str,
) -> tuple[Path, ...]:
    """Require exactly three valid archives and assemble the verified asset set."""

    expected_names = set(expected_archive_names(tag))
    discovered: dict[str, Path] = {}
    for path in regular_files_below(input_directory):
        if path.name not in expected_names:
            raise ReleaseError(f"unexpected release artifact: {path}")
        if path.name in discovered:
            raise ReleaseError(f"duplicate release artifact named {path.name!r}")
        discovered[path.name] = path
    missing = expected_names.difference(discovered)
    if missing:
        raise ReleaseError(f"missing release artifacts: {', '.join(sorted(missing))}")
    if len(discovered) != len(TARGETS):
        raise ReleaseError(
            f"expected exactly {len(TARGETS)} release archives, found {len(discovered)}"
        )

    for target in TARGETS:
        inspect_archive(
            discovered[asset_name(tag, target)],
            target=target,
            version=version,
            tag=tag,
            commit=commit,
        )

    if output_directory.exists() and any(output_directory.iterdir()):
        raise ReleaseError(f"release output directory is not empty: {output_directory}")
    output_directory.mkdir(parents=True, exist_ok=True)
    output_archives: list[Path] = []
    for name in sorted(discovered):
        destination = output_directory / name
        shutil.copyfile(discovered[name], destination)
        output_archives.append(destination)
    (output_directory / CHECKSUM_FILE).write_text(
        checksum_text(output_archives), encoding="ascii", newline="\n"
    )
    verify_release_set(
        output_directory,
        version=version,
        tag=tag,
        commit=commit,
    )
    return tuple(output_archives)


def verify_release_set(
    directory: Path, *, version: str, tag: str, commit: str
) -> None:
    """Verify the exact four-file release set and every local checksum."""

    expected_archives = set(expected_archive_names(tag))
    expected_files = expected_archives | {CHECKSUM_FILE}
    files = regular_files_below(directory)
    relative_names = []
    for path in files:
        relative = path.relative_to(directory)
        if len(relative.parts) != 1:
            raise ReleaseError(f"release asset is not at the workspace root: {relative}")
        relative_names.append(path.name)
    if set(relative_names) != expected_files or len(relative_names) != len(expected_files):
        raise ReleaseError(
            f"release asset set is {sorted(relative_names)!r}; expected {sorted(expected_files)!r}"
        )

    for target in TARGETS:
        inspect_archive(
            directory / asset_name(tag, target),
            target=target,
            version=version,
            tag=tag,
            commit=commit,
        )
    checksums = parse_checksum_file(directory / CHECKSUM_FILE)
    if set(checksums) != expected_archives:
        raise ReleaseError(
            f"{CHECKSUM_FILE} covers {sorted(checksums)!r}; expected {sorted(expected_archives)!r}"
        )
    for filename, expected_digest in checksums.items():
        actual_digest = sha256_file(directory / filename)
        if actual_digest != expected_digest:
            raise ReleaseError(
                f"SHA-256 mismatch for {filename}: expected {expected_digest}, got {actual_digest}"
            )


def argument_parser() -> argparse.ArgumentParser:
    """Build the release-helper command-line parser."""

    parser = argparse.ArgumentParser(description=__doc__)
    subparsers = parser.add_subparsers(dest="command", required=True)

    preflight_parser = subparsers.add_parser("preflight")
    preflight_parser.add_argument("--repository", type=Path, default=Path.cwd())
    preflight_parser.add_argument("--event-name", required=True)
    preflight_parser.add_argument("--ref-name", required=True)
    preflight_parser.add_argument("--event-sha", required=True)
    preflight_parser.add_argument("--github-output", type=Path, required=True)

    source_parser = subparsers.add_parser("verify-publication-source")
    source_parser.add_argument("--repository", type=Path, default=Path.cwd())
    source_parser.add_argument("--commit", required=True)

    runner_parser = subparsers.add_parser("verify-runner")
    runner_parser.add_argument("--repository", type=Path, default=Path.cwd())
    runner_parser.add_argument("--toolchain", required=True)
    runner_parser.add_argument("--runner-arch", required=True)
    runner_parser.add_argument("--expected-runner-arch", required=True)
    runner_parser.add_argument("--expected-host", required=True)
    runner_parser.add_argument("--target", required=True)

    smoke_parser = subparsers.add_parser("smoke")
    smoke_parser.add_argument("--binary-directory", type=Path, required=True)
    smoke_parser.add_argument("--target", required=True)
    smoke_parser.add_argument("--version", required=True)

    package_parser = subparsers.add_parser("package")
    package_parser.add_argument("--repository", type=Path, default=Path.cwd())
    package_parser.add_argument("--binary-directory", type=Path, required=True)
    package_parser.add_argument("--output-directory", type=Path, required=True)
    package_parser.add_argument("--target", required=True)
    package_parser.add_argument("--version", required=True)
    package_parser.add_argument("--tag", required=True)
    package_parser.add_argument("--commit", required=True)

    aggregate_parser = subparsers.add_parser("aggregate")
    aggregate_parser.add_argument("--input-directory", type=Path, required=True)
    aggregate_parser.add_argument("--output-directory", type=Path, required=True)
    aggregate_parser.add_argument("--version", required=True)
    aggregate_parser.add_argument("--tag", required=True)
    aggregate_parser.add_argument("--commit", required=True)

    verify_parser = subparsers.add_parser("verify-set")
    verify_parser.add_argument("--directory", type=Path, required=True)
    verify_parser.add_argument("--version", required=True)
    verify_parser.add_argument("--tag", required=True)
    verify_parser.add_argument("--commit", required=True)

    return parser


def run(arguments: Sequence[str]) -> int:
    """Run one release-helper subcommand."""

    parser = argument_parser()
    options = parser.parse_args(arguments)
    if options.command == "preflight":
        result = preflight(
            options.repository,
            event_name=options.event_name,
            ref_name=options.ref_name,
            event_sha=options.event_sha,
        )
        outputs = workflow_outputs(result)
        append_github_outputs(options.github_output, outputs)
        print(
            f"Validated {result.tag} at {result.commit}; "
            f"publish={str(result.publish).lower()}"
        )
        return 0
    if options.command == "verify-publication-source":
        verify_publication_source(options.repository, options.commit)
        print(f"Validated current main publication compatibility for {options.commit}")
        return 0
    if options.command == "verify-runner":
        evidence = rustc_runner_evidence(
            options.repository,
            toolchain=options.toolchain,
            runner_arch=options.runner_arch,
            expected_runner_arch=options.expected_runner_arch,
            expected_host=options.expected_host,
            target=options.target,
        )
        print(evidence, end="" if evidence.endswith("\n") else "\n")
        print(f"Validated runner target {options.target}")
        return 0
    if options.command == "smoke":
        target = target_for(options.target)
        smoke_pair(options.binary_directory, target, options.version)
        print(f"Validated executable parity for {target.triple}")
        return 0
    if options.command == "package":
        target = target_for(options.target)
        archive = package_release(
            options.repository,
            options.binary_directory,
            options.output_directory,
            target=target,
            version=options.version,
            tag=options.tag,
            commit=options.commit,
        )
        print(archive)
        return 0
    if options.command == "aggregate":
        archives = aggregate_release(
            options.input_directory,
            options.output_directory,
            version=options.version,
            tag=options.tag,
            commit=options.commit,
        )
        print(f"Verified {len(archives)} release archives and {CHECKSUM_FILE}")
        return 0
    if options.command == "verify-set":
        verify_release_set(
            options.directory,
            version=options.version,
            tag=options.tag,
            commit=options.commit,
        )
        print(f"Verified complete release set in {options.directory}")
        return 0
    raise ReleaseError(f"unhandled release command {options.command!r}")


def main(arguments: Sequence[str] | None = None) -> int:
    """Convert release invariant failures into a stable nonzero status."""

    try:
        return run(sys.argv[1:] if arguments is None else arguments)
    except (OSError, ReleaseError, UnicodeError, json.JSONDecodeError) as error:
        print(f"release validation failed: {error}", file=sys.stderr)
        return 1


if __name__ == "__main__":
    raise SystemExit(main())
