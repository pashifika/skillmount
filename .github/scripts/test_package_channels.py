#!/usr/bin/env python3
"""Behavior tests for SkillMount package-channel identity, preflight, and inspection."""

from __future__ import annotations

import contextlib
import copy
import hashlib
import io
import json
import os
import shutil
import tempfile
import unittest
import zipfile
from pathlib import Path
from typing import Any

import package_channels as channels
import release

REPOSITORY = "pashifika/skillmount"
VERSION = "0.2.0"
TAG = "v0.2.0"
COMMIT = "b" * 40
OTHER_COMMIT = "c" * 40
RELEASE_URL = f"https://github.com/{REPOSITORY}/releases/tag/{TAG}"
ANNOTATED_TAG_OBJECT = "51f06711667411cdaea8c7755032645d61e695a4"
ANNOTATED_TAG_COMMIT = "6814cddcbf70a8d6006a4d3ce96c9280ad73a076"
RUN_ID = 4242

FORMULA_TEMPLATE = '''# @PACKAGE_ID@ generated from tag @TAG@ at commit @COMMIT@
class @FORMULA_CLASS@ < Formula
  desc "@DESCRIPTION@"
  homepage "@HOMEPAGE@"
  url "@ARCHIVE_URL@"
  sha256 "@ARCHIVE_SHA256@"
  license @LICENSE@

  depends_on :macos
  depends_on arch: :arm64

  def install
    bin.install "@COMMAND@"
    pkgshare.install "LICENSE-APACHE", "LICENSE-MIT", "VERSION"
    generate_completions_from_executable(
      bin/"@COMMAND@", "completions", base_name: "@COMMAND@", shells: [:bash, :zsh, :fish]
    )
  end

  test do
    assert_match "@VERSION@", shell_output("#{bin}/@COMMAND@ --version")
    refute_predicate bin/"@OTHER_COMMAND@", :exist?
  end
end
'''

NUSPEC_TEMPLATE = '''<?xml version="1.0" encoding="utf-8"?>
<package xmlns="http://schemas.microsoft.com/packaging/2015/06/nuspec.xsd">
  <metadata>
    <id>@PACKAGE_ID@</id>
    <version>@VERSION@</version>
    <title>@TITLE@</title>
    <authors>pashifika</authors>
    <projectUrl>@PROJECT_URL@</projectUrl>
    <projectSourceUrl>@PROJECT_SOURCE_URL@</projectSourceUrl>
    <licenseUrl>@LICENSE_URL@</licenseUrl>
    <requireLicenseAcceptance>false</requireLicenseAcceptance>
    <releaseNotes>@RELEASE_NOTES_URL@</releaseNotes>
    <summary>@SUMMARY@</summary>
    <description>@DESCRIPTION@ Installs the @COMMAND@ command built at @TAG@.</description>
  </metadata>
  <files>
    <file src="tools/**" target="tools" />
  </files>
</package>
'''

INSTALL_TEMPLATE = """$ErrorActionPreference = 'Stop'
Set-StrictMode -Version 2

$packageId = '@PACKAGE_ID@'
$packageVersion = '@VERSION@'
$releaseTag = '@TAG@'
$command = '@COMMAND@'
$selectedExecutable = '@SELECTED_EXECUTABLE@'
$otherExecutable = '@OTHER_EXECUTABLE@'
$url32 = '@URL_X86@'
$checksum32 = '@SHA256_X86@'
$url64 = '@URL_X64@'
$checksum64 = '@SHA256_X64@'
$root32 = '@ARCHIVE_ROOT_X86@'
$root64 = '@ARCHIVE_ROOT_X64@'

$toolsDir = Split-Path -Parent $MyInvocation.MyCommand.Definition
Write-Host "Installing $packageId $packageVersion ($releaseTag) as $command"
Write-Host "Retaining $selectedExecutable and dropping $otherExecutable"
Write-Host "Expecting $root32 from $url32 ($checksum32) or $root64 from $url64 ($checksum64)"
Write-Host "Package tools live in $toolsDir"
"""

UNINSTALL_TEMPLATE = """$ErrorActionPreference = 'Stop'
Set-StrictMode -Version 2

Write-Host "Removing @PACKAGE_ID@ shim for @COMMAND@ (@SELECTED_EXECUTABLE@)"
"""

CONTENT_TYPES_XML = b'<?xml version="1.0" encoding="utf-8"?><Types/>'
RELS_XML = b'<?xml version="1.0" encoding="utf-8"?><Relationships/>'
PSMDCP_MEMBER = "package/services/metadata/core-properties/0a1b2c3d4e5f.psmdcp"
PSMDCP_XML = b'<?xml version="1.0" encoding="utf-8"?><coreProperties/>'


def manifest_text(version: str) -> str:
    """Return a Cargo manifest fixture declaring one package version."""

    return (
        "[package]\n"
        'name = "skillmount"\n'
        f'version = "{version}"\n'
        'edition = "2024"\n'
        "\n"
        "[[bin]]\n"
        'name = "asm"\n'
        'path = "src/bin/asm.rs"\n'
    )


def lock_text(version: str) -> str:
    """Return a Cargo lockfile fixture recording one product package version."""

    return (
        "version = 4\n"
        "\n"
        "[[package]]\n"
        'name = "clap"\n'
        'version = "4.5.48"\n'
        "\n"
        "[[package]]\n"
        'name = "skillmount"\n'
        f'version = "{version}"\n'
        "dependencies = [\n"
        ' "clap",\n'
        "]\n"
    )


def digest_for(target: release.Target) -> str:
    """Return a deterministic distinct digest fixture for one target."""

    return hashlib.sha256(target.triple.encode()).hexdigest()


def archive_identity(target: release.Target) -> channels.ArchiveIdentity:
    """Return one canonical archive identity fixture."""

    name = release.asset_name(TAG, target)
    return channels.ArchiveIdentity(
        triple=target.triple,
        name=name,
        url=channels.asset_download_url(REPOSITORY, TAG, name),
        sha256=digest_for(target),
    )


def canonical_inputs() -> channels.PackageInputs:
    """Return a complete valid preflight result fixture."""

    return channels.PackageInputs(
        repository=REPOSITORY,
        version=VERSION,
        tag=TAG,
        commit=COMMIT,
        release_url=RELEASE_URL,
        archives=tuple(
            archive_identity(target)
            for target in sorted(release.TARGETS, key=lambda item: item.triple)
        ),
    )


def canonical_document() -> dict[str, Any]:
    """Return the canonical artifact as a mutable JSON document."""

    return json.loads(canonical_inputs().to_json())


def template_tokens(path: Path) -> list[str]:
    """Return every token one template references."""

    return sorted(set(channels.TOKEN_PATTERN.findall(path.read_text(encoding="utf-8"))))


def write_templates(root: Path, *, uninstall: bool = False) -> Path:
    """Write minimal channel templates that use exactly the contract token sets."""

    homebrew = root / "homebrew"
    homebrew.mkdir(parents=True)
    for identity in channels.PACKAGES:
        (homebrew / f"{identity.package_id}.rb.in").write_text(
            FORMULA_TEMPLATE, encoding="utf-8"
        )
        package = root / "chocolatey" / identity.package_id
        tools = package / "tools"
        tools.mkdir(parents=True)
        (package / f"{identity.package_id}.nuspec.in").write_text(
            NUSPEC_TEMPLATE, encoding="utf-8"
        )
        (tools / "chocolateyinstall.ps1.in").write_text(INSTALL_TEMPLATE, encoding="utf-8")
        if uninstall:
            (tools / "chocolateyuninstall.ps1.in").write_text(
                UNINSTALL_TEMPLATE, encoding="utf-8"
            )
    return root


def write_licenses(root: Path) -> Path:
    """Write both license fixtures a Chocolatey package must ship."""

    root.mkdir(parents=True, exist_ok=True)
    for name in release.LICENSE_FILES:
        (root / name).write_text(f"{name} fixture\n", encoding="utf-8")
    return root


def pack_nupkg(
    root: Path,
    identity: channels.PackageIdentity,
    destination: Path,
    *,
    version: str = VERSION,
    extra: dict[str, bytes] | None = None,
    omit: tuple[str, ...] = (),
    name: str | None = None,
) -> Path:
    """Pack one generated package source into a NuGet-shaped ZIP without running choco."""

    destination.mkdir(parents=True, exist_ok=True)
    path = destination / (name or channels.nupkg_name(identity, version))
    entries: dict[str, bytes] = {
        member: (root / member).read_bytes()
        for member in channels.chocolatey_member_names(root)
    }
    entries[channels.CONTENT_TYPES_MEMBER] = CONTENT_TYPES_XML
    entries[channels.RELS_MEMBER] = RELS_XML
    entries[PSMDCP_MEMBER] = PSMDCP_XML
    for member in omit:
        entries.pop(member, None)
    entries.update(extra or {})
    with zipfile.ZipFile(path, "w") as package:
        for member in sorted(entries):
            package.writestr(member, entries[member])
    return path


def build_release_assets(root: Path) -> Path:
    """Build the complete deterministic three-archive release set on disk."""

    repository = root / "repository"
    repository.mkdir(parents=True)
    for name in release.LICENSE_FILES:
        (repository / name).write_text(f"{name} fixture\n", encoding="utf-8")
    artifacts = root / "artifacts"
    for target in release.TARGETS:
        binaries = root / "binaries" / target.triple
        binaries.mkdir(parents=True)
        for executable in release.executable_names(target):
            binary = binaries / executable
            binary.write_bytes(f"binary:{target.triple}:{executable}\n".encode())
            binary.chmod(0o755)
        release.package_release(
            repository,
            binaries,
            artifacts / target.name,
            target=target,
            version=VERSION,
            tag=TAG,
            commit=COMMIT,
        )
    assets = root / "assets"
    release.aggregate_release(artifacts, assets, version=VERSION, tag=TAG, commit=COMMIT)
    return assets


