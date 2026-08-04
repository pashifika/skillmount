#!/usr/bin/env python3
"""Fetch exact native agent packages, verify committed SRI, and extract allowlisted runtime files."""

from __future__ import annotations

import argparse
import base64
import hashlib
import hmac
import json
import shutil
import subprocess
import tarfile
import tempfile
from dataclasses import asdict, dataclass
from pathlib import Path


@dataclass(frozen=True)
class Package:
    agent: str
    spec: str
    package_name: str
    version: str
    integrity: str
    archive_root: str
    members: tuple[str, ...]
    executable: str


PACKAGES = {
    "macos-arm64": (
        Package(
            agent="codex",
            spec="@openai/codex@0.146.0-darwin-arm64",
            package_name="@openai/codex",
            version="0.146.0-darwin-arm64",
            integrity="sha512-nb61yX4r5L6Z0dlC4o3u0GAK1YCd4TUvjaB382bajDoh84V+uv2hTBIVZ++fgXWV9yoeuNrNnNcn7GoTGOe2Tg==",
            archive_root="package/vendor/aarch64-apple-darwin/",
            members=(
                "package/vendor/aarch64-apple-darwin/bin/codex",
                "package/vendor/aarch64-apple-darwin/bin/codex-code-mode-host",
                "package/vendor/aarch64-apple-darwin/codex-package.json",
                "package/vendor/aarch64-apple-darwin/codex-path/rg",
                "package/vendor/aarch64-apple-darwin/codex-resources/zsh/bin/zsh",
            ),
            executable="codex/bin/codex",
        ),
        Package(
            agent="claude",
            spec="@anthropic-ai/claude-code-darwin-arm64@2.1.220",
            package_name="@anthropic-ai/claude-code-darwin-arm64",
            version="2.1.220",
            integrity="sha512-rmtd41Bf+n+YnhjSjtQ8WG5qy8KKogUp3YRfQrkLsTgPUD0H3j869rBInBJT3SHrKQ0hLghQLGM73CC1C+USLQ==",
            archive_root="package/",
            members=("package/claude",),
            executable="claude/claude",
        ),
    ),
    "windows-x64": (
        Package(
            agent="codex",
            spec="@openai/codex@0.146.0-win32-x64",
            package_name="@openai/codex",
            version="0.146.0-win32-x64",
            integrity="sha512-b3lxMYeR0+IhstNo4JjX1P9cPc1xwVcCVkPd1lD1wpWPJ0SBhpIkPczwbu3ZRkJcdyl342+rgyf4DUrbZLdrGA==",
            archive_root="package/vendor/x86_64-pc-windows-msvc/",
            members=(
                "package/vendor/x86_64-pc-windows-msvc/bin/codex.exe",
                "package/vendor/x86_64-pc-windows-msvc/bin/codex-code-mode-host.exe",
                "package/vendor/x86_64-pc-windows-msvc/codex-package.json",
                "package/vendor/x86_64-pc-windows-msvc/codex-path/rg.exe",
                "package/vendor/x86_64-pc-windows-msvc/codex-resources/codex-command-runner.exe",
                "package/vendor/x86_64-pc-windows-msvc/codex-resources/codex-windows-sandbox-setup.exe",
            ),
            executable="codex/bin/codex.exe",
        ),
        Package(
            agent="claude",
            spec="@anthropic-ai/claude-code-win32-x64@2.1.220",
            package_name="@anthropic-ai/claude-code-win32-x64",
            version="2.1.220",
            integrity="sha512-UGrjH8cGhC6PzhTyZSdgf/RpKxpfk9XJZ/RT/wsG2AJg9yEJLjLg6/TrnlL8RFbEv6Zahu0Quytc02UOpA/GiA==",
            archive_root="package/",
            members=("package/claude.exe",),
            executable="claude/claude.exe",
        ),
    ),
}


def parse_args() -> argparse.Namespace:
    parser = argparse.ArgumentParser()
    parser.add_argument("--platform", required=True, choices=tuple(PACKAGES))
    parser.add_argument("--output-dir", required=True, type=Path)
    return parser.parse_args()


def file_digest(path: Path, algorithm: str) -> str:
    digest = hashlib.new(algorithm)
    with path.open("rb") as stream:
        for block in iter(lambda: stream.read(1024 * 1024), b""):
            digest.update(block)
    return digest.hexdigest()


def sri(path: Path) -> str:
    digest = hashlib.sha512()
    with path.open("rb") as stream:
        for block in iter(lambda: stream.read(1024 * 1024), b""):
            digest.update(block)
    return "sha512-" + base64.b64encode(digest.digest()).decode("ascii")


