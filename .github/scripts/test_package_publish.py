#!/usr/bin/env python3
"""State-machine tests for paired Homebrew tap and Chocolatey reconciliation."""

from __future__ import annotations

import base64
import hashlib
import json
import tempfile
import unittest
from unittest import mock
from pathlib import Path
from typing import Any

import package_channels as channels
import package_publish
import release

REPOSITORY = "pashifika/skillmount"
TAP_REPOSITORY = "pashifika/homebrew-tap"
VERSION = "0.2.0"
TAG = "v0.2.0"
COMMIT = "c" * 40
DEFAULT_BRANCH = "main"
DEFAULT_BRANCH_COMMIT = "d" * 40
BRANCH = f"skillmount/{VERSION}"
OTHER_BRANCH = "skillmount/0.1.9"

FORMULA_TEMPLATE = """\
# {product} {version}
# Tag: {tag}
# Commit: {commit}
class {formula_class} < Formula
  desc "{summary}"
  homepage "{homepage}"
  url "{archive_url}"
  sha256 "{archive_sha256}"
  license {license_expression}
{version_line}
  depends_on :macos
  depends_on arch: :arm64

  def install
    bin.install "{installed_binary}"
    pkgshare.install "LICENSE-APACHE", "LICENSE-MIT", "VERSION"
    generate_completions_from_executable(bin/"{command}", "completions",
                                         base_name: "{command}",
                                         shells: [:bash, :zsh, :fish])
  end

  test do
    assert_match "{version}", shell_output("#{{bin}}/{command} --version")
    system bin/"{command}", "--help"
    refute_predicate (bin/"{other_command}"), :exist?
  end
end
{trailer}"""


def digest(seed: str) -> str:
    """Return a deterministic lowercase SHA-256 digest for one fixture value."""

    return hashlib.sha256(seed.encode()).hexdigest()


def inputs_document(**overrides: Any) -> dict[str, Any]:
    """Build the preflight inputs document both channel lanes consume."""

    archives = [
        {
            "triple": target.triple,
            "name": release.asset_name(TAG, target),
            "url": (
                f"https://github.com/{REPOSITORY}/releases/download/{TAG}/"
                f"{release.asset_name(TAG, target)}"
            ),
            "sha256": digest(f"archive:{target.triple}"),
        }
        for target in sorted(release.TARGETS, key=lambda target: target.triple)
    ]
    document: dict[str, Any] = {
        "schema": channels.INPUTS_SCHEMA,
        "repository": REPOSITORY,
        "version": VERSION,
        "tag": TAG,
        "commit": COMMIT,
        "release_url": f"https://github.com/{REPOSITORY}/releases/tag/{TAG}",
        "archives": archives,
    }
    document.update(overrides)
    return document


def package_inputs() -> channels.PackageInputs:
    """Load the fixture inputs only through the documented strict entry point."""

    return channels.PackageInputs.from_json(json.dumps(inputs_document()))


def formula_text(
    inputs: channels.PackageInputs,
    identity: channels.PackageIdentity,
    *,
    archive_url: str | None = None,
    archive_sha256: str | None = None,
    version: str | None = None,
    installed_binary: str | None = None,
    command: str | None = None,
    trailer: str = "",
) -> str:
    """Render one Formula fixture, optionally diverging from expected provenance."""

    archive = inputs.archive(channels.MACOS_ARM64.triple)
    selected = command or identity.command
    return FORMULA_TEMPLATE.format(
        product=channels.PRODUCT_NAME,
        version=inputs.version,
        tag=inputs.tag,
        commit=inputs.commit,
        formula_class=identity.formula_class,
        summary=identity.summary,
        homepage=channels.HOMEPAGE,
        archive_url=archive_url or archive.url,
        archive_sha256=archive_sha256 or archive.sha256,
        license_expression=channels.HOMEBREW_LICENSE_EXPRESSION,
        version_line="" if version is None else f'  version "{version}"\n',
        installed_binary=installed_binary or identity.command,
        command=selected,
        other_command=identity.other.command,
        trailer=trailer,
    )


def generated_formulae(inputs: channels.PackageInputs) -> dict[str, str]:
    """Render the expected Formula candidate for both pair members."""

    return {
        identity.package_id: formula_text(inputs, identity)
        for identity in channels.PACKAGES
    }


class FakeTapGateway:
    """In-memory tap that refuses every operation the publisher must never perform."""

    def __init__(self) -> None:
        self.default_branch_name = DEFAULT_BRANCH
        self.branches: dict[str, str] = {DEFAULT_BRANCH: DEFAULT_BRANCH_COMMIT}
        self.files: dict[tuple[str, str], str] = {}
        self.pull_requests: dict[str, dict[str, Any]] = {}
        self.created_branches: list[str] = []
        self.writes: list[tuple[str, str]] = []
        self.opened: list[dict[str, Any]] = []
        self.writes_forbidden = False
        self.refusal: Exception | None = None
        self.write_refusal: Exception | None = None
        self.next_number = 41

    def default_branch(self) -> str:
        if self.refusal is not None:
            raise self.refusal
        return self.default_branch_name

    def default_branch_commit(self) -> str:
        return self.branches[self.default_branch_name]

    def file_text(self, ref: str, path: str) -> str | None:
        if ref not in self.branches:
            raise AssertionError(f"publisher read from unknown tap ref {ref}")
        return self.files.get((ref, path))

    def branch_commit(self, branch: str) -> str | None:
        return self.branches.get(branch)

    def create_branch(self, branch: str, commit: str) -> None:
        self._guard_write(f"create branch {branch}")
        if branch == self.default_branch_name:
            raise AssertionError("publisher attempted to create the tap default branch")
        if branch in self.branches:
            raise AssertionError(f"publisher attempted to recreate branch {branch}")
        if commit != self.branches[self.default_branch_name]:
            raise AssertionError(
                f"branch {branch} was based on {commit}, not the default branch head"
            )
        self.branches[branch] = commit
        self.created_branches.append(branch)

    def put_file(self, branch: str, path: str, text: str, message: str) -> str:
        self._guard_write(f"write {path} on {branch}")
        if branch == self.default_branch_name:
            raise AssertionError("publisher attempted to write the tap default branch")
        if branch not in self.branches:
            raise AssertionError(f"publisher wrote {path} to missing branch {branch}")
        if not message.strip():
            raise AssertionError(f"publisher wrote {path} with an empty commit message")
        self.files[(branch, path)] = text
        self.writes.append((branch, path))
        commit = format(len(self.writes), "040x")
        self.branches[branch] = commit
        return commit

    def pull_request_for_branch(self, branch: str) -> dict[str, Any] | None:
        pull_request = self.pull_requests.get(branch)
        return None if pull_request is None else dict(pull_request)

    def open_pull_request(
        self, *, head: str, base: str, title: str, body: str
    ) -> dict[str, Any]:
        self._guard_write(f"open a pull request for {head}")
        if head in self.pull_requests:
            raise AssertionError(f"publisher opened a second pull request for {head}")
        if head == self.default_branch_name:
            raise AssertionError("publisher opened a pull request from the tap default branch")
        if base != self.default_branch_name:
            raise AssertionError(f"publisher targeted unexpected base branch {base}")
        if not title.strip() or not body.strip():
            raise AssertionError("publisher opened a pull request without title or body")
        self.next_number += 1
        pull_request = {
            "url": f"https://github.com/{TAP_REPOSITORY}/pull/{self.next_number}",
            "state": "open",
            "head": head,
            "base": base,
            "merged": False,
            "number": self.next_number,
        }
        self.pull_requests[head] = pull_request
        self.opened.append(dict(pull_request))
        return dict(pull_request)

    def force_push(self, *arguments: Any, **keywords: Any) -> None:
        raise AssertionError("publisher attempted a force push on the tap")

    def delete_branch(self, *arguments: Any, **keywords: Any) -> None:
        raise AssertionError("publisher attempted to delete a tap branch")

    def close_pull_request(self, *arguments: Any, **keywords: Any) -> None:
        raise AssertionError("publisher attempted to close an existing pull request")

    def merge_pull_request(self, *arguments: Any, **keywords: Any) -> None:
        raise AssertionError("publisher attempted to merge an existing pull request")

    def package_version(self, *arguments: Any, **keywords: Any) -> None:
        raise AssertionError("tap gateway was asked for Chocolatey package state")

    def push(self, *arguments: Any, **keywords: Any) -> None:
        raise AssertionError("tap gateway was asked to push a Chocolatey package")

    def place(self, ref: str, path: str, text: str) -> None:
        """Seed one observed tap file, creating the branch record when needed."""

        self.branches.setdefault(ref, DEFAULT_BRANCH_COMMIT)
        self.files[(ref, path)] = text

    def record_pull_request(
        self, branch: str, *, state: str = "open", merged: bool = False
    ) -> dict[str, Any]:
        """Seed one existing pull request without letting the publisher open it."""

        self.next_number += 1
        pull_request = {
            "url": f"https://github.com/{TAP_REPOSITORY}/pull/{self.next_number}",
            "state": state,
            "head": branch,
            "base": self.default_branch_name,
            "merged": merged,
            "number": self.next_number,
        }
        self.pull_requests[branch] = pull_request
        return pull_request

    def _guard_write(self, description: str) -> None:
        if self.writes_forbidden:
            raise AssertionError(
                f"publisher attempted to {description} while the pair was blocked"
            )
        if self.write_refusal is not None:
            raise self.write_refusal