def release_payload(assets: Path, *, tag: str = TAG, commit: str = COMMIT) -> dict[str, Any]:
    """Return a GitHub release payload describing the local asset fixture."""

    return {
        "tag_name": tag,
        "draft": False,
        "prerelease": False,
        "target_commitish": commit,
        "html_url": RELEASE_URL,
        "assets": [
            {
                "name": path.name,
                "state": "uploaded",
                "size": path.stat().st_size,
                "digest": f"sha256:{release.sha256_file(path)}",
                "browser_download_url": channels.asset_download_url(
                    REPOSITORY, tag, path.name
                ),
            }
            for path in sorted(assets.iterdir())
        ],
    }


def workflow_run_payload(**overrides: Any) -> dict[str, Any]:
    """Return a successful tag-push Release run payload with optional overrides."""

    payload: dict[str, Any] = {
        "id": RUN_ID,
        "name": channels.RELEASE_WORKFLOW_NAME,
        "path": channels.RELEASE_WORKFLOW_PATH,
        "event": "push",
        "status": "completed",
        "conclusion": "success",
        "head_branch": TAG,
        "head_sha": COMMIT,
        "head_repository": {"full_name": REPOSITORY},
    }
    payload.update(overrides)
    return payload


class FakeGateway:
    """In-memory read-only GitHub boundary over a real local release fixture."""

    def __init__(self, assets: Path) -> None:
        self.assets = assets
        self.commit = COMMIT
        self.contained = True
        self.files = {
            "Cargo.toml": manifest_text(VERSION).encode(),
            "Cargo.lock": lock_text(VERSION).encode(),
        }
        self.release = release_payload(assets)
        self.downloads: list[str] = []
        self.dereferenced = 0

    def workflow_run(self, repository: str, run_id: int) -> dict[str, Any]:
        raise AssertionError("preflight must not reread the triggering run")

    def dereference_tag(self, repository: str, tag: str) -> str:
        self.assert_identity(repository, tag)
        self.dereferenced += 1
        return self.commit

    def release_for_tag(self, repository: str, tag: str) -> dict[str, Any]:
        self.assert_identity(repository, tag)
        return copy.deepcopy(self.release)

    def commit_contained_in_default_branch(self, repository: str, commit: str) -> bool:
        self.assert_repository(repository)
        if commit != self.commit:
            raise AssertionError(f"unexpected ancestry probe for {commit}")
        return self.contained

    def file_at_commit(self, repository: str, commit: str, path: str) -> bytes:
        self.assert_repository(repository)
        if commit != self.commit:
            raise AssertionError(f"unexpected file read at {commit}")
        try:
            return self.files[path]
        except KeyError as error:
            raise AssertionError(f"unexpected repository file {path}") from error

    def download(self, url: str, destination: Path) -> None:
        self.downloads.append(url)
        destination.parent.mkdir(parents=True, exist_ok=True)
        prefix = f"https://github.com/{REPOSITORY}/releases/download/{TAG}/"
        if not url.startswith(prefix):
            raise AssertionError(f"unexpected download {url}")
        candidate = self.assets / url[len(prefix) :]
        if not candidate.is_file():
            raise AssertionError(f"unexpected asset download {url}")
        shutil.copyfile(candidate, destination)

    def refresh_release(self) -> None:
        """Rebuild the payload so only the tampered value under test disagrees."""

        self.release = release_payload(self.assets)

    @staticmethod
    def assert_repository(repository: str) -> None:
        if repository != REPOSITORY:
            raise AssertionError(f"unexpected repository {repository}")

    @classmethod
    def assert_identity(cls, repository: str, tag: str) -> None:
        cls.assert_repository(repository)
        if tag != TAG:
            raise AssertionError(f"unexpected tag {tag}")


class ApiStub(channels.GhReleaseGateway):
    """Real GitHub CLI adapter logic driven by canned API payloads."""

    def __init__(self, responses: dict[str, Any]) -> None:
        self.working_directory = Path.cwd()
        self.responses = responses
        self.requests: list[str] = []
        self._default_branches = {}

    def _api(self, endpoint: str) -> Any:
        self.requests.append(endpoint)
        if endpoint not in self.responses:
            raise AssertionError(f"unexpected endpoint {endpoint}")
        return self.responses[endpoint]


class IdentityTests(unittest.TestCase):
    """Cover the immutable ordered selection map."""

    def test_pair_is_ordered_and_selects_distinct_executables(self) -> None:
        """Keep skillmount first and skillmount-asm second everywhere."""

        self.assertEqual(
            [identity.package_id for identity in channels.PACKAGES],
            ["skillmount", "skillmount-asm"],
        )
        first, second = channels.PACKAGES
        self.assertEqual((first.command, second.command), ("skillmount", "asm"))
        self.assertEqual(first.formula_path, "Formula/skillmount.rb")
        self.assertEqual(second.formula_path, "Formula/skillmount-asm.rb")
        self.assertEqual(first.windows_executable, "skillmount.exe")
        self.assertEqual(second.windows_executable, "asm.exe")
        self.assertEqual(first.formula_class, "Skillmount")
        self.assertEqual(second.formula_class, "SkillmountAsm")
        self.assertIs(first.other, second)
        self.assertIs(second.other, first)
        self.assertEqual(channels.PACKAGE_BY_ID, {p.package_id: p for p in channels.PACKAGES})

    def test_descriptions_stay_within_the_homebrew_audit_budget(self) -> None:
        """Keep the shared description token short enough for `brew audit --strict`."""

        for identity in channels.PACKAGES:
            with self.subTest(package=identity.package_id):
                self.assertLessEqual(len(identity.description), 80)
                self.assertNotIn("\n", identity.summary)

    def test_unknown_package_id_is_rejected(self) -> None:
        """Refuse a package identity this product does not publish."""

        with self.assertRaises(channels.ChannelError):
            channels.package_for("skillmount-cli")

    def test_selection_map_is_deterministic(self) -> None:
        """Print exactly two stable selection lines."""

        lines = channels.selection_map_lines()
        self.assertEqual(lines, channels.selection_map_lines())
        self.assertEqual(len(lines), 2)
        self.assertTrue(lines[0].startswith("skillmount command=skillmount"))
        self.assertTrue(lines[1].startswith("skillmount-asm command=asm"))

    def test_release_targets_are_taken_from_the_release_matrix(self) -> None:
        """Derive package targets from release.TARGETS instead of restating them."""

        self.assertIn(channels.MACOS_ARM64, release.TARGETS)
        self.assertIn(channels.WINDOWS_X64, release.TARGETS)
        self.assertIn(channels.WINDOWS_X86, release.TARGETS)
        self.assertNotEqual(channels.WINDOWS_X64.triple, channels.WINDOWS_X86.triple)
        with self.assertRaises(channels.ChannelError):
            channels.target_named("linux-x64")