def verify_archive(path: Path, expected: str) -> None:
    observed = sri(path)
    if not hmac.compare_digest(observed, expected):
        raise RuntimeError(
            f"package integrity mismatch for {path.name}: expected {expected}, observed {observed}"
        )


def extract_regular_files(archive: Path, destinations: dict[str, Path]) -> None:
    with tarfile.open(archive, mode="r:gz") as bundle:
        members = []
        for member_name, destination in destinations.items():
            try:
                member = bundle.getmember(member_name)
            except KeyError as error:
                raise RuntimeError(
                    f"verified package does not contain {member_name}"
                ) from error
            if not member.isfile() or member.issym() or member.islnk():
                raise RuntimeError(
                    f"verified package member {member_name} is not a regular file"
                )
            members.append((member, destination))
        for member, destination in sorted(members, key=lambda item: item[0].offset_data):
            source = bundle.extractfile(member)
            if source is None:
                raise RuntimeError(f"verified package member {member.name} cannot be read")
            destination.parent.mkdir(parents=True, exist_ok=True)
            with source, destination.open("xb") as output:
                while block := source.read(1024 * 1024):
                    output.write(block)
            destination.chmod(0o755 if member.mode & 0o111 else 0o644)


def extract_regular_file(archive: Path, member_name: str, destination: Path) -> None:
    extract_regular_files(archive, {member_name: destination})


def fetch(package: Package, output_dir: Path, download_dir: Path) -> dict[str, object]:
    npm = shutil.which("npm")
    if npm is None:
        raise RuntimeError("npm is not installed on PATH")
    completed = subprocess.run(
        [
            npm,
            "pack",
            package.spec,
            "--ignore-scripts",
            "--json",
            "--pack-destination",
            str(download_dir),
        ],
        check=False,
        capture_output=True,
        text=True,
        timeout=300,
    )
    if completed.returncode != 0:
        raise RuntimeError(
            f"npm pack failed for {package.spec} with exit {completed.returncode}"
        )
    try:
        metadata = json.loads(completed.stdout)
    except json.JSONDecodeError as error:
        raise RuntimeError(f"npm pack returned invalid metadata for {package.spec}") from error
    if isinstance(metadata, list):
        records = metadata
    elif isinstance(metadata, dict):
        records = list(metadata.values())
    else:
        raise RuntimeError(f"npm pack returned invalid metadata for {package.spec}")
    if len(records) != 1:
        raise RuntimeError(f"npm pack returned multiple packages for {package.spec}")
    record = records[0]
    if not isinstance(record, dict):
        raise RuntimeError(f"npm pack returned invalid metadata for {package.spec}")
    observed = (record.get("name"), record.get("version"), record.get("integrity"))
    expected = (package.package_name, package.version, package.integrity)
    if observed != expected:
        raise RuntimeError(
            f"npm metadata mismatch for {package.spec}: expected {expected!r}, observed {observed!r}"
        )
    filename = record.get("filename")
    if not isinstance(filename, str) or Path(filename).name != filename:
        raise RuntimeError(f"npm returned an unsafe archive name for {package.spec}")
    archive = (download_dir / filename).resolve(strict=True)
    if archive.parent != download_dir.resolve():
        raise RuntimeError(
            f"npm returned an archive outside the download directory for {package.spec}"
        )
    verify_archive(archive, package.integrity)
    destinations = {}
    for member in package.members:
        if not member.startswith(package.archive_root):
            raise RuntimeError(f"allowlisted member escapes its package root: {member}")
        relative = Path(member.removeprefix(package.archive_root))
        if relative.is_absolute() or ".." in relative.parts:
            raise RuntimeError(f"allowlisted member has an unsafe relative path: {member}")
        destinations[member] = output_dir / package.agent / relative
    extract_regular_files(archive, destinations)
    executable = output_dir / package.executable
    return {
        **asdict(package),
        "archive_sha256": file_digest(archive, "sha256"),
        "binary_sha256": file_digest(executable, "sha256"),
    }


def main() -> int:
    args = parse_args()
    output_dir = args.output_dir.resolve()
    output_dir.mkdir(parents=True, exist_ok=False)
    with tempfile.TemporaryDirectory(prefix="skillmount-agent-download-") as temporary:
        download_dir = Path(temporary).resolve()
        records = [
            fetch(package, output_dir, download_dir) for package in PACKAGES[args.platform]
        ]
    manifest = {
        "schema": 1,
        "platform": args.platform,
        "packages": records,
    }
    (output_dir / "manifest.json").write_text(
        json.dumps(manifest, indent=2, sort_keys=True) + "\n", encoding="utf-8"
    )
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