class FakeCommunityGateway:
    """In-memory Community Repository that refuses a second or premature push."""

    def __init__(self) -> None:
        self.entries: dict[tuple[str, str], dict[str, Any]] = {}
        self.queries: list[tuple[str, str]] = []
        self.pushes: list[str] = []
        self.pushes_forbidden = False
        self.query_error: dict[str, Exception] = {}
        self.push_error: dict[str, Exception] = {}
        self.push_response: dict[str, dict[str, Any]] = {}

    def package_version(self, package_id: str, version: str) -> dict[str, Any] | None:
        self._assert_known(package_id)
        if self.pushes:
            raise AssertionError(
                f"publisher queried {package_id} after pushing {self.pushes}"
            )
        self.queries.append((package_id, version))
        error = self.query_error.get(package_id)
        if error is not None:
            raise error
        entry = self.entries.get((package_id, version))
        return None if entry is None else dict(entry)

    def push(self, path: Path, *, package_id: str, version: str) -> dict[str, Any]:
        self._assert_known(package_id)
        if self.pushes_forbidden:
            raise AssertionError(
                f"publisher pushed {package_id} while the pair was blocked"
            )
        if package_id in self.pushes:
            raise AssertionError(f"publisher pushed {package_id} twice in one run")
        if len(self.queries) != len(channels.PACKAGES):
            raise AssertionError(
                f"publisher pushed {package_id} after only {self.queries!r}"
            )
        if (package_id, version) in self.entries:
            raise AssertionError(
                f"publisher pushed over existing {package_id} {version}"
            )
        if path.name != f"{package_id}.{version}.nupkg":
            raise AssertionError(f"publisher pushed unexpected file {path.name}")
        self.pushes.append(package_id)
        error = self.push_error.get(package_id)
        if error is not None:
            raise error
        self.entries[(package_id, version)] = {
            "version": version,
            "listed": False,
            "moderation_status": "pending",
            "package_hash": hashlib.sha512(path.read_bytes()).hexdigest(),
            "package_hash_algorithm": "SHA512",
        }
        response = {"package_id": package_id, "version": version, "status": "accepted"}
        response.update(self.push_response.get(package_id, {}))
        return response

    def default_branch(self, *arguments: Any, **keywords: Any) -> None:
        raise AssertionError("community gateway was asked for the tap default branch")

    def file_text(self, *arguments: Any, **keywords: Any) -> None:
        raise AssertionError("community gateway was asked to read a tap file")

    def put_file(self, *arguments: Any, **keywords: Any) -> None:
        raise AssertionError("community gateway was asked to write a tap file")

    def open_pull_request(self, *arguments: Any, **keywords: Any) -> None:
        raise AssertionError("community gateway was asked to open a tap pull request")

    def publish(
        self,
        package_id: str,
        version: str,
        *,
        package_hash: str,
        moderation_status: str = "approved",
        listed: bool = True,
        algorithm: str = "SHA512",
    ) -> None:
        """Seed one observed Community Repository package version."""

        self.entries[(package_id, version)] = {
            "version": version,
            "listed": listed,
            "moderation_status": moderation_status,
            "package_hash": package_hash,
            "package_hash_algorithm": algorithm,
        }

    @staticmethod
    def _assert_known(package_id: str) -> None:
        if package_id not in channels.PACKAGE_BY_ID:
            raise AssertionError(f"unexpected Chocolatey package id {package_id}")