class PackageInputsTests(unittest.TestCase):
    """Cover artifact serialization and untrusted-artifact revalidation."""

    def test_round_trip_preserves_every_value(self) -> None:
        """Serialize and rebuild inputs without losing or reordering a field."""

        inputs = canonical_inputs()
        document = inputs.to_json()
        self.assertTrue(document.endswith("\n"))
        self.assertEqual(json.loads(document)["schema"], channels.INPUTS_SCHEMA)
        self.assertEqual(channels.PackageInputs.from_json(document), inputs)
        self.assertEqual(channels.PackageInputs.from_json(document).to_json(), document)
    def test_serialized_inputs_contain_only_the_validated_release_identity(self) -> None:
        """Do not retain a second generated-source artifact beside the release assets."""

        document = json.loads(canonical_inputs().to_json())
        self.assertEqual(document["schema"], 2)
        self.assertNotIn("source_url", document)
        self.assertNotIn("source_sha256", document)
        self.assertEqual(
            [archive["triple"] for archive in document["archives"]],
            sorted(target.triple for target in release.TARGETS),
        )


    def test_archive_lookup_names_recorded_targets(self) -> None:
        """Return the requested archive and refuse an unrecorded target."""

        inputs = canonical_inputs()
        archive = inputs.archive(channels.WINDOWS_X64.triple)
        self.assertEqual(archive.name, release.asset_name(TAG, channels.WINDOWS_X64))
        with self.assertRaises(channels.ChannelError):
            inputs.archive("x86_64-unknown-linux-gnu")

    def test_partial_archive_sets_are_constructible_for_local_harnesses(self) -> None:
        """Allow in-process target fixtures while keeping artifact completeness strict."""

        windows = tuple(
            archive_identity(target)
            for target in sorted(
                (channels.WINDOWS_X86, channels.WINDOWS_X64), key=lambda item: item.triple
            )
        )
        inputs = channels.PackageInputs(
            repository=REPOSITORY,
            version=VERSION,
            tag=TAG,
            commit=COMMIT,
            release_url=RELEASE_URL,
            archives=windows,
        )
        self.assertEqual(len(inputs.archives), 2)
        with self.assertRaises(channels.ChannelError):
            channels.PackageInputs.from_json(inputs.to_json())

        empty = channels.PackageInputs(
            repository=REPOSITORY,
            version=VERSION,
            tag=TAG,
            commit=COMMIT,
            release_url=RELEASE_URL,
            archives=(),
        )
        self.assertEqual(empty.archives, ())
        with self.assertRaises(channels.ChannelError):
            empty.archive(channels.WINDOWS_X64.triple)
        with self.assertRaises(channels.ChannelError):
            channels.PackageInputs.from_json(empty.to_json())

    def test_structural_construction_failures(self) -> None:
        """Reject a structurally impossible identity at construction time."""

        base = canonical_inputs()
        replacements: dict[str, dict[str, Any]] = {
            "repository": {"repository": "skillmount"},
            "version": {"version": "0.2"},
            "tag": {"tag": "v0.3.0"},
            "commit": {"commit": "b" * 39},
            "empty release url": {"release_url": ""},
            "duplicate triples": {"archives": (base.archives[0], base.archives[0])},
            "unsorted archives": {"archives": tuple(reversed(base.archives))},
            "unsupported triple": {
                "archives": (
                    channels.ArchiveIdentity(
                        triple="x86_64-unknown-linux-gnu",
                        name="skillmount-v0.2.0-x86_64-unknown-linux-gnu.tar.gz",
                        url="https://github.com/pashifika/skillmount/releases/download/v0.2.0/x",
                        sha256="e" * 64,
                    ),
                )
            },
            "renamed archive": {
                "archives": (
                    channels.ArchiveIdentity(
                        triple=channels.WINDOWS_X64.triple,
                        name="skillmount.zip",
                        url=channels.asset_download_url(REPOSITORY, TAG, "skillmount.zip"),
                        sha256="f" * 64,
                    ),
                )
            },
        }
        for description, replacement in replacements.items():
            with self.subTest(case=description):
                values = {
                    "repository": base.repository,
                    "version": base.version,
                    "tag": base.tag,
                    "commit": base.commit,
                    "release_url": base.release_url,
                    "archives": base.archives,
                }
                values.update(replacement)
                with self.assertRaises(channels.ChannelError):
                    channels.PackageInputs(**values)

    def test_tag_and_version_must_agree(self) -> None:
        """Refuse an artifact whose tag does not derive its version."""

        document = canonical_document()
        document["version"] = "0.3.0"
        with self.assertRaisesRegex(channels.ChannelError, "expected '0.3.0'"):
            channels.PackageInputs.from_json(json.dumps(document))

    def test_from_json_rejections(self) -> None:
        """Reject every way a downstream artifact can be tampered with."""

        def without(key: str) -> str:
            document = canonical_document()
            document.pop(key)
            return json.dumps(document)

        def replace(**values: Any) -> str:
            document = canonical_document()
            document.update(values)
            return json.dumps(document)

        def with_archives(mutate: Any) -> str:
            document = canonical_document()
            mutate(document["archives"])
            return json.dumps(document)

        foreign = "https://github.com/attacker/skillmount"
        cases = {
            "not json": "{",
            "not an object": "[]",
            "missing schema": without("schema"),
            "wrong schema": replace(schema=1),
            "missing commit": without("commit"),
            "extra key": replace(published="yes"),
            "stale source identity": replace(source_url="https://example.test/source.tar.gz"),
            "non-string tag": replace(tag=2),
            "non-string commit": replace(commit=None),
            "malformed tag": replace(tag="0.2.0", version="0.2.0"),
            "prerelease tag": replace(tag="v0.2.0-rc.1"),
            "short commit": replace(commit="b" * 39),
            "uppercase commit": replace(commit="B" * 40),
            "archives not a list": replace(archives={}),
            "foreign release url": replace(release_url=f"{foreign}/releases/tag/{TAG}"),
            "release url outside releases": replace(
                release_url=f"https://github.com/{REPOSITORY}/blob/{TAG}/README.md"
            ),
            "insecure release url": replace(
                release_url=f"http://github.com/{REPOSITORY}/releases/tag/{TAG}"
            ),
            "missing archive": with_archives(lambda archives: archives.pop()),
            "duplicate archive": with_archives(
                lambda archives: archives.append(copy.deepcopy(archives[0]))
            ),
            "unsorted archives": with_archives(lambda archives: archives.reverse()),
            "renamed archive": with_archives(
                lambda archives: archives[0].update({"name": "skillmount.zip"})
            ),
            "foreign archive url": with_archives(
                lambda archives: archives[0].update(
                    {"url": f"{foreign}/releases/download/{TAG}/{archives[0]['name']}"}
                )
            ),
            "archive digest tampered": with_archives(
                lambda archives: archives[0].update({"sha256": "z" * 64})
            ),
            "uppercase archive digest": with_archives(
                lambda archives: archives[0].update({"sha256": "A" * 64})
            ),
            "archive entry extra key": with_archives(
                lambda archives: archives[0].update({"signed": "no"})
            ),
            "archive entry non-string": with_archives(
                lambda archives: archives[0].update({"sha256": 1})
            ),
            "archive entry not an object": with_archives(
                lambda archives: archives.__setitem__(0, "skillmount.zip")
            ),
        }
        for description, text in cases.items():
            with self.subTest(case=description):
                with self.assertRaises(channels.ChannelError):
                    channels.PackageInputs.from_json(text)

    def test_asset_and_license_urls_are_pinned_to_the_tag(self) -> None:
        """Build only immutable tag-scoped GitHub URLs."""

        self.assertEqual(
            channels.asset_download_url(REPOSITORY, TAG, "a.zip"),
            f"https://github.com/{REPOSITORY}/releases/download/{TAG}/a.zip",
        )
        self.assertEqual(
            channels.license_url(REPOSITORY, TAG),
            f"https://github.com/{REPOSITORY}/blob/{TAG}/LICENSE-MIT",
        )
        with self.assertRaises(channels.ChannelError):
            channels.asset_download_url(REPOSITORY, TAG, "tools/a.zip")


class TriggerTests(unittest.TestCase):
    """Cover the trusted-trigger policy for automatic and manual runs."""

    def decide(self, **overrides: Any) -> channels.TriggerDecision:
        arguments: dict[str, Any] = {
            "event_name": "workflow_run",
            "repository": REPOSITORY,
            "workflow_run": workflow_run_payload(),
            "dispatch_tag": None,
            "dispatch_verification_only": False,
        }
        arguments.update(overrides)
        return channels.evaluate_trigger(**arguments)

    def test_successful_release_run_authorizes_publication(self) -> None:
        """Accept a successful tag-push Release run from this repository."""

        decision = self.decide()
        self.assertEqual(decision.tag, TAG)
        self.assertFalse(decision.verification_only)
        self.assertIn(str(RUN_ID), decision.reason)

    def test_rejected_workflow_runs(self) -> None:
        """Reject every inconsistent triggering run and name the observed value."""

        cases = {
            "failed run": {"conclusion": "failure"},
            "cancelled run": {"conclusion": "cancelled"},
            "incomplete run": {"status": "in_progress"},
            "wrong workflow name": {"name": "CI"},
            "wrong workflow path": {"path": ".github/workflows/ci.yml"},
            "wrong event": {"event": "workflow_dispatch"},
            "branch ref": {"head_branch": "main"},
            "prerelease tag": {"head_branch": "v0.2.0-rc.1"},
            "malformed tag": {"head_branch": "0.2.0"},
            "leading zero tag": {"head_branch": "v0.02.0"},
            "commit ref": {"head_branch": COMMIT},
            "missing head branch": {"head_branch": None},
            "foreign head repository": {"head_repository": {"full_name": "attacker/skillmount"}},
            "missing head repository": {"head_repository": None},
        }
        for description, overrides in cases.items():
            with self.subTest(case=description):
                with self.assertRaises(channels.ChannelError):
                    self.decide(workflow_run=workflow_run_payload(**overrides))

    def test_missing_workflow_run_payload_is_rejected(self) -> None:
        """Refuse a workflow_run trigger with no run object."""

        with self.assertRaises(channels.ChannelError):
            self.decide(workflow_run=None)

    def test_manual_dispatch_requires_an_exact_stable_tag(self) -> None:
        """Accept a stable dispatch tag and honor the verification-only input."""

        decision = self.decide(
            event_name="workflow_dispatch",
            workflow_run=None,
            dispatch_tag=TAG,
            dispatch_verification_only=True,
        )
        self.assertEqual(decision.tag, TAG)
        self.assertTrue(decision.verification_only)
        self.assertIn("verification_only=true", decision.reason)

        publishing = self.decide(
            event_name="workflow_dispatch",
            workflow_run=None,
            dispatch_tag=TAG,
            dispatch_verification_only=False,
        )
        self.assertFalse(publishing.verification_only)

    def test_invalid_manual_dispatch_is_rejected(self) -> None:
        """Reject a branch, commit, prerelease, malformed, or absent dispatch tag."""

        for tag in (None, "", "main", COMMIT, "v0.2.0-rc.1", "0.2.0", "v0.2.0 "):
            with self.subTest(tag=tag):
                with self.assertRaises(channels.ChannelError):
                    self.decide(
                        event_name="workflow_dispatch",
                        workflow_run=None,
                        dispatch_tag=tag,
                        dispatch_verification_only=True,
                    )

    def test_other_events_cannot_trigger_publication(self) -> None:
        """Reject any event other than the two authorized triggers."""

        for event in ("push", "release", "pull_request", "schedule"):
            with self.subTest(event=event):
                with self.assertRaises(channels.ChannelError):
                    self.decide(event_name=event, workflow_run=None, dispatch_tag=TAG)

    def test_foreign_repository_identity_is_rejected(self) -> None:
        """Refuse a malformed repository identity before reading the payload."""

        with self.assertRaises(channels.ChannelError):
            self.decide(repository="skillmount")