class HomebrewTapTests(unittest.TestCase):
    """Cover paired tap creation, resumption, idempotency, and hard conflicts."""

    def setUp(self) -> None:
        """Build validated inputs and both generated Formula candidates."""

        self.inputs = package_inputs()
        self.formulae = generated_formulae(self.inputs)
        self.first, self.second = channels.PACKAGES

    def reconcile(self, gateway: FakeTapGateway) -> package_publish.TapOutcome:
        """Reconcile the tap with the fixture inputs and generated pair."""

        return package_publish.reconcile_tap(gateway, self.inputs, self.formulae)

    def assert_no_writes(self, gateway: FakeTapGateway) -> None:
        """Assert the blocked channel performed no branch, file, or review write."""

        self.assertEqual(gateway.writes, [])
        self.assertEqual(gateway.created_branches, [])
        self.assertEqual(gateway.opened, [])

    def test_tap_branch_names_the_exact_version(self) -> None:
        """Derive one immutable version branch from validated inputs."""

        self.assertEqual(package_publish.tap_branch(self.inputs), BRANCH)

    def test_absent_pair_creates_one_branch_two_files_and_one_pull_request(self) -> None:
        """Propose both Formulae in a single protected version-specific change."""

        gateway = FakeTapGateway()
        outcome = self.reconcile(gateway)

        self.assertEqual(outcome.branch, BRANCH)
        self.assertEqual(
            outcome.formula_states,
            {self.first.package_id: "created", self.second.package_id: "created"},
        )
        self.assertEqual(gateway.created_branches, [BRANCH])
        self.assertEqual(
            gateway.writes,
            [(BRANCH, self.first.formula_path), (BRANCH, self.second.formula_path)],
        )
        self.assertEqual(len(gateway.opened), 1)
        self.assertEqual(gateway.opened[0]["head"], BRANCH)
        self.assertEqual(gateway.opened[0]["base"], DEFAULT_BRANCH)
        self.assertEqual(outcome.pull_request_url, gateway.opened[0]["url"])
        self.assertEqual(
            gateway.files[(BRANCH, self.first.formula_path)],
            self.formulae[self.first.package_id],
        )

    def test_member_identical_on_default_branch_is_unchanged_and_preserved(self) -> None:
        """Keep a merged Formula untouched and write only the absent pair member."""

        gateway = FakeTapGateway()
        gateway.place(
            DEFAULT_BRANCH, self.first.formula_path, self.formulae[self.first.package_id]
        )
        outcome = self.reconcile(gateway)

        self.assertEqual(
            outcome.formula_states,
            {self.first.package_id: "unchanged", self.second.package_id: "created"},
        )
        self.assertEqual(gateway.writes, [(BRANCH, self.second.formula_path)])
        self.assertEqual(gateway.created_branches, [BRANCH])
        self.assertEqual(len(gateway.opened), 1)

    def test_member_identical_on_version_branch_with_open_pull_request_resumes(self) -> None:
        """Resume the existing protected change rather than opening a second one."""

        gateway = FakeTapGateway()
        gateway.place(
            BRANCH, self.first.formula_path, self.formulae[self.first.package_id]
        )
        existing = gateway.record_pull_request(BRANCH)
        outcome = self.reconcile(gateway)

        self.assertEqual(
            outcome.formula_states,
            {self.first.package_id: "resumed", self.second.package_id: "created"},
        )
        self.assertEqual(gateway.created_branches, [])
        self.assertEqual(gateway.writes, [(BRANCH, self.second.formula_path)])
        self.assertEqual(gateway.opened, [])
        self.assertEqual(outcome.pull_request_url, existing["url"])

    def test_complete_identical_pair_already_merged_performs_no_write(self) -> None:
        """Report an already-merged identical pair as an idempotent success."""

        gateway = FakeTapGateway()
        for identity in channels.PACKAGES:
            gateway.place(
                DEFAULT_BRANCH,
                identity.formula_path,
                self.formulae[identity.package_id],
            )
        merged = gateway.record_pull_request(BRANCH, state="closed", merged=True)
        gateway.writes_forbidden = True
        outcome = self.reconcile(gateway)

        self.assertEqual(
            outcome.formula_states,
            {self.first.package_id: "unchanged", self.second.package_id: "unchanged"},
        )
        self.assertEqual(outcome.pull_request_url, merged["url"])
        self.assert_no_writes(gateway)

    def test_duplicate_publisher_run_is_idempotent(self) -> None:
        """Resume the same branch and pull request instead of duplicating work."""

        gateway = FakeTapGateway()
        first_outcome = self.reconcile(gateway)
        writes = list(gateway.writes)

        second_outcome = self.reconcile(gateway)
        self.assertEqual(
            second_outcome.formula_states,
            {self.first.package_id: "resumed", self.second.package_id: "resumed"},
        )
        self.assertEqual(gateway.writes, writes)
        self.assertEqual(len(gateway.opened), 1)
        self.assertEqual(gateway.created_branches, [BRANCH])
        self.assertEqual(second_outcome.pull_request_url, first_outcome.pull_request_url)

    def test_matching_provenance_with_different_bytes_is_rewritten_once(self) -> None:
        """Refresh a same-provenance but non-identical branch file without a new review."""

        gateway = FakeTapGateway()
        for identity in channels.PACKAGES:
            gateway.place(
                BRANCH,
                identity.formula_path,
                formula_text(self.inputs, identity, trailer="\n# regenerated\n"),
            )
        existing = gateway.record_pull_request(BRANCH)
        outcome = self.reconcile(gateway)

        self.assertEqual(
            outcome.formula_states,
            {self.first.package_id: "created", self.second.package_id: "created"},
        )
        self.assertEqual(
            gateway.writes,
            [(BRANCH, self.first.formula_path), (BRANCH, self.second.formula_path)],
        )
        self.assertEqual(gateway.created_branches, [])
        self.assertEqual(gateway.opened, [])
        self.assertEqual(outcome.pull_request_url, existing["url"])

    def assert_conflict_reports_pair(self, gateway: FakeTapGateway) -> str:
        """Assert a blocked tap change names both members and wrote nothing."""

        gateway.writes_forbidden = True
        with self.assertRaises(package_publish.PublicationError) as raised:
            self.reconcile(gateway)
        message = str(raised.exception)
        for identity in channels.PACKAGES:
            self.assertIn(f"{identity.package_id}: expected", message)
            self.assertIn(f"{identity.package_id}: observed on", message)
        self.assert_no_writes(gateway)
        return message

    def test_conflicting_archive_url_blocks_the_pair_without_writing(self) -> None:
        """Never overwrite a Formula version that pins another release archive."""

        gateway = FakeTapGateway()
        foreign = (
            f"https://github.com/{REPOSITORY}/releases/download/v0.1.9/"
            "skillmount-v0.1.9-aarch64-apple-darwin.tar.gz"
        )
        gateway.place(
            DEFAULT_BRANCH,
            self.first.formula_path,
            formula_text(self.inputs, self.first, archive_url=foreign),
        )
        message = self.assert_conflict_reports_pair(gateway)
        self.assertIn(foreign, message)
        self.assertIn(self.inputs.archive(channels.MACOS_ARM64.triple).url, message)

    def test_conflicting_archive_sha256_blocks_the_pair(self) -> None:
        """Treat a differing release-archive digest for the same URL as a hard conflict."""

        gateway = FakeTapGateway()
        foreign = digest("other-archive")
        gateway.place(
            BRANCH,
            self.second.formula_path,
            formula_text(self.inputs, self.second, archive_sha256=foreign),
        )
        message = self.assert_conflict_reports_pair(gateway)
        self.assertIn(foreign, message)
        self.assertIn(
            self.inputs.archive(channels.MACOS_ARM64.triple).sha256,
            message,
        )

    def test_conflicting_version_blocks_the_pair(self) -> None:
        """Refuse to reconcile a Formula that declares another package version."""

        gateway = FakeTapGateway()
        gateway.place(
            DEFAULT_BRANCH,
            self.first.formula_path,
            formula_text(self.inputs, self.first, version="0.1.9"),
        )
        message = self.assert_conflict_reports_pair(gateway)
        self.assertIn("version=0.1.9", message)
        self.assertIn(f"version={VERSION}", message)

    def test_conflicting_selected_binary_blocks_the_pair(self) -> None:
        """Refuse a Formula that installs the pair member's release binary."""

        gateway = FakeTapGateway()
        gateway.place(
            DEFAULT_BRANCH,
            self.first.formula_path,
            formula_text(
                self.inputs,
                self.first,
                installed_binary=self.second.command,
            ),
        )
        message = self.assert_conflict_reports_pair(gateway)
        self.assertIn(f"installed={self.second.command}", message)
        self.assertIn(f"installed={self.first.command}", message)

    def test_conflicting_selected_command_blocks_the_pair(self) -> None:
        """Refuse a Formula that generates completions for the other command."""

        gateway = FakeTapGateway()
        gateway.place(
            BRANCH,
            self.first.formula_path,
            formula_text(self.inputs, self.first, command=self.second.command),
        )
        message = self.assert_conflict_reports_pair(gateway)
        self.assertIn(f"command={self.second.command}", message)

    def test_unparseable_existing_formula_is_a_conflict(self) -> None:
        """Fail closed when an existing Formula's provenance cannot be observed."""

        gateway = FakeTapGateway()
        gateway.place(
            DEFAULT_BRANCH, self.second.formula_path, "class Broken < Formula\nend\n"
        )
        message = self.assert_conflict_reports_pair(gateway)
        self.assertIn("unparsed", message)

    def test_existing_pull_request_for_another_version_is_untouched(self) -> None:
        """Leave another version's open review alone and propose only this version."""

        gateway = FakeTapGateway()
        other = gateway.record_pull_request(OTHER_BRANCH)
        outcome = self.reconcile(gateway)

        self.assertEqual(outcome.branch, BRANCH)
        self.assertEqual([opened["head"] for opened in gateway.opened], [BRANCH])
        self.assertEqual(gateway.pull_requests[OTHER_BRANCH], other)
        self.assertEqual(gateway.pull_requests[OTHER_BRANCH]["state"], "open")
        self.assertNotEqual(outcome.pull_request_url, other["url"])

    def test_closed_unmerged_pull_request_blocks_outstanding_work(self) -> None:
        """Refuse to bypass a review that a human closed without merging."""

        gateway = FakeTapGateway()
        closed = gateway.record_pull_request(BRANCH, state="closed", merged=False)
        gateway.writes_forbidden = True
        with self.assertRaises(package_publish.PublicationError) as raised:
            self.reconcile(gateway)
        self.assertIn(closed["url"], str(raised.exception))
        self.assertIn("refusing to reopen", str(raised.exception))
        self.assert_no_writes(gateway)

    def test_retargeted_pull_request_blocks_the_pair(self) -> None:
        """Refuse a review that no longer targets the protected default branch."""

        gateway = FakeTapGateway()
        gateway.record_pull_request(BRANCH)["base"] = "release"
        gateway.writes_forbidden = True
        with self.assertRaises(package_publish.PublicationError) as raised:
            self.reconcile(gateway)
        self.assertIn("expected 'main'", str(raised.exception))
        self.assert_no_writes(gateway)

    def test_unavailable_tap_surfaces_as_publication_error_without_writes(self) -> None:
        """Stop before any write when tap ownership cannot be proven."""

        gateway = FakeTapGateway()
        gateway.refusal = package_publish.PublicationError(
            f"tap repository {TAP_REPOSITORY} is unavailable"
        )
        gateway.writes_forbidden = True
        with self.assertRaisesRegex(package_publish.PublicationError, "is unavailable"):
            self.reconcile(gateway)
        self.assert_no_writes(gateway)

    def test_revoked_token_refusal_never_opens_a_pull_request(self) -> None:
        """Surface an incorrectly scoped tap token before any review is created."""

        gateway = FakeTapGateway()
        gateway.write_refusal = package_publish.PublicationError(
            f"tap app token lacks contents:write on {TAP_REPOSITORY}"
        )
        with self.assertRaisesRegex(
            package_publish.PublicationError, "lacks contents:write"
        ):
            self.reconcile(gateway)
        self.assert_no_writes(gateway)

    def test_version_branch_equal_to_default_branch_is_refused(self) -> None:
        """Never accept a configuration that would write the tap default branch."""

        gateway = FakeTapGateway()
        gateway.default_branch_name = BRANCH
        gateway.branches = {BRANCH: DEFAULT_BRANCH_COMMIT}
        gateway.writes_forbidden = True
        with self.assertRaisesRegex(
            package_publish.PublicationError, "collides with the tap default branch"
        ):
            self.reconcile(gateway)
        self.assert_no_writes(gateway)

    def test_incomplete_generated_pair_is_refused(self) -> None:
        """Refuse to reconcile the tap with only one generated Formula."""

        gateway = FakeTapGateway()
        gateway.writes_forbidden = True
        partial = {self.first.package_id: self.formulae[self.first.package_id]}
        with self.assertRaises(package_publish.PublicationError):
            package_publish.reconcile_tap(gateway, self.inputs, partial)
        self.assert_no_writes(gateway)

    def test_empty_generated_formula_is_refused(self) -> None:
        """Refuse an empty rendered Formula rather than committing blank bytes."""

        gateway = FakeTapGateway()
        gateway.writes_forbidden = True
        blank = dict(self.formulae)
        blank[self.second.package_id] = "   \n"
        with self.assertRaisesRegex(package_publish.PublicationError, "is empty"):
            package_publish.reconcile_tap(gateway, self.inputs, blank)
        self.assert_no_writes(gateway)