class GatewayTests(unittest.TestCase):
    """Cover the real GitHub CLI adapter logic without any network access."""

    def test_lightweight_tag_resolves_directly(self) -> None:
        """Return the commit a lightweight tag reference names."""

        stub = ApiStub(
            {
                f"repos/{REPOSITORY}/git/ref/tags/{TAG}": {
                    "object": {"type": "commit", "sha": COMMIT}
                }
            }
        )
        self.assertEqual(stub.dereference_tag(REPOSITORY, TAG), COMMIT)

    def test_annotated_tag_is_dereferenced_to_its_commit(self) -> None:
        """Follow an annotated tag object through to the commit it wraps."""

        stub = ApiStub(
            {
                "repos/pashifika/skillmount/git/ref/tags/v0.1.0": {
                    "object": {"type": "tag", "sha": ANNOTATED_TAG_OBJECT}
                },
                f"repos/{REPOSITORY}/git/tags/{ANNOTATED_TAG_OBJECT}": {
                    "object": {"type": "commit", "sha": ANNOTATED_TAG_COMMIT}
                },
            }
        )
        self.assertEqual(
            stub.dereference_tag(REPOSITORY, "v0.1.0"), ANNOTATED_TAG_COMMIT
        )
        self.assertEqual(len(stub.requests), 2)

    def test_non_commit_tag_object_is_rejected(self) -> None:
        """Refuse a tag pointing at a tree or blob instead of a commit."""

        for object_type in ("tree", "blob", None):
            with self.subTest(object_type=object_type):
                stub = ApiStub(
                    {
                        f"repos/{REPOSITORY}/git/ref/tags/{TAG}": {
                            "object": {"type": object_type, "sha": COMMIT}
                        }
                    }
                )
                with self.assertRaises(channels.ChannelError):
                    stub.dereference_tag(REPOSITORY, TAG)

    def test_malformed_tag_metadata_is_rejected(self) -> None:
        """Refuse tag metadata that does not carry an object SHA."""

        for reference in ({}, {"object": {}}, {"object": {"type": "tag"}}, None):
            with self.subTest(reference=reference):
                stub = ApiStub({f"repos/{REPOSITORY}/git/ref/tags/{TAG}": reference})
                with self.assertRaises(channels.ChannelError):
                    stub.dereference_tag(REPOSITORY, TAG)

    def test_default_branch_containment_uses_the_comparison_status(self) -> None:
        """Treat only identical or behind comparisons as containment."""

        for status, contained in (
            ("identical", True),
            ("behind", True),
            ("ahead", False),
            ("diverged", False),
        ):
            with self.subTest(status=status):
                stub = ApiStub(
                    {
                        f"repos/{REPOSITORY}": {"default_branch": "main"},
                        f"repos/{REPOSITORY}/compare/main...{COMMIT}": {"status": status},
                    }
                )
                self.assertEqual(
                    stub.commit_contained_in_default_branch(REPOSITORY, COMMIT), contained
                )

    def test_missing_comparison_status_fails_closed(self) -> None:
        """Refuse to assume containment when GitHub reports no status."""

        stub = ApiStub(
            {
                f"repos/{REPOSITORY}": {"default_branch": "main"},
                f"repos/{REPOSITORY}/compare/main...{COMMIT}": {},
            }
        )
        with self.assertRaises(channels.ChannelError):
            stub.commit_contained_in_default_branch(REPOSITORY, COMMIT)

    def test_download_refuses_a_foreign_host(self) -> None:
        """Never stream bytes from anywhere but github.com."""

        stub = ApiStub({})
        with tempfile.TemporaryDirectory() as name:
            destination = Path(name) / "payload.zip"
            with self.assertRaises(channels.ChannelError):
                stub.download("https://attacker.test/payload.zip", destination)
            self.assertFalse(destination.exists())

    def test_gateway_requires_an_authenticated_token(self) -> None:
        """Refuse to construct a gateway without GH_TOKEN."""

        previous = os.environ.pop("GH_TOKEN", None)
        try:
            with self.assertRaises(channels.ChannelError):
                channels.GhReleaseGateway(Path.cwd())
        finally:
            if previous is not None:
                os.environ["GH_TOKEN"] = previous


class CargoMetadataTests(unittest.TestCase):
    """Cover the data-only Cargo manifest and lockfile readers."""

    def test_manifest_version_comes_from_the_package_table(self) -> None:
        """Read the version only from `[package]`."""

        self.assertEqual(channels.cargo_manifest_version(manifest_text(VERSION)), VERSION)

    def test_manifest_without_a_package_version_is_rejected(self) -> None:
        """Refuse a manifest that declares no or several package versions."""

        for text in (
            "[workspace]\nmembers = []\n",
            '[[bin]]\nname = "asm"\nversion = "0.2.0"\n',
            '[package]\nversion = "0.2.0"\nversion = "0.3.0"\n',
        ):
            with self.subTest(text=text):
                with self.assertRaises(channels.ChannelError):
                    channels.cargo_manifest_version(text)

    def test_lock_version_is_read_from_the_product_entry(self) -> None:
        """Read the locked version of the product package only."""

        self.assertEqual(
            channels.cargo_lock_version(lock_text(VERSION), channels.CARGO_PACKAGE_NAME),
            VERSION,
        )

    def test_absent_lock_entry_is_rejected(self) -> None:
        """Refuse a lockfile that does not record the product package."""

        text = lock_text(VERSION).replace('name = "skillmount"', 'name = "other"')
        with self.assertRaises(channels.ChannelError):
            channels.cargo_lock_version(text, channels.CARGO_PACKAGE_NAME)


class PreflightTests(unittest.TestCase):
    """Cover ordered release verification against a real local asset fixture."""

    def setUp(self) -> None:
        """Build the complete release set and fake read-only boundary."""

        self.temporary = tempfile.TemporaryDirectory()
        self.root = Path(self.temporary.name)
        self.assets = build_release_assets(self.root)
        self.gateway = FakeGateway(self.assets)
        self.work = self.root / "work"

    def tearDown(self) -> None:
        """Remove the isolated fixture."""

        self.temporary.cleanup()

    def preflight(self) -> channels.PackageInputs:
        """Run preflight against the fake boundary."""

        return channels.preflight(
            self.gateway, repository=REPOSITORY, tag=TAG, work_directory=self.work
        )

    def refresh_checksums(self) -> None:
        """Recompute SHA256SUMS and the payload so one tampered value stays isolated."""

        archives = [self.assets / name for name in release.expected_archive_names(TAG)]
        (self.assets / release.CHECKSUM_FILE).write_text(
            release.checksum_text(archives), encoding="ascii"
        )
        self.gateway.refresh_release()

    def test_valid_release_yields_complete_inputs(self) -> None:
        """Emit the immutable identity every channel job consumes."""

        inputs = self.preflight()
        self.assertEqual(inputs.repository, REPOSITORY)
        self.assertEqual((inputs.version, inputs.tag, inputs.commit), (VERSION, TAG, COMMIT))
        self.assertEqual(inputs.release_url, RELEASE_URL)
        self.assertEqual(
            [archive.triple for archive in inputs.archives],
            sorted(target.triple for target in release.TARGETS),
        )
        for archive in inputs.archives:
            with self.subTest(triple=archive.triple):
                self.assertEqual(
                    archive.sha256, release.sha256_file(self.assets / archive.name)
                )
                self.assertEqual(
                    archive.url,
                    channels.asset_download_url(REPOSITORY, TAG, archive.name),
                )
        self.assertEqual(len(self.gateway.downloads), len(release.TARGETS) + 1)
        self.assertTrue(
            all("/releases/download/" in url for url in self.gateway.downloads),
            self.gateway.downloads,
        )
        self.assertEqual(channels.PackageInputs.from_json(inputs.to_json()), inputs)

    def test_commit_outside_the_default_branch_stops_before_download(self) -> None:
        """Refuse a tag whose commit the default branch does not contain."""

        self.gateway.contained = False
        with self.assertRaisesRegex(channels.ChannelError, "default branch"):
            self.preflight()
        self.assertEqual(self.gateway.downloads, [])

    def test_malformed_dereferenced_commit_is_rejected(self) -> None:
        """Refuse a tag resolution that is not a full lowercase object ID."""

        self.gateway.commit = "not-a-commit"
        with self.assertRaises(channels.ChannelError):
            self.preflight()
        self.assertEqual(self.gateway.downloads, [])

    def test_cargo_manifest_version_mismatch_is_rejected(self) -> None:
        """Refuse a tag whose commit declares another package version."""

        self.gateway.files["Cargo.toml"] = manifest_text("0.1.0").encode()
        with self.assertRaisesRegex(channels.ChannelError, "Cargo.toml"):
            self.preflight()
        self.assertEqual(self.gateway.downloads, [])

    def test_cargo_lock_version_mismatch_is_rejected(self) -> None:
        """Refuse a tag whose lockfile records another package version."""

        self.gateway.files["Cargo.lock"] = lock_text("0.1.0").encode()
        with self.assertRaisesRegex(channels.ChannelError, "Cargo.lock"):
            self.preflight()
        self.assertEqual(self.gateway.downloads, [])

    def test_non_utf8_manifest_is_rejected(self) -> None:
        """Treat repository files as untrusted bytes."""

        self.gateway.files["Cargo.toml"] = b"\xff\xfe[package]\n"
        with self.assertRaises(channels.ChannelError):
            self.preflight()

    def test_draft_release_is_rejected(self) -> None:
        """Never package an unpublished draft."""

        self.gateway.release["draft"] = True
        with self.assertRaisesRegex(channels.ChannelError, "draft"):
            self.preflight()
        self.assertEqual(self.gateway.downloads, [])

    def test_prerelease_release_is_rejected(self) -> None:
        """Never package a prerelease."""

        self.gateway.release["prerelease"] = True
        with self.assertRaisesRegex(channels.ChannelError, "prerelease"):
            self.preflight()

    def test_release_tag_mismatch_is_rejected(self) -> None:
        """Refuse a release whose tag name is not the requested tag."""

        self.gateway.release["tag_name"] = "v0.1.0"
        with self.assertRaises(channels.ChannelError):
            self.preflight()

    def test_release_commit_mismatch_is_rejected(self) -> None:
        """Refuse a release targeting another commit than the tag resolves to."""

        self.gateway.release["target_commitish"] = OTHER_COMMIT
        with self.assertRaisesRegex(channels.ChannelError, OTHER_COMMIT):
            self.preflight()
        self.assertEqual(self.gateway.downloads, [])

    def test_branch_target_commitish_is_accepted(self) -> None:
        """Accept a branch commitish because the tag resolution is authoritative."""

        self.gateway.release["target_commitish"] = "main"
        self.assertEqual(self.preflight().commit, COMMIT)

    def test_empty_target_commitish_is_rejected(self) -> None:
        """Refuse a release that reports no target at all."""

        self.gateway.release["target_commitish"] = ""
        with self.assertRaises(channels.ChannelError):
            self.preflight()

    def test_foreign_release_url_is_rejected(self) -> None:
        """Refuse a release whose own URL is not under this repository."""

        self.gateway.release["html_url"] = "https://github.com/attacker/skillmount/releases/tag/x"
        with self.assertRaises(channels.ChannelError):
            self.preflight()

    def test_missing_asset_is_rejected(self) -> None:
        """Refuse an incomplete published asset set."""

        removed = self.gateway.release["assets"].pop()
        with self.assertRaisesRegex(channels.ChannelError, "missing"):
            self.preflight()
        self.assertIn(removed["name"], release.expected_archive_names(TAG) + (
            release.CHECKSUM_FILE,
        ))
        self.assertEqual(self.gateway.downloads, [])

    def test_extra_asset_is_rejected(self) -> None:
        """Refuse an unexpected extra release asset instead of ignoring it."""

        self.gateway.release["assets"].append(
            {
                "name": "skillmount-v0.2.0-installer.msi",
                "state": "uploaded",
                "digest": f"sha256:{'a' * 64}",
                "browser_download_url": channels.asset_download_url(
                    REPOSITORY, TAG, "skillmount-v0.2.0-installer.msi"
                ),
            }
        )
        with self.assertRaisesRegex(channels.ChannelError, "unexpected"):
            self.preflight()

    def test_duplicate_asset_name_is_rejected(self) -> None:
        """Refuse a payload declaring one asset name twice."""

        self.gateway.release["assets"].append(copy.deepcopy(self.gateway.release["assets"][0]))
        with self.assertRaisesRegex(channels.ChannelError, "duplicate"):
            self.preflight()

    def test_incomplete_asset_state_is_rejected(self) -> None:
        """Refuse an asset that GitHub has not finished storing."""

        self.gateway.release["assets"][0]["state"] = "open"
        with self.assertRaises(channels.ChannelError):
            self.preflight()

    def test_foreign_asset_url_is_rejected(self) -> None:
        """Refuse an asset download URL outside this repository's releases."""

        self.gateway.release["assets"][0]["browser_download_url"] = (
            "https://attacker.test/skillmount.zip"
        )
        with self.assertRaises(channels.ChannelError):
            self.preflight()

    def test_checksum_file_mismatch_is_rejected(self) -> None:
        """Refuse a SHA256SUMS entry that disagrees with the downloaded bytes."""

        checksums = self.assets / release.CHECKSUM_FILE
        lines = checksums.read_text(encoding="ascii").splitlines(keepends=True)
        lines[0] = f"{'0' * 64}{lines[0][64:]}"
        checksums.write_text("".join(lines), encoding="ascii")
        self.gateway.refresh_release()
        with self.assertRaisesRegex(channels.ChannelError, release.CHECKSUM_FILE):
            self.preflight()

    def test_checksum_file_coverage_mismatch_is_rejected(self) -> None:
        """Refuse a SHA256SUMS file that does not cover exactly the three archives."""

        checksums = self.assets / release.CHECKSUM_FILE
        lines = checksums.read_text(encoding="ascii").splitlines(keepends=True)
        checksums.write_text("".join(lines[:2]), encoding="ascii")
        self.gateway.refresh_release()
        with self.assertRaisesRegex(channels.ChannelError, "covers"):
            self.preflight()

    def test_release_reported_digest_mismatch_is_rejected(self) -> None:
        """Refuse an asset whose GitHub-reported digest disagrees with its bytes."""

        self.gateway.release["assets"][0]["digest"] = f"sha256:{'1' * 64}"
        with self.assertRaisesRegex(channels.ChannelError, "reports digest"):
            self.preflight()

    def test_malformed_reported_digest_is_rejected(self) -> None:
        """Refuse a digest field that is not a lowercase sha256 value."""

        self.gateway.release["assets"][0]["digest"] = "md5:abc"
        with self.assertRaises(channels.ChannelError):
            self.preflight()

    def test_absent_reported_digest_still_verifies_local_bytes(self) -> None:
        """Accept assets GitHub reports no digest for after local verification."""

        for asset in self.gateway.release["assets"]:
            asset.pop("digest")
        self.assertEqual(self.preflight().version, VERSION)

    def test_bad_archive_layout_is_rejected(self) -> None:
        """Prove the dual-binary layout from bytes rather than assuming it."""

        target = channels.WINDOWS_X64
        archive = self.assets / release.asset_name(TAG, target)
        root = release.asset_stem(TAG, target)
        with zipfile.ZipFile(archive, "w") as package:
            package.writestr(f"{root}/", b"")
            package.writestr(f"{root}/{release.VERSION_FILE}", b"tampered\n")
        self.refresh_checksums()
        with self.assertRaises(release.ReleaseError):
            self.preflight()

    def test_prerelease_tag_never_reaches_the_gateway(self) -> None:
        """Refuse a prerelease tag before observing any release state."""

        with self.assertRaises(channels.ChannelError):
            channels.preflight(
                self.gateway,
                repository=REPOSITORY,
                tag="v0.2.0-rc.1",
                work_directory=self.work,
            )
        self.assertEqual(self.gateway.dereferenced, 0)


class TemplateFixture(unittest.TestCase):
    """Shared minimal template, license, and inputs fixture."""

    uninstall = False

    def setUp(self) -> None:
        """Write the channel templates and licenses into an isolated tree."""

        self.temporary = tempfile.TemporaryDirectory()
        self.root = Path(self.temporary.name)
        self.templates = write_templates(self.root / "packaging", uninstall=self.uninstall)
        self.licenses = write_licenses(self.root / "licenses")
        self.output = self.root / "candidates"
        self.inputs = canonical_inputs()

    def tearDown(self) -> None:
        """Remove the isolated fixture."""

        self.temporary.cleanup()

    def generate_formulae(self) -> dict[str, Path]:
        """Render both Formulae from the fixture templates."""

        return channels.generate_formulae(
            self.inputs,
            template_directory=self.templates / "homebrew",
            output_directory=self.output / "tap",
        )

    def generate_chocolatey(self) -> dict[str, Path]:
        """Render both Chocolatey package sources from the fixture templates."""

        return channels.generate_chocolatey_sources(
            self.inputs,
            template_directory=self.templates / "chocolatey",
            output_directory=self.output / "chocolatey",
            license_directory=self.licenses,
        )


class RenderTests(TemplateFixture):
    """Cover token substitution and drift detection."""

    def test_every_token_is_substituted(self) -> None:
        """Substitute all tokens and leave no placeholder behind."""

        rendered = channels.render_template(
            "@ONE@ and @TWO@", {"ONE": "first", "TWO": "second"}
        )
        self.assertEqual(rendered, "first and second")

    def test_unknown_token_is_a_hard_failure(self) -> None:
        """Fail when a template references a token the builder does not define."""

        with self.assertRaisesRegex(channels.ChannelError, "undefined"):
            channels.render_template("@ONE@ @THREE@", {"ONE": "first"})

    def test_unused_value_is_a_hard_failure(self) -> None:
        """Fail when a template stops using a token the builder still defines."""

        with self.assertRaisesRegex(channels.ChannelError, "omits"):
            channels.render_template("@ONE@", {"ONE": "first", "TWO": "second"})

    def test_empty_and_multiline_values_are_rejected(self) -> None:
        """Refuse a value that is empty or spans lines."""

        for value in ("", "first\nsecond", "first\r\nsecond", "trailing\n"):
            with self.subTest(value=value):
                with self.assertRaises(channels.ChannelError):
                    channels.render_template("@ONE@", {"ONE": value})

    def test_token_builders_match_the_contract_sets(self) -> None:
        """Expose exactly the documented token names for each template."""

        identity = channels.PACKAGES[1]
        self.assertEqual(
            sorted(channels.formula_tokens(self.inputs, identity)),
            sorted(
                (
                    "FORMULA_CLASS",
                    "PACKAGE_ID",
                    "DESCRIPTION",
                    "HOMEPAGE",
                    "ARCHIVE_URL",
                    "ARCHIVE_SHA256",
                    "VERSION",
                    "LICENSE",
                    "COMMAND",
                    "OTHER_COMMAND",
                    "TAG",
                    "COMMIT",
                )
            ),
        )
        self.assertEqual(
            sorted(channels.nuspec_tokens(self.inputs, identity)),
            sorted(
                (
                    "PACKAGE_ID",
                    "VERSION",
                    "TITLE",
                    "SUMMARY",
                    "DESCRIPTION",
                    "PROJECT_URL",
                    "PROJECT_SOURCE_URL",
                    "LICENSE_URL",
                    "RELEASE_NOTES_URL",
                    "COMMAND",
                    "TAG",
                )
            ),
        )
        self.assertEqual(
            sorted(channels.install_script_tokens(self.inputs, identity)),
            sorted(
                (
                    "PACKAGE_ID",
                    "VERSION",
                    "TAG",
                    "COMMAND",
                    "SELECTED_EXECUTABLE",
                    "OTHER_EXECUTABLE",
                    "URL_X86",
                    "SHA256_X86",
                    "URL_X64",
                    "SHA256_X64",
                    "ARCHIVE_ROOT_X86",
                    "ARCHIVE_ROOT_X64",
                )
            ),
        )
        self.assertEqual(
            sorted(channels.uninstall_script_tokens(self.inputs, identity)),
            ["COMMAND", "PACKAGE_ID", "SELECTED_EXECUTABLE"],
        )
        tokens = channels.install_script_tokens(self.inputs, identity)
        self.assertEqual(tokens["SELECTED_EXECUTABLE"], "asm.exe")
        self.assertEqual(tokens["OTHER_EXECUTABLE"], "skillmount.exe")
        self.assertEqual(
            tokens["ARCHIVE_ROOT_X64"], release.asset_stem(TAG, channels.WINDOWS_X64)
        )

    def test_template_drift_in_both_directions_fails_generation(self) -> None:
        """Fail generation when a template adds or drops a token."""

        formula = self.templates / "homebrew" / "skillmount.rb.in"
        formula.write_text(
            FORMULA_TEMPLATE.replace("@LICENSE@", "@LICENCE@"), encoding="utf-8"
        )
        with self.assertRaisesRegex(channels.ChannelError, "undefined"):
            self.generate_formulae()

        formula.write_text(
            FORMULA_TEMPLATE.replace("license @LICENSE@\n", ""), encoding="utf-8"
        )
        with self.assertRaisesRegex(channels.ChannelError, "omits"):
            self.generate_formulae()