class ChocolateyCommunityTests(unittest.TestCase):
    """Cover moderation-aware, pair-aware, single-push Community reconciliation."""

    def setUp(self) -> None:
        """Write both deterministic nupkg candidates and their digests."""

        self.temporary = tempfile.TemporaryDirectory()
        self.root = Path(self.temporary.name)
        self.inputs = package_inputs()
        self.first, self.second = channels.PACKAGES
        self.nupkgs: dict[str, Path] = {}
        self.package_sha256s: dict[str, str] = {}
        self.package_hashes: dict[str, str] = {}
        for identity in channels.PACKAGES:
            path = self.root / channels.nupkg_name(identity, VERSION)
            path.write_bytes(f"nupkg:{identity.package_id}:{VERSION}\n".encode())
            self.nupkgs[identity.package_id] = path
            self.package_sha256s[identity.package_id] = hashlib.sha256(
                path.read_bytes()
            ).hexdigest()
            self.package_hashes[identity.package_id] = hashlib.sha512(
                path.read_bytes()
            ).hexdigest()

    def tearDown(self) -> None:
        """Remove the isolated candidate fixture."""

        self.temporary.cleanup()

    def reconcile(self, gateway: FakeCommunityGateway) -> dict[str, str]:
        """Reconcile both package ids against the fixture candidates."""

        return dict(
            package_publish.reconcile_chocolatey(
                gateway, self.inputs, self.nupkgs, self.package_sha256s
            )
        )

    def test_absent_pair_pushes_both_ids_exactly_once(self) -> None:
        """Push each absent version once, only after both ids were observed."""

        gateway = FakeCommunityGateway()
        states = self.reconcile(gateway)

        self.assertEqual(
            states,
            {self.first.package_id: "pending", self.second.package_id: "pending"},
        )
        self.assertEqual(
            gateway.pushes, [self.first.package_id, self.second.package_id]
        )
        self.assertEqual(
            gateway.queries,
            [(self.first.package_id, VERSION), (self.second.package_id, VERSION)],
        )

    def test_freshly_accepted_push_reports_pending_and_never_listed(self) -> None:
        """Never claim public listing from an upload response."""

        gateway = FakeCommunityGateway()
        for identity in channels.PACKAGES:
            gateway.push_response[identity.package_id] = {
                "moderation_status": "approved",
                "listed": True,
            }
        states = self.reconcile(gateway)

        self.assertEqual(set(states.values()), {"pending"})
        self.assertNotIn("listed", states.values())

    def test_absent_member_beside_matching_listed_member_pushes_only_the_absent(self) -> None:
        """Preserve an approved and listed member and push only the missing id."""

        gateway = FakeCommunityGateway()
        gateway.publish(
            self.first.package_id,
            VERSION,
            package_hash=self.package_hashes[self.first.package_id],
        )
        states = self.reconcile(gateway)

        self.assertEqual(
            states,
            {self.first.package_id: "listed", self.second.package_id: "pending"},
        )
        self.assertEqual(gateway.pushes, [self.second.package_id])

    def test_absent_member_beside_matching_pending_member_pushes_only_the_absent(self) -> None:
        """Push the absent id while the other id still awaits moderation."""

        gateway = FakeCommunityGateway()
        gateway.publish(
            self.second.package_id,
            VERSION,
            package_hash=self.package_hashes[self.second.package_id],
            moderation_status="pending",
            listed=False,
        )
        states = self.reconcile(gateway)

        self.assertEqual(
            states,
            {self.first.package_id: "pending", self.second.package_id: "pending"},
        )
        self.assertEqual(gateway.pushes, [self.first.package_id])

    def test_matching_listed_pair_pushes_nothing(self) -> None:
        """Treat a complete matching approved pair as an idempotent success."""

        gateway = FakeCommunityGateway()
        for identity in channels.PACKAGES:
            gateway.publish(
                identity.package_id,
                VERSION,
                package_hash=self.package_hashes[identity.package_id],
            )
        gateway.pushes_forbidden = True
        states = self.reconcile(gateway)

        self.assertEqual(
            states, {self.first.package_id: "listed", self.second.package_id: "listed"}
        )
        self.assertEqual(gateway.pushes, [])

    def test_matching_pending_pair_pushes_nothing(self) -> None:
        """Never repush a pair that the repository already accepted."""

        gateway = FakeCommunityGateway()
        for identity in channels.PACKAGES:
            gateway.publish(
                identity.package_id,
                VERSION,
                package_hash=self.package_hashes[identity.package_id],
                moderation_status="pending",
                listed=False,
            )
        gateway.pushes_forbidden = True
        states = self.reconcile(gateway)

        self.assertEqual(
            states,
            {self.first.package_id: "pending", self.second.package_id: "pending"},
        )
        self.assertEqual(gateway.pushes, [])

    def test_divergent_moderation_states_are_reported_separately(self) -> None:
        """Report one listed and one pending member without collapsing them."""

        gateway = FakeCommunityGateway()
        gateway.publish(
            self.first.package_id,
            VERSION,
            package_hash=self.package_hashes[self.first.package_id],
        )
        gateway.publish(
            self.second.package_id,
            VERSION,
            package_hash=self.package_hashes[self.second.package_id],
            moderation_status="pending",
            listed=False,
        )
        gateway.pushes_forbidden = True
        states = self.reconcile(gateway)

        self.assertEqual(
            states,
            {self.first.package_id: "listed", self.second.package_id: "pending"},
        )

    def test_approved_but_not_publicly_resolved_member_is_unchanged(self) -> None:
        """Distinguish approved metadata from a publicly resolved package."""

        gateway = FakeCommunityGateway()
        for identity in channels.PACKAGES:
            gateway.publish(
                identity.package_id,
                VERSION,
                package_hash=self.package_hashes[identity.package_id],
                listed=False,
            )
        gateway.pushes_forbidden = True
        states = self.reconcile(gateway)

        self.assertEqual(
            states,
            {self.first.package_id: "unchanged", self.second.package_id: "unchanged"},
        )

    def test_base64_package_hash_is_accepted(self) -> None:
        """Accept the repository's base64 digest encoding for the same bytes."""

        gateway = FakeCommunityGateway()
        for identity in channels.PACKAGES:
            raw = bytes.fromhex(self.package_hashes[identity.package_id])
            gateway.publish(
                identity.package_id,
                VERSION,
                package_hash=base64.b64encode(raw).decode("ascii"),
            )
        gateway.pushes_forbidden = True
        states = self.reconcile(gateway)

        self.assertEqual(
            states, {self.first.package_id: "listed", self.second.package_id: "listed"}
        )

    def assert_pair_blocked(self, gateway: FakeCommunityGateway) -> str:
        """Assert a blocked pair reported both identities and pushed nothing."""

        gateway.pushes_forbidden = True
        with self.assertRaises(package_publish.PublicationError) as raised:
            self.reconcile(gateway)
        message = str(raised.exception)
        for identity in channels.PACKAGES:
            self.assertIn(f"{identity.package_id}: expected", message)
            self.assertIn(f"{identity.package_id}: observed", message)
        self.assertEqual(gateway.pushes, [])
        return message

    def test_package_hash_mismatch_blocks_both_ids(self) -> None:
        """Never repush or overwrite a version whose published bytes differ."""

        gateway = FakeCommunityGateway()
        foreign = hashlib.sha512(b"foreign-nupkg").hexdigest()
        gateway.publish(self.first.package_id, VERSION, package_hash=foreign)
        message = self.assert_pair_blocked(gateway)
        self.assertIn(foreign, message)
        self.assertIn(self.package_hashes[self.first.package_id], message)

    def test_wrong_hash_algorithm_is_a_conflict_not_a_pass(self) -> None:
        """Refuse a member whose digest cannot be compared under SHA-512."""

        gateway = FakeCommunityGateway()
        gateway.publish(
            self.second.package_id,
            VERSION,
            package_hash=self.package_hashes[self.second.package_id],
            algorithm="SHA256",
        )
        message = self.assert_pair_blocked(gateway)
        self.assertIn("'SHA256'", message)
        self.assertIn("expected 'SHA512'", message)

    def test_rejected_member_blocks_both_ids(self) -> None:
        """Stop the pair for human review when moderation rejected a member."""

        gateway = FakeCommunityGateway()
        gateway.publish(
            self.first.package_id,
            VERSION,
            package_hash=self.package_hashes[self.first.package_id],
            moderation_status="rejected",
            listed=False,
        )
        message = self.assert_pair_blocked(gateway)
        self.assertIn("rejected by moderation", message)

    def test_unknown_moderation_status_blocks_both_ids(self) -> None:
        """Fail closed on a moderation state the publisher cannot interpret."""

        gateway = FakeCommunityGateway()
        gateway.publish(
            self.second.package_id,
            VERSION,
            package_hash=self.package_hashes[self.second.package_id],
            moderation_status="quarantined",
        )
        message = self.assert_pair_blocked(gateway)
        self.assertIn("'quarantined'", message)

    def test_version_mismatch_blocks_both_ids(self) -> None:
        """Reject a feed entry that reports another package version."""

        gateway = FakeCommunityGateway()
        gateway.entries[(self.first.package_id, VERSION)] = {
            "version": "0.1.9",
            "listed": True,
            "moderation_status": "approved",
            "package_hash": self.package_hashes[self.first.package_id],
            "package_hash_algorithm": "SHA512",
        }
        message = self.assert_pair_blocked(gateway)
        self.assertIn("'0.1.9'", message)

    def test_api_outage_blocks_the_pair_without_pushing(self) -> None:
        """Never push when either package id's state could not be observed."""

        gateway = FakeCommunityGateway()
        gateway.query_error[self.second.package_id] = OSError(
            "community.chocolatey.org is unreachable"
        )
        gateway.pushes_forbidden = True
        with self.assertRaisesRegex(
            package_publish.PublicationError, "neither package id was pushed"
        ):
            self.reconcile(gateway)
        self.assertEqual(gateway.pushes, [])

    def test_push_failure_on_one_id_never_pushes_or_retries_the_other(self) -> None:
        """Stop the run on the first failed upload without touching the other id."""

        gateway = FakeCommunityGateway()
        gateway.push_error[self.first.package_id] = OSError("gateway timeout")
        with self.assertRaisesRegex(
            package_publish.PublicationError, "was not retried"
        ):
            self.reconcile(gateway)
        self.assertEqual(gateway.pushes, [self.first.package_id])
        self.assertEqual(gateway.entries, {})

    def test_second_push_failure_preserves_the_accepted_first_member(self) -> None:
        """Retry only the member missing after a non-atomic second upload failure."""

        gateway = FakeCommunityGateway()
        gateway.push_error[self.second.package_id] = OSError("forbidden package id")
        with self.assertRaisesRegex(
            package_publish.PublicationError, "was not retried"
        ):
            self.reconcile(gateway)
        self.assertEqual(
            gateway.pushes, [self.first.package_id, self.second.package_id]
        )
        self.assertIn((self.first.package_id, VERSION), gateway.entries)
        self.assertNotIn((self.second.package_id, VERSION), gateway.entries)

        gateway.push_error.clear()
        gateway.queries.clear()
        gateway.pushes.clear()
        states = self.reconcile(gateway)

        self.assertEqual(
            states,
            {self.first.package_id: "pending", self.second.package_id: "pending"},
        )
        self.assertEqual(gateway.pushes, [self.second.package_id])

    def test_duplicate_run_performs_no_second_push(self) -> None:
        """Observe the accepted pair on retry instead of pushing either id again."""

        gateway = FakeCommunityGateway()
        self.assertEqual(set(self.reconcile(gateway).values()), {"pending"})
        pushes = list(gateway.pushes)

        gateway.queries.clear()
        gateway.pushes.clear()
        gateway.pushes_forbidden = True
        states = self.reconcile(gateway)

        self.assertEqual(
            states,
            {self.first.package_id: "pending", self.second.package_id: "pending"},
        )
        self.assertEqual(gateway.pushes, [])
        self.assertEqual(pushes, [self.first.package_id, self.second.package_id])

    def test_rejected_upload_response_fails_closed(self) -> None:
        """Refuse to report an upload the repository rejected outright."""

        gateway = FakeCommunityGateway()
        gateway.push_response[self.first.package_id] = {
            "moderation_status": "rejected",
            "message": "duplicate packaging",
        }
        with self.assertRaisesRegex(
            package_publish.PublicationError, "duplicate packaging"
        ):
            self.reconcile(gateway)
        self.assertEqual(gateway.pushes, [self.first.package_id])

    def test_local_digest_mismatch_blocks_before_any_query(self) -> None:
        """Prove the candidate digest from bytes before contacting the repository."""

        gateway = FakeCommunityGateway()
        self.package_sha256s[self.second.package_id] = digest("stale-digest")
        with self.assertRaisesRegex(package_publish.PublicationError, "hashes to"):
            self.reconcile(gateway)
        self.assertEqual(gateway.queries, [])
        self.assertEqual(gateway.pushes, [])

    def test_unexpected_candidate_name_blocks_before_any_query(self) -> None:
        """Refuse a candidate filename that does not name its id and version."""

        gateway = FakeCommunityGateway()
        renamed = self.root / f"{self.first.package_id}.0.1.9.nupkg"
        renamed.write_bytes(self.nupkgs[self.first.package_id].read_bytes())
        self.nupkgs[self.first.package_id] = renamed
        with self.assertRaisesRegex(package_publish.PublicationError, "expected"):
            self.reconcile(gateway)
        self.assertEqual(gateway.queries, [])

    def test_missing_candidate_blocks_before_any_query(self) -> None:
        """Refuse to reconcile when a packed candidate is absent."""

        gateway = FakeCommunityGateway()
        self.nupkgs[self.second.package_id].unlink()
        with self.assertRaisesRegex(
            package_publish.PublicationError, "is not a regular file"
        ):
            self.reconcile(gateway)
        self.assertEqual(gateway.queries, [])

    def test_incomplete_candidate_pair_is_refused(self) -> None:
        """Require exactly one candidate and digest per package identity."""

        gateway = FakeCommunityGateway()
        del self.nupkgs[self.second.package_id]
        with self.assertRaises(package_publish.PublicationError):
            self.reconcile(gateway)
        self.assertEqual(gateway.queries, [])


class ChocolateyGatewayTests(unittest.TestCase):
    """Cover the distinct official read and write Community endpoints."""

    def test_gateway_parses_sha512_and_proves_listing_with_supported_cli(self) -> None:
        """Bind real OData hash fields to an exact public `choco search` result."""

        package_bytes = b"candidate"
        package_hash = base64.b64encode(hashlib.sha512(package_bytes).digest()).decode()
        body = f"""\
<entry xmlns="http://www.w3.org/2005/Atom"
       xmlns:d="http://schemas.microsoft.com/ado/2007/08/dataservices"
       xmlns:m="http://schemas.microsoft.com/ado/2007/08/dataservices/metadata">
  <m:properties>
    <d:Version>{VERSION}</d:Version>
    <d:IsApproved m:type="Edm.Boolean">true</d:IsApproved>
    <d:PackageStatus>Approved</d:PackageStatus>
    <d:PackageHash>{package_hash}</d:PackageHash>
    <d:PackageHashAlgorithm>SHA512</d:PackageHashAlgorithm>
  </m:properties>
</entry>
""".encode()
        response = mock.MagicMock()
        response.__enter__.return_value.read.return_value = body
        completed = mock.Mock(
            returncode=0,
            stdout=f"skillmount|{VERSION}\n".encode(),
            stderr=b"",
        )
        with tempfile.TemporaryDirectory() as directory, mock.patch.dict(
            package_publish.os.environ,
            {"CHOCOLATEY_API_KEY": "test-api-key"},
        ):
            gateway = package_publish.ChocolateyGateway(
                working_directory=Path(directory)
            )
            with mock.patch.object(
                package_publish.urllib.request, "urlopen", return_value=response
            ), mock.patch.object(
                package_publish.subprocess, "run", return_value=completed
            ) as run:
                observed = gateway.package_version("skillmount", VERSION)

        self.assertEqual(
            observed,
            {
                "version": VERSION,
                "listed": True,
                "moderation_status": "approved",
                "package_hash": package_hash,
                "package_hash_algorithm": "SHA512",
            },
        )
        self.assertEqual(
            package_publish.normalized_sha512_hash(observed["package_hash"]),
            hashlib.sha512(package_bytes).hexdigest(),
        )
        arguments = run.call_args.args[0]
        self.assertEqual(
            arguments,
            [
                "choco",
                "search",
                "skillmount",
                f"--version={VERSION}",
                "--exact",
                "--all-versions",
                "--approved-only",
                "--limit-output",
                "--source",
                package_publish.COMMUNITY_QUERY_SOURCE,
            ],
        )
        self.assertNotIn("CHOCOLATEY_API_KEY", run.call_args.kwargs["env"])

    def test_gateway_queries_the_feed_and_pushes_to_the_upload_endpoint(self) -> None:
        """Never send a package upload to the read-only OData endpoint."""

        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            candidate = root / f"skillmount.{VERSION}.nupkg"
            candidate.write_bytes(b"candidate")
            with mock.patch.dict(
                package_publish.os.environ,
                {"CHOCOLATEY_API_KEY": "test-api-key"},
            ):
                gateway = package_publish.ChocolateyGateway(
                    working_directory=root
                )

            not_found = package_publish.urllib.error.HTTPError(
                "https://example.invalid", 404, "not found", {}, None
            )
            with mock.patch.object(
                package_publish.urllib.request,
                "urlopen",
                side_effect=not_found,
            ) as urlopen:
                self.assertIsNone(gateway.package_version("skillmount", VERSION))
            not_found.close()
            request = urlopen.call_args.args[0]
            self.assertTrue(
                request.full_url.startswith(
                    f"{package_publish.COMMUNITY_QUERY_SOURCE}/Packages("
                )
            )

            completed = mock.Mock(returncode=0, stdout=b"accepted", stderr=b"")
            with mock.patch.object(
                package_publish.subprocess, "run", return_value=completed
            ) as run:
                response = gateway.push(
                    candidate,
                    package_id="skillmount",
                    version=VERSION,
                )
            arguments = run.call_args.args[0]
            source_index = arguments.index("--source") + 1
            self.assertEqual(
                arguments[source_index], package_publish.COMMUNITY_PUSH_SOURCE
            )
            self.assertNotIn(package_publish.COMMUNITY_QUERY_SOURCE, arguments)
            self.assertEqual(
                response["source"], package_publish.COMMUNITY_PUSH_SOURCE
            )