class HomebrewGenerationTests(TemplateFixture):
    """Cover Formula generation and paired structural inspection."""
    def test_formula_tokens_select_the_validated_macos_archive(self) -> None:
        """Render Homebrew only from the checked Apple Silicon release asset."""

        identity = channels.PACKAGES[0]
        archive = self.inputs.archive(channels.MACOS_ARM64.triple)
        tokens = channels.formula_tokens(self.inputs, identity)
        self.assertEqual(tokens["ARCHIVE_URL"], archive.url)
        self.assertEqual(tokens["ARCHIVE_SHA256"], archive.sha256)
        self.assertEqual(tokens["LICENSE"], channels.HOMEBREW_LICENSE_EXPRESSION)
        self.assertNotIn("SOURCE_URL", tokens)
        self.assertNotIn("SOURCE_SHA256", tokens)
        self.assertNotIn("CARGO_BIN", tokens)


    def test_generated_pair_passes_inspection(self) -> None:
        """Render both Formulae at the expected paths and verify the pair."""

        formulae = self.generate_formulae()
        archive = self.inputs.archive(channels.MACOS_ARM64.triple)
        self.assertEqual(sorted(formulae), ["skillmount", "skillmount-asm"])
        for identity in channels.PACKAGES:
            path = formulae[identity.package_id]
            self.assertEqual(
                path, self.output / "tap" / "Formula" / f"{identity.package_id}.rb"
            )
            text = path.read_text(encoding="utf-8")
            self.assertIn(f"class {identity.formula_class} < Formula", text)
            self.assertIn(f'bin.install "{identity.command}"', text)
            self.assertIn(archive.sha256, text)
            self.assertIn('license any_of: ["MIT", "Apache-2.0"]', text)
            self.assertIn('pkgshare.install "LICENSE-APACHE", "LICENSE-MIT", "VERSION"', text)
            self.assertNotIn('system "cargo"', text)
            self.assertNotIn('depends_on "rust"', text)
            self.assertNotIn("@", text.replace("#{", ""))
        channels.inspect_formulae(formulae, self.inputs)

    def test_incomplete_pair_is_rejected(self) -> None:
        """Refuse to verify a pair that is missing a member or file."""

        formulae = self.generate_formulae()
        with self.assertRaises(channels.ChannelError):
            channels.inspect_formulae({"skillmount": formulae["skillmount"]}, self.inputs)
        formulae["skillmount-asm"].unlink()
        with self.assertRaises(channels.ChannelError):
            channels.inspect_formulae(formulae, self.inputs)

    def test_formula_failure_modes(self) -> None:
        """Reject every observable Formula defect the specs name."""

        archive = self.inputs.archive(channels.MACOS_ARM64.triple)
        mutations = {
            "wrong class": (
                "class Skillmount < Formula",
                "class Other < Formula",
                "Formula class 'Other'",
            ),
            "wrong archive url": (
                archive.url,
                "https://attacker.test/x.tar.gz",
                "declares url",
            ),
            "wrong digest": (archive.sha256, "0" * 64, "declares sha256"),
            "wrong homepage": (
                channels.HOMEPAGE,
                "https://attacker.test",
                "declares homepage",
            ),
            "wrong license": (
                channels.HOMEBREW_LICENSE_EXPRESSION,
                '"Proprietary"',
                "declares license",
            ),
            "wrong description": (
                channels.PACKAGES[0].description,
                "Something else entirely",
                "declares desc",
            ),
            "missing macos requirement": (
                "  depends_on :macos\n",
                "",
                "declares dependencies",
            ),
            "extra dependency": (
                "  depends_on :macos\n",
                '  depends_on :macos\n  depends_on "openssl@3"\n',
                "declares dependencies",
            ),
            "pair dependency": (
                "  depends_on :macos\n",
                '  depends_on :macos\n  depends_on "pashifika/tap/skillmount-asm"\n',
                "declares dependencies",
            ),
            "conflicts declaration": (
                "  depends_on :macos\n",
                '  depends_on :macos\n  conflicts_with "skillmount-asm"\n',
                "conflicts_with",
            ),
            "wrong installed binary": (
                'bin.install "skillmount"',
                'bin.install "asm"',
                r"installs binaries \['asm'\]",
            ),
            "missing package data": (
                '    pkgshare.install "LICENSE-APACHE", "LICENSE-MIT", "VERSION"\n',
                "",
                "installs package data",
            ),
            "Cargo invocation": (
                '    bin.install "skillmount"',
                '    system "cargo", "install"\n    bin.install "skillmount"',
                "invokes Cargo",
            ),
            "missing commit provenance": (
                COMMIT,
                "unknown",
                "does not record the released commit",
            ),
            "pair command in install": (
                '    bin.install "skillmount"',
                '    # also probes asm\n    bin.install "skillmount"',
                "names the pair member command 'asm' outside its test block",
            ),
            "missing test block": (
                "  test do\n",
                "  verify do\n",
                "declares no `test do` block",
            ),
        }
        for description, (before, after, expected) in mutations.items():
            with self.subTest(case=description):
                formulae = self.generate_formulae()
                path = formulae["skillmount"]
                text = path.read_text(encoding="utf-8")
                self.assertIn(before, text)
                path.write_text(text.replace(before, after, 1), encoding="utf-8")
                with self.assertRaisesRegex(channels.ChannelError, expected):
                    channels.inspect_formulae(formulae, self.inputs)

    def test_unsubstituted_token_is_rejected(self) -> None:
        """Refuse a generated Formula that still carries a template token."""

        formulae = self.generate_formulae()
        path = formulae["skillmount-asm"]
        path.write_text(
            path.read_text(encoding="utf-8").replace(TAG, "@TAG@", 1), encoding="utf-8"
        )
        with self.assertRaisesRegex(channels.ChannelError, "unsubstituted"):
            channels.inspect_formulae(formulae, self.inputs)

    def test_pair_divergence_outside_selection_tokens_is_rejected(self) -> None:
        """Refuse Formulae that differ anywhere except the selection tokens."""

        formulae = self.generate_formulae()
        path = formulae["skillmount-asm"]
        path.write_text(
            path.read_text(encoding="utf-8") + "# unreviewed trailing note\n", encoding="utf-8"
        )
        with self.assertRaisesRegex(channels.ChannelError, "differ outside"):
            channels.inspect_formulae(formulae, self.inputs)

    def test_missing_template_is_reported(self) -> None:
        """Name the absent template instead of generating half a pair."""

        (self.templates / "homebrew" / "skillmount-asm.rb.in").unlink()
        with self.assertRaisesRegex(channels.ChannelError, "does not exist"):
            self.generate_formulae()