class PublisherHelperTests(unittest.TestCase):
    """Cover the pure helpers both lanes depend on."""

    def test_normalized_sha512_hash_accepts_hex_and_base64(self) -> None:
        """Normalize both published SHA-512 encodings to lowercase hex."""

        expected = hashlib.sha512(b"candidate").hexdigest()
        raw = bytes.fromhex(expected)
        self.assertEqual(package_publish.normalized_sha512_hash(expected), expected)
        self.assertEqual(
            package_publish.normalized_sha512_hash(expected.upper()), expected
        )
        self.assertEqual(
            package_publish.normalized_sha512_hash(
                base64.b64encode(raw).decode("ascii")
            ),
            expected,
        )

    def test_normalized_sha512_hash_rejects_other_values(self) -> None:
        """Return None for a value that is not a SHA-512 digest."""

        for value in (None, "", "zz", digest("x"), base64.b64encode(b"short")):
            with self.subTest(value=value):
                self.assertIsNone(package_publish.normalized_sha512_hash(value))

    def test_format_states_reports_both_members_in_pair_order(self) -> None:
        """Render one workflow-output line naming each member's own state."""

        states = {
            channels.PACKAGES[0].package_id: "listed",
            channels.PACKAGES[1].package_id: "pending",
        }
        self.assertEqual(
            package_publish.format_states(states),
            f"{channels.PACKAGES[0].package_id}=listed "
            f"{channels.PACKAGES[1].package_id}=pending",
        )

    def test_formula_identity_derives_version_from_the_release_tag(self) -> None:
        """Read the package version from the pinned release-archive URL."""

        inputs = package_inputs()
        identity = channels.PACKAGES[0]
        observed = package_publish.formula_identity(formula_text(inputs, identity))
        self.assertEqual(
            observed, package_publish.expected_formula_identity(inputs, identity)
        )
        self.assertTrue(observed.complete)

    def test_formula_identity_is_incomplete_for_ambiguous_archive(self) -> None:
        """Refuse to guess provenance when a Formula declares two archive URLs."""

        inputs = package_inputs()
        identity = channels.PACKAGES[1]
        text = formula_text(inputs, identity) + '  url "https://example.invalid/a.gz"\n'
        observed = package_publish.formula_identity(text)
        self.assertFalse(observed.complete)
        self.assertIn("url=unparsed", observed.describe())