class ChocolateyGenerationTests(TemplateFixture):
    """Cover Chocolatey source generation and paired structural inspection."""

    def test_generated_pair_passes_inspection(self) -> None:
        """Render both package sources with licenses and provenance evidence."""

        sources = self.generate_chocolatey()
        self.assertEqual(sorted(sources), ["skillmount", "skillmount-asm"])
        for identity in channels.PACKAGES:
            root = sources[identity.package_id]
            self.assertEqual(
                channels.chocolatey_member_names(root),
                channels.expected_chocolatey_members(identity, uninstall=False),
            )
            verification = (root / channels.VERIFICATION_FILE).read_text(encoding="utf-8")
            self.assertIn(identity.windows_executable, verification)
            self.assertIn(self.inputs.commit, verification)
            for target in (channels.WINDOWS_X86, channels.WINDOWS_X64):
                archive = self.inputs.archive(target.triple)
                self.assertIn(archive.url, verification)
                self.assertIn(archive.sha256, verification)
        channels.inspect_chocolatey_sources(sources, self.inputs)

    def test_nuspec_records_the_pinned_urls(self) -> None:
        """Pin the license, project, and release-notes URLs to this tag."""

        sources = self.generate_chocolatey()
        text = (sources["skillmount"] / "skillmount.nuspec").read_text(encoding="utf-8")
        self.assertIn(channels.license_url(REPOSITORY, TAG), text)
        self.assertIn(RELEASE_URL, text)
        self.assertIn(f"<version>{VERSION}</version>", text)

    def test_incomplete_pair_is_rejected(self) -> None:
        """Refuse to verify one package source alone."""

        sources = self.generate_chocolatey()
        with self.assertRaises(channels.ChannelError):
            channels.inspect_chocolatey_sources(
                {"skillmount": sources["skillmount"]}, self.inputs
            )

    def test_unexpected_and_missing_members_are_rejected(self) -> None:
        """Refuse an extra payload or an absent required member."""

        sources = self.generate_chocolatey()
        extra = sources["skillmount"] / "tools" / "extra.txt"
        extra.write_text("unreviewed\n", encoding="utf-8")
        with self.assertRaises(channels.ChannelError):
            channels.inspect_chocolatey_sources(sources, self.inputs)
        extra.unlink()
        (sources["skillmount"] / channels.VERIFICATION_FILE).unlink()
        with self.assertRaises(channels.ChannelError):
            channels.inspect_chocolatey_sources(sources, self.inputs)

    def test_install_script_failure_modes(self) -> None:
        """Reject every observable install-script defect the specs name."""

        mutations = {
            "wrong x64 url": (
                self.inputs.archive(channels.WINDOWS_X64.triple).url,
                "https://attacker.test/x.zip",
                "omits the x64 archive URL",
            ),
            "wrong x86 digest": (
                self.inputs.archive(channels.WINDOWS_X86.triple).sha256,
                "0" * 64,
                "omits the x86 archive digest",
            ),
            "missing strict mode": (
                "Set-StrictMode -Version 2\n",
                "",
                "does not set Set-StrictMode",
            ),
            "missing error preference": (
                "$ErrorActionPreference = 'Stop'\n",
                "",
                r"does not set \$ErrorActionPreference",
            ),
            "path mutation": (
                "$toolsDir = ",
                "Install-ChocolateyPath 'C:\\tools'\n$toolsDir = ",
                "permanent PATH edit",
            ),
            "profile edit": (
                "$toolsDir = ",
                "Add-Content $PROFILE ''\n$toolsDir = ",
                "PowerShell profile edit",
            ),
            "ignore marker": (
                "$toolsDir = ",
                'New-Item "$env:TEMP\\asm.exe.ignore"\n$toolsDir = ',
                "ignore marker",
            ),
            "unselected executable dropped": (
                "$otherExecutable = 'asm.exe'\n",
                "",
                "omits the unselected executable",
            ),
        }
        for description, (before, after, expected) in mutations.items():
            with self.subTest(case=description):
                sources = self.generate_chocolatey()
                path = sources["skillmount"] / channels.INSTALL_SCRIPT
                text = path.read_text(encoding="utf-8")
                self.assertIn(before, text)
                path.write_text(text.replace(before, after, 1), encoding="utf-8")
                with self.assertRaisesRegex(channels.ChannelError, expected):
                    channels.inspect_chocolatey_sources(sources, self.inputs)

    def test_nuspec_failure_modes(self) -> None:
        """Reject a nuspec that misdeclares its identity or URLs."""

        mutations = {
            "wrong id": (
                "<id>skillmount</id>",
                "<id>skillmount-cli</id>",
                "declares <id>",
            ),
            "wrong version": (
                f"<version>{VERSION}</version>",
                "<version>0.3.0</version>",
                "declares <version>",
            ),
            "wrong license url": (
                channels.license_url(REPOSITORY, TAG),
                "https://attacker.test/license",
                "declares <licenseUrl>",
            ),
            "wrong project url": (
                f"<projectUrl>{channels.HOMEPAGE}</projectUrl>",
                "<projectUrl>https://attacker.test</projectUrl>",
                "declares <projectUrl>",
            ),
            "wrong release notes": (
                f"<releaseNotes>{RELEASE_URL}</releaseNotes>",
                "<releaseNotes>https://attacker.test</releaseNotes>",
                "declares <releaseNotes>",
            ),
            "dropped summary": ("<summary>", "<other>", "declares 0 <summary> elements"),
        }
        for description, (before, after, expected) in mutations.items():
            with self.subTest(case=description):
                sources = self.generate_chocolatey()
                path = sources["skillmount"] / "skillmount.nuspec"
                text = path.read_text(encoding="utf-8")
                self.assertIn(before, text)
                path.write_text(text.replace(before, after, 1), encoding="utf-8")
                with self.assertRaisesRegex(channels.ChannelError, expected):
                    channels.inspect_chocolatey_sources(sources, self.inputs)

    def test_verification_file_must_name_every_value(self) -> None:
        """Refuse provenance evidence that drops a URL or digest."""

        sources = self.generate_chocolatey()
        path = sources["skillmount-asm"] / channels.VERIFICATION_FILE
        digest = self.inputs.archive(channels.WINDOWS_X64.triple).sha256
        path.write_text(
            path.read_text(encoding="utf-8").replace(digest, "0" * 64), encoding="utf-8"
        )
        with self.assertRaisesRegex(channels.ChannelError, "VERIFICATION.txt"):
            channels.inspect_chocolatey_sources(sources, self.inputs)

    def test_install_script_pair_divergence_is_rejected(self) -> None:
        """Refuse install scripts that differ outside the selection tokens."""

        sources = self.generate_chocolatey()
        path = sources["skillmount"] / channels.INSTALL_SCRIPT
        path.write_text(
            path.read_text(encoding="utf-8") + "Write-Host 'unreviewed step'\n",
            encoding="utf-8",
        )
        with self.assertRaisesRegex(channels.ChannelError, "differ outside"):
            channels.inspect_chocolatey_sources(sources, self.inputs)

    def test_missing_license_source_is_reported(self) -> None:
        """Name the absent license file instead of shipping a package without it."""

        (self.licenses / "LICENSE-MIT").unlink()
        with self.assertRaisesRegex(channels.ChannelError, "LICENSE-MIT"):
            self.generate_chocolatey()


class ChocolateyUninstallTests(TemplateFixture):
    """Cover the optional uninstall script template."""

    uninstall = True

    def test_optional_uninstall_script_is_rendered_and_accepted(self) -> None:
        """Render and accept the optional uninstall script when it is provided."""

        sources = self.generate_chocolatey()
        for identity in channels.PACKAGES:
            root = sources[identity.package_id]
            self.assertEqual(
                channels.chocolatey_member_names(root),
                channels.expected_chocolatey_members(identity, uninstall=True),
            )
            text = (root / channels.UNINSTALL_SCRIPT).read_text(encoding="utf-8")
            self.assertIn(identity.windows_executable, text)
        channels.inspect_chocolatey_sources(sources, self.inputs)


class NupkgTests(TemplateFixture):
    """Cover packed-candidate inspection performed without extraction."""

    def setUp(self) -> None:
        """Generate both package sources and pack both candidates."""

        super().setUp()
        self.sources = self.generate_chocolatey()
        self.packed = self.root / "packed"
        self.paths = {
            identity.package_id: pack_nupkg(
                self.sources[identity.package_id], identity, self.packed
            )
            for identity in channels.PACKAGES
        }

    def test_valid_pair_returns_both_digests(self) -> None:
        """Verify both candidates and report their digests."""

        digests = channels.inspect_nupkg_pair(self.paths, self.inputs)
        self.assertEqual(sorted(digests), ["skillmount", "skillmount-asm"])
        for package_id, digest in digests.items():
            with self.subTest(package=package_id):
                self.assertEqual(digest, release.sha256_file(self.paths[package_id]))
        self.assertEqual(
            self.paths["skillmount-asm"].name, f"skillmount-asm.{VERSION}.nupkg"
        )

    def test_wrong_candidate_filename_is_rejected(self) -> None:
        """Require the exact `<id>.<version>.nupkg` filename."""

        renamed = pack_nupkg(
            self.sources["skillmount"],
            channels.PACKAGES[0],
            self.packed,
            name="skillmount.0.3.0.nupkg",
        )
        with self.assertRaises(channels.ChannelError):
            channels.inspect_nupkg(renamed, self.inputs, channels.PACKAGES[0])

    def test_unsafe_and_executable_members_are_rejected(self) -> None:
        """Refuse any executable, archive, absolute, drive-letter, or traversing member."""

        payload = "is an executable or archive payload"
        unsafe = "is not a safe relative path"
        cases = {
            "windows executable": ({"tools/asm.exe": b"MZ"}, payload),
            "library": ({"tools/skillmount.dll": b"MZ"}, payload),
            "zip payload": ({"tools/skillmount.zip": b"PK"}, payload),
            "tarball payload": ({"tools/skillmount.tar.gz": b"\x1f\x8b"}, payload),
            "installer payload": ({"tools/skillmount.msi": b"MZ"}, payload),
            "absolute member": ({"/etc/passwd": b"root"}, unsafe),
            "drive letter member": ({"C:/Windows/system32/a.txt": b"x"}, "drive letter"),
            "traversing member": ({"../escaped.txt": b"x"}, unsafe),
            "backslash member": ({"tools\\escaped.txt": b"x"}, "uses a backslash"),
            "unexpected member": ({"tools/extra.txt": b"x"}, "unexpected"),
        }
        for description, (extra, expected) in cases.items():
            with self.subTest(case=description):
                path = pack_nupkg(
                    self.sources["skillmount"],
                    channels.PACKAGES[0],
                    self.packed / description,
                    extra=extra,
                )
                with self.assertRaisesRegex(channels.ChannelError, expected):
                    channels.inspect_nupkg(path, self.inputs, channels.PACKAGES[0])

    def test_missing_members_are_rejected(self) -> None:
        """Refuse a candidate missing metadata, a script, a license, or provenance."""

        for member in (
            "skillmount.nuspec",
            channels.INSTALL_SCRIPT,
            channels.VERIFICATION_FILE,
            "tools/LICENSE-MIT",
            channels.CONTENT_TYPES_MEMBER,
            channels.RELS_MEMBER,
            PSMDCP_MEMBER,
        ):
            with self.subTest(member=member):
                path = pack_nupkg(
                    self.sources["skillmount"],
                    channels.PACKAGES[0],
                    self.packed / member.replace("/", "_"),
                    omit=(member,),
                )
                with self.assertRaises(channels.ChannelError):
                    channels.inspect_nupkg(path, self.inputs, channels.PACKAGES[0])

    def test_duplicate_core_properties_member_is_rejected(self) -> None:
        """Require exactly one NuGet core-properties member."""

        path = pack_nupkg(
            self.sources["skillmount"],
            channels.PACKAGES[0],
            self.packed / "twin",
            extra={
                "package/services/metadata/core-properties/ffffffff.psmdcp": PSMDCP_XML
            },
        )
        with self.assertRaisesRegex(channels.ChannelError, "core-properties"):
            channels.inspect_nupkg(path, self.inputs, channels.PACKAGES[0])

    def test_declared_identity_must_match_the_inputs(self) -> None:
        """Refuse a candidate whose nuspec declares another id or version."""

        for description, replacement in (
            ("id", ("<id>skillmount</id>", "<id>skillmount-cli</id>")),
            ("version", (f"<version>{VERSION}</version>", "<version>0.3.0</version>")),
        ):
            with self.subTest(case=description):
                text = (self.sources["skillmount"] / "skillmount.nuspec").read_text(
                    encoding="utf-8"
                )
                path = pack_nupkg(
                    self.sources["skillmount"],
                    channels.PACKAGES[0],
                    self.packed / f"identity-{description}",
                    extra={
                        "skillmount.nuspec": text.replace(*replacement, 1).encode("utf-8")
                    },
                )
                with self.assertRaises(channels.ChannelError):
                    channels.inspect_nupkg(path, self.inputs, channels.PACKAGES[0])

    def test_install_script_must_pin_both_architectures(self) -> None:
        """Refuse a packed script that drops an architecture URL or digest."""

        archive = self.inputs.archive(channels.WINDOWS_X86.triple)
        text = (self.sources["skillmount"] / channels.INSTALL_SCRIPT).read_text(
            encoding="utf-8"
        )
        path = pack_nupkg(
            self.sources["skillmount"],
            channels.PACKAGES[0],
            self.packed / "unpinned",
            extra={
                channels.INSTALL_SCRIPT: text.replace(
                    archive.url, "https://attacker.test/x.zip"
                ).encode("utf-8")
            },
        )
        with self.assertRaisesRegex(channels.ChannelError, channels.WINDOWS_X86.triple):
            channels.inspect_nupkg(path, self.inputs, channels.PACKAGES[0])

    def test_pair_divergence_blocks_both_candidates(self) -> None:
        """Refuse packed candidates whose scripts differ outside selection tokens."""

        script = self.sources["skillmount"] / channels.INSTALL_SCRIPT
        script.write_text(
            script.read_text(encoding="utf-8") + "Write-Host 'unreviewed step'\n",
            encoding="utf-8",
        )
        self.paths["skillmount"] = pack_nupkg(
            self.sources["skillmount"], channels.PACKAGES[0], self.packed / "diverged"
        )
        with self.assertRaisesRegex(channels.ChannelError, "differ outside"):
            channels.inspect_nupkg_pair(self.paths, self.inputs)

    def test_incomplete_pair_is_rejected(self) -> None:
        """Refuse to inspect one packed candidate as a pair."""

        with self.assertRaises(channels.ChannelError):
            channels.inspect_nupkg_pair(
                {"skillmount": self.paths["skillmount"]}, self.inputs
            )

    def test_absent_candidate_is_reported(self) -> None:
        """Name a candidate that was never packed."""

        self.paths["skillmount"].unlink()
        with self.assertRaises(channels.ChannelError):
            channels.inspect_nupkg_pair(self.paths, self.inputs)

    def test_corrupt_candidate_is_reported(self) -> None:
        """Refuse a candidate that is not a readable ZIP container."""

        self.paths["skillmount"].write_bytes(b"not a zip")
        with self.assertRaisesRegex(channels.ChannelError, "cannot inspect"):
            channels.inspect_nupkg_pair(self.paths, self.inputs)


class CommandLineTests(TemplateFixture):
    """Cover the operator-facing subcommands end to end."""

    def capture(self, arguments: list[str]) -> tuple[int, str, str]:
        """Run one subcommand and capture its status and streams."""

        out = io.StringIO()
        err = io.StringIO()
        with contextlib.redirect_stdout(out), contextlib.redirect_stderr(err):
            status = channels.main(arguments)
        return status, out.getvalue(), err.getvalue()

    def test_selection_map_prints_both_packages(self) -> None:
        """Print the two-package selection map and exit successfully."""

        status, output, errors = self.capture(["selection-map"])
        self.assertEqual(status, 0)
        self.assertEqual(errors, "")
        self.assertEqual(output.splitlines(), list(channels.selection_map_lines()))

    def test_generate_and_inspect_round_trip(self) -> None:
        """Generate and inspect both channels through the command line."""

        artifact = self.root / "package-inputs.json"
        artifact.write_text(self.inputs.to_json(), encoding="utf-8")
        tap = self.root / "cli-tap"
        chocolatey = self.root / "cli-chocolatey"
        for arguments in (
            [
                "generate-homebrew",
                "--inputs",
                str(artifact),
                "--template-directory",
                str(self.templates / "homebrew"),
                "--output-directory",
                str(tap),
            ],
            [
                "generate-chocolatey",
                "--inputs",
                str(artifact),
                "--template-directory",
                str(self.templates / "chocolatey"),
                "--output-directory",
                str(chocolatey),
                "--license-directory",
                str(self.licenses),
            ],
            ["inspect-homebrew", "--inputs", str(artifact), "--directory", str(tap)],
            [
                "inspect-chocolatey",
                "--inputs",
                str(artifact),
                "--directory",
                str(chocolatey),
            ],
        ):
            with self.subTest(command=arguments[0]):
                status, output, errors = self.capture(arguments)
                self.assertEqual(status, 0, msg=errors)
                self.assertNotEqual(output, "")

        packed = self.root / "cli-packed"
        for identity in channels.PACKAGES:
            pack_nupkg(chocolatey / identity.package_id, identity, packed)
        status, output, errors = self.capture(
            ["inspect-nupkg", "--inputs", str(artifact), "--directory", str(packed)]
        )
        self.assertEqual(status, 0, msg=errors)
        self.assertEqual(len(output.splitlines()), 2)

    def test_tampered_artifact_exits_nonzero(self) -> None:
        """Report a tampered inputs artifact as a stable failure."""

        artifact = self.root / "tampered.json"
        document = canonical_document()
        document["release_url"] = "https://attacker.test/releases/tag/v0.2.0"
        artifact.write_text(json.dumps(document), encoding="utf-8")
        status, _, errors = self.capture(
            [
                "generate-homebrew",
                "--inputs",
                str(artifact),
                "--template-directory",
                str(self.templates / "homebrew"),
                "--output-directory",
                str(self.root / "rejected"),
            ]
        )
        self.assertEqual(status, 1)
        self.assertIn("package channel validation failed", errors)
        self.assertFalse((self.root / "rejected").exists())

    def test_missing_artifact_exits_nonzero(self) -> None:
        """Report an absent inputs artifact instead of raising."""

        status, _, errors = self.capture(
            [
                "inspect-homebrew",
                "--inputs",
                str(self.root / "absent.json"),
                "--directory",
                str(self.root),
            ]
        )
        self.assertEqual(status, 1)
        self.assertIn("does not exist", errors)


class RealTemplateTests(unittest.TestCase):
    """Assert the tracked packaging templates match this module's token contract."""

    def packaging_root(self) -> Path:
        """Return the tracked packaging tree or skip while it is being authored."""

        root = channels.REPOSITORY_ROOT / "packaging"
        if not (root / "homebrew" / "skillmount.rb.in").is_file():
            raise unittest.SkipTest("packaging/homebrew/skillmount.rb.in is not present yet")
        return root

    def test_tracked_templates_use_exactly_the_contract_tokens(self) -> None:
        """Render and inspect the real templates with the documented token sets."""

        root = self.packaging_root()
        inputs = canonical_inputs()
        for identity in channels.PACKAGES:
            with self.subTest(package=identity.package_id):
                self.assertEqual(
                    template_tokens(root / "homebrew" / f"{identity.package_id}.rb.in"),
                    sorted(channels.formula_tokens(inputs, identity)),
                )
                package = root / "chocolatey" / identity.package_id
                self.assertEqual(
                    template_tokens(package / f"{identity.package_id}.nuspec.in"),
                    sorted(channels.nuspec_tokens(inputs, identity)),
                )
                self.assertEqual(
                    template_tokens(package / f"{channels.INSTALL_SCRIPT}.in"),
                    sorted(channels.install_script_tokens(inputs, identity)),
                )
                uninstall = package / f"{channels.UNINSTALL_SCRIPT}.in"
                if uninstall.is_file():
                    self.assertEqual(
                        template_tokens(uninstall),
                        sorted(channels.uninstall_script_tokens(inputs, identity)),
                    )
        with tempfile.TemporaryDirectory() as name:
            output = Path(name)
            formulae = channels.generate_formulae(
                inputs,
                template_directory=root / "homebrew",
                output_directory=output / "tap",
            )
            channels.inspect_formulae(formulae, inputs)
            sources = channels.generate_chocolatey_sources(
                inputs,
                template_directory=root / "chocolatey",
                output_directory=output / "chocolatey",
                license_directory=channels.REPOSITORY_ROOT,
            )
            channels.inspect_chocolatey_sources(sources, inputs)


if __name__ == "__main__":
    unittest.main()