class PublisherCommandTests(unittest.TestCase):
    """Cover the argument_parser/run/main trio without any external process."""

    def setUp(self) -> None:
        """Stage validated inputs, generated Formulae, and packed candidates."""

        self.temporary = tempfile.TemporaryDirectory()
        self.root = Path(self.temporary.name)
        self.inputs = package_inputs()
        self.inputs_path = self.root / "inputs.json"
        self.inputs_path.write_text(
            json.dumps(inputs_document(), indent=2, sort_keys=True) + "\n",
            encoding="utf-8",
        )
        self.formula_directory = self.root / "candidates"
        for identity, text in generated_formulae(self.inputs).items():
            path = self.formula_directory / channels.package_for(identity).formula_path
            path.parent.mkdir(parents=True, exist_ok=True)
            path.write_text(text, encoding="utf-8")
        self.nupkg_directory = self.root / "nupkgs"
        self.nupkg_directory.mkdir()
        self.package_sha256s: dict[str, str] = {}
        self.package_hashes: dict[str, str] = {}
        for identity in channels.PACKAGES:
            path = self.nupkg_directory / channels.nupkg_name(identity, VERSION)
            path.write_bytes(f"nupkg:{identity.package_id}\n".encode())
            self.package_sha256s[identity.package_id] = hashlib.sha256(
                path.read_bytes()
            ).hexdigest()
            self.package_hashes[identity.package_id] = hashlib.sha512(
                path.read_bytes()
            ).hexdigest()
        self.report = self.root / "report.json"
        self.github_output = self.root / "outputs.txt"
        self.github_output.touch()

    def tearDown(self) -> None:
        """Remove the isolated command fixture."""

        self.temporary.cleanup()

    def install_tap_gateway(self, gateway: FakeTapGateway) -> None:
        """Replace the real tap gateway with a fake for one command run."""

        original = package_publish.GhTapGateway
        self.addCleanup(setattr, package_publish, "GhTapGateway", original)
        package_publish.GhTapGateway = lambda *arguments, **keywords: gateway

    def install_community_gateway(self, gateway: FakeCommunityGateway) -> None:
        """Replace the real Chocolatey gateway and pair inspection for one run."""

        original_gateway = package_publish.ChocolateyGateway
        original_inspect = channels.inspect_nupkg_pair
        self.addCleanup(setattr, package_publish, "ChocolateyGateway", original_gateway)
        self.addCleanup(setattr, channels, "inspect_nupkg_pair", original_inspect)
        package_publish.ChocolateyGateway = lambda *arguments, **keywords: gateway
        channels.inspect_nupkg_pair = lambda paths, inputs: dict(self.package_sha256s)

    def outputs(self) -> dict[str, str]:
        """Parse the appended single-line GitHub Actions outputs."""

        lines = self.github_output.read_text(encoding="utf-8").splitlines()
        return dict(line.split("=", 1) for line in lines if line)

    def test_argument_parser_requires_a_known_subcommand(self) -> None:
        """Expose exactly the homebrew and chocolatey publisher subcommands."""

        parser = package_publish.argument_parser()
        with self.assertRaises(SystemExit):
            parser.parse_args([])
        with self.assertRaises(SystemExit):
            parser.parse_args(["winget"])
        options = parser.parse_args(
            [
                "homebrew",
                "--inputs",
                str(self.inputs_path),
                "--formula-directory",
                str(self.formula_directory),
                "--tap-repository",
                TAP_REPOSITORY,
            ]
        )
        self.assertEqual(options.command, "homebrew")
        self.assertEqual(options.tap_repository, TAP_REPOSITORY)

    def test_homebrew_command_reports_both_states_and_writes_evidence(self) -> None:
        """Record the branch, review URL, and each Formula's own state."""

        gateway = FakeTapGateway()
        self.install_tap_gateway(gateway)
        status = package_publish.run(
            [
                "homebrew",
                "--inputs",
                str(self.inputs_path),
                "--formula-directory",
                str(self.formula_directory),
                "--tap-repository",
                TAP_REPOSITORY,
                "--report",
                str(self.report),
                "--github-output",
                str(self.github_output),
            ]
        )

        self.assertEqual(status, 0)
        report = json.loads(self.report.read_text(encoding="utf-8"))
        self.assertEqual(report["channel"], "homebrew")
        self.assertEqual(report["branch"], BRANCH)
        self.assertEqual(report["pull_request_url"], gateway.opened[0]["url"])
        self.assertEqual(
            report["formula_states"],
            {
                channels.PACKAGES[0].package_id: "created",
                channels.PACKAGES[1].package_id: "created",
            },
        )
        outputs = self.outputs()
        self.assertEqual(outputs["tap_branch"], BRANCH)
        self.assertEqual(outputs["tap_pull_request_url"], gateway.opened[0]["url"])
        self.assertEqual(
            outputs["homebrew_states"],
            f"{channels.PACKAGES[0].package_id}=created "
            f"{channels.PACKAGES[1].package_id}=created",
        )

    def test_chocolatey_command_reports_each_package_state(self) -> None:
        """Record both package ids' independently observed publication states."""

        gateway = FakeCommunityGateway()
        gateway.publish(
            channels.PACKAGES[0].package_id,
            VERSION,
            package_hash=self.package_hashes[channels.PACKAGES[0].package_id],
        )
        self.install_community_gateway(gateway)
        status = package_publish.run(
            [
                "chocolatey",
                "--inputs",
                str(self.inputs_path),
                "--nupkg-directory",
                str(self.nupkg_directory),
                "--report",
                str(self.report),
                "--github-output",
                str(self.github_output),
            ]
        )

        self.assertEqual(status, 0)
        report = json.loads(self.report.read_text(encoding="utf-8"))
        self.assertEqual(report["channel"], "chocolatey")
        self.assertEqual(
            report["query_source"], package_publish.COMMUNITY_QUERY_SOURCE
        )
        self.assertEqual(
            report["push_source"], package_publish.COMMUNITY_PUSH_SOURCE
        )
        self.assertEqual(
            report["package_states"],
            {
                channels.PACKAGES[0].package_id: "listed",
                channels.PACKAGES[1].package_id: "pending",
            },
        )
        self.assertEqual(gateway.pushes, [channels.PACKAGES[1].package_id])
        self.assertEqual(
            self.outputs()["chocolatey_states"],
            f"{channels.PACKAGES[0].package_id}=listed "
            f"{channels.PACKAGES[1].package_id}=pending",
        )

    def test_main_converts_a_channel_conflict_into_a_nonzero_status(self) -> None:
        """Report a blocked channel as a stable failure without writing evidence."""

        gateway = FakeTapGateway()
        gateway.place(
            DEFAULT_BRANCH,
            channels.PACKAGES[0].formula_path,
            formula_text(
                self.inputs,
                channels.PACKAGES[0],
                archive_sha256=digest("foreign-archive"),
            ),
        )
        gateway.writes_forbidden = True
        self.install_tap_gateway(gateway)
        status = package_publish.main(
            [
                "homebrew",
                "--inputs",
                str(self.inputs_path),
                "--formula-directory",
                str(self.formula_directory),
                "--tap-repository",
                TAP_REPOSITORY,
                "--report",
                str(self.report),
            ]
        )

        self.assertEqual(status, 1)
        self.assertFalse(self.report.exists())
        self.assertEqual(gateway.writes, [])
        self.assertEqual(gateway.opened, [])


if __name__ == "__main__":
    unittest.main()
