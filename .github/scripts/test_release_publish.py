#!/usr/bin/env python3
"""State-machine tests for controlled GitHub Release publication."""

from __future__ import annotations

import copy
import hashlib
import shutil
import tempfile
import unittest
from pathlib import Path
from typing import Any

import release
import release_publish

VERSION = "0.1.0"
TAG = "v0.1.0"
COMMIT = "a" * 40
REPOSITORY = "pashifika/skillmount"
RUN_URL = "https://github.com/pashifika/skillmount/actions/runs/123"


class FakeGateway:
    """In-memory GitHub release boundary with immutable tag evidence."""

    def __init__(self) -> None:
        self.tag_commit = COMMIT
        self.releases: list[dict[str, Any]] = []
        self.asset_bytes: dict[int, bytes] = {}
        self.next_release_id = 1
        self.next_asset_id = 100
        self.successful_uploads = 0
        self.fail_after_uploads: int | None = None
        self.deleted_assets: list[int] = []
        self.resolve_calls = 0
        self.release_calls = 0

    def resolve_tag_commit(self, repository: str, tag: str) -> str:
        self.assert_identity(repository, tag)
        self.resolve_calls += 1
        return self.tag_commit

    def list_releases(self, repository: str) -> list[dict[str, Any]]:
        self.assert_repository(repository)
        self.release_calls += 1
        return copy.deepcopy(self.releases)

    def create_draft(
        self, repository: str, *, tag: str, commit: str, body: str
    ) -> dict[str, Any]:
        self.assert_identity(repository, tag)
        if commit != self.tag_commit:
            raise AssertionError("publisher attempted a draft for the wrong commit")
        candidate = {
            "id": self.next_release_id,
            "tag_name": tag,
            "name": tag,
            "body": f"{body}\n\n## What's Changed\n\nGenerated notes fixture.",
            "draft": True,
            "prerelease": False,
            "immutable": False,
            "assets": [],
        }
        self.next_release_id += 1
        self.releases.append(candidate)
        return copy.deepcopy(candidate)

    def get_release(self, repository: str, release_id: int) -> dict[str, Any]:
        self.assert_repository(repository)
        return copy.deepcopy(self.release_by_id(release_id))

    def delete_asset(self, repository: str, asset_id: int) -> None:
        self.assert_repository(repository)
        for candidate in self.releases:
            for asset in candidate["assets"]:
                if asset["id"] == asset_id:
                    if candidate["draft"] is not True or asset["state"] != "open":
                        raise AssertionError("publisher deleted an uploaded or published asset")
                    candidate["assets"].remove(asset)
                    self.asset_bytes.pop(asset_id, None)
                    self.deleted_assets.append(asset_id)
                    return
        raise AssertionError(f"unknown fake asset {asset_id}")

    def upload_asset(self, repository: str, tag: str, path: Path) -> None:
        self.assert_identity(repository, tag)
        if (
            self.fail_after_uploads is not None
            and self.successful_uploads >= self.fail_after_uploads
        ):
            raise release_publish.PublishError("injected upload interruption")
        candidate = self.release_by_tag(tag)
        if candidate["draft"] is not True:
            raise AssertionError("publisher uploaded to a published release")
        self.add_asset(candidate, path.name, path.read_bytes())
        self.successful_uploads += 1

    def download_asset(self, repository: str, asset_id: int, path: Path) -> None:
        self.assert_repository(repository)
        path.write_bytes(self.asset_bytes[asset_id])

    def publish(self, repository: str, release_id: int) -> dict[str, Any]:
        self.assert_repository(repository)
        candidate = self.release_by_id(release_id)
        if candidate["draft"] is not True:
            raise AssertionError("publisher attempted a second publication transition")
        candidate["draft"] = False
        return copy.deepcopy(candidate)

    def add_asset(
        self,
        candidate: dict[str, Any],
        name: str,
        content: bytes,
        *,
        state: str = "uploaded",
        digest: str | None = None,
    ) -> dict[str, Any]:
        if any(asset["name"] == name for asset in candidate["assets"]):
            raise AssertionError(f"duplicate fake asset {name}")
        asset_id = self.next_asset_id
        self.next_asset_id += 1
        asset = {
            "id": asset_id,
            "name": name,
            "state": state,
            "size": len(content),
            "digest": (
                f"sha256:{hashlib.sha256(content).hexdigest()}"
                if digest is None and state == "uploaded"
                else digest
            ),
        }
        candidate["assets"].append(asset)
        self.asset_bytes[asset_id] = content
        return asset

    def release_by_tag(self, tag: str) -> dict[str, Any]:
        matches = [candidate for candidate in self.releases if candidate["tag_name"] == tag]
        if len(matches) != 1:
            raise AssertionError(f"expected one fake release for {tag}, found {len(matches)}")
        return matches[0]

    def release_by_id(self, release_id: int) -> dict[str, Any]:
        matches = [candidate for candidate in self.releases if candidate["id"] == release_id]
        if len(matches) != 1:
            raise AssertionError(
                f"expected one fake release ID {release_id}, found {len(matches)}"
            )
        return matches[0]

    @staticmethod
    def assert_repository(repository: str) -> None:
        if repository != REPOSITORY:
            raise AssertionError(f"unexpected repository {repository}")

    @classmethod
    def assert_identity(cls, repository: str, tag: str) -> None:
        cls.assert_repository(repository)
        if tag != TAG:
            raise AssertionError(f"unexpected tag {tag}")


class ReleasePublishTests(unittest.TestCase):
    """Cover draft creation, retries, conflicts, and remote byte verification."""

    def setUp(self) -> None:
        """Build a complete deterministic local release set."""

        self.temporary = tempfile.TemporaryDirectory()
        self.root = Path(self.temporary.name)
        repository = self.root / "repository"
        repository.mkdir()
        (repository / "LICENSE-APACHE").write_text("Apache fixture\n", encoding="utf-8")
        (repository / "LICENSE-MIT").write_text("MIT fixture\n", encoding="utf-8")
        workflow_artifacts = self.root / "workflow-artifacts"
        for target in release.TARGETS:
            binaries = self.root / "binaries" / target.triple
            binaries.mkdir(parents=True)
            for executable in release.executable_names(target):
                binary = binaries / executable
                binary.write_bytes(f"binary:{target.triple}:{executable}\n".encode())
                binary.chmod(0o755)
            release.package_release(
                repository,
                binaries,
                workflow_artifacts / target.name,
                target=target,
                version=VERSION,
                tag=TAG,
                commit=COMMIT,
            )
        self.assets = self.root / "verified-assets"
        release.aggregate_release(
            workflow_artifacts,
            self.assets,
            version=VERSION,
            tag=TAG,
            commit=COMMIT,
        )

    def tearDown(self) -> None:
        """Remove the isolated release fixture."""

        self.temporary.cleanup()

    def publish(self, gateway: FakeGateway) -> str:
        """Run the publisher with fixed validated fixture identity."""

        return release_publish.publish_release(
            gateway,
            REPOSITORY,
            self.assets,
            version=VERSION,
            tag=TAG,
            commit=COMMIT,
            run_url=RUN_URL,
        )

    def create_owned_draft(self, gateway: FakeGateway) -> dict[str, Any]:
        """Create a fake workflow-owned generated-notes draft."""

        return gateway.create_draft(
            REPOSITORY,
            tag=TAG,
            commit=COMMIT,
            body=release_publish.release_body(TAG, COMMIT, RUN_URL),
        )

    def test_complete_local_set_creates_verifies_and_publishes_once(self) -> None:
        """Publish only after all four uploaded assets round-trip byte-for-byte."""

        gateway = FakeGateway()
        self.assertEqual(self.publish(gateway), "published")
        candidate = gateway.release_by_tag(TAG)
        self.assertFalse(candidate["draft"])
        self.assertIn(release_publish.release_marker(TAG, COMMIT), candidate["body"])
        self.assertIn("Generated notes fixture", candidate["body"])
        self.assertEqual(
            sorted(asset["name"] for asset in candidate["assets"]),
            sorted((*release.expected_archive_names(TAG), release.CHECKSUM_FILE)),
        )
        self.assertEqual(gateway.tag_commit, COMMIT)
        self.assertGreaterEqual(gateway.resolve_calls, 3)

    def test_missing_local_asset_fails_before_remote_interaction(self) -> None:
        """Do not touch Releases when aggregate completeness is not proven."""

        (self.assets / release.expected_archive_names(TAG)[0]).unlink()
        gateway = FakeGateway()
        with self.assertRaises(release.ReleaseError):
            self.publish(gateway)
        self.assertEqual(gateway.resolve_calls, 0)
        self.assertEqual(gateway.release_calls, 0)
        self.assertEqual(gateway.releases, [])

    def test_remote_tag_mismatch_fails_before_draft_creation(self) -> None:
        """Never create or move a release when the remote tag identity changed."""

        gateway = FakeGateway()
        gateway.tag_commit = "b" * 40
        with self.assertRaises(release_publish.PublishError):
            self.publish(gateway)
        self.assertEqual(gateway.releases, [])
        self.assertEqual(gateway.tag_commit, "b" * 40)

    def test_upload_interruption_retains_and_resumes_matching_draft(self) -> None:
        """Leave partial uploads as a draft and resume only the same tag/commit marker."""

        gateway = FakeGateway()
        gateway.fail_after_uploads = 1
        with self.assertRaisesRegex(
            release_publish.PublishError, "retained for a same-tag retry"
        ):
            self.publish(gateway)
        candidate = gateway.release_by_tag(TAG)
        self.assertTrue(candidate["draft"])
        self.assertEqual(len(candidate["assets"]), 1)

        gateway.fail_after_uploads = None
        self.assertEqual(self.publish(gateway), "published")
        self.assertFalse(gateway.release_by_tag(TAG)["draft"])
        self.assertEqual(gateway.tag_commit, COMMIT)

    def test_matching_draft_with_existing_asset_uploads_only_missing_files(self) -> None:
        """Keep an identical completed asset and add only absent workflow assets."""

        gateway = FakeGateway()
        self.create_owned_draft(gateway)
        candidate = gateway.release_by_tag(TAG)
        existing_name = release.expected_archive_names(TAG)[0]
        gateway.add_asset(
            candidate, existing_name, (self.assets / existing_name).read_bytes()
        )

        self.assertEqual(self.publish(gateway), "published")
        self.assertEqual(gateway.successful_uploads, 3)

    def test_incomplete_open_asset_is_deleted_only_from_owned_draft(self) -> None:
        """Repair an interrupted open asset without clobbering uploaded bytes."""

        gateway = FakeGateway()
        self.create_owned_draft(gateway)
        candidate = gateway.release_by_tag(TAG)
        name = release.expected_archive_names(TAG)[0]
        incomplete = gateway.add_asset(
            candidate, name, b"partial", state="open", digest=None
        )

        self.assertEqual(self.publish(gateway), "published")
        self.assertEqual(gateway.deleted_assets, [incomplete["id"]])
        self.assertNotEqual(
            gateway.asset_bytes.get(incomplete["id"]),
            (self.assets / name).read_bytes(),
        )

    def test_conflicting_uploaded_asset_is_never_clobbered(self) -> None:
        """Reject immutable filename/content conflicts in a matching draft."""

        gateway = FakeGateway()
        self.create_owned_draft(gateway)
        candidate = gateway.release_by_tag(TAG)
        name = release.expected_archive_names(TAG)[0]
        local = (self.assets / name).read_bytes()
        conflicting = bytes([local[0] ^ 0xFF]) + local[1:]
        gateway.add_asset(candidate, name, conflicting)

        with self.assertRaises(release_publish.PublishError):
            self.publish(gateway)
        self.assertTrue(candidate["draft"])
        self.assertEqual(gateway.successful_uploads, 0)
        self.assertEqual(gateway.asset_bytes[candidate["assets"][0]["id"]], conflicting)

    def test_non_workflow_release_is_a_conflict(self) -> None:
        """Refuse a manually created draft or release sharing the immutable tag."""

        gateway = FakeGateway()
        self.create_owned_draft(gateway)
        candidate = gateway.release_by_tag(TAG)
        candidate["body"] = "manual release without ownership marker"
        with self.assertRaises(release_publish.PublishError):
            self.publish(gateway)
        self.assertTrue(candidate["draft"])
        self.assertEqual(gateway.successful_uploads, 0)

    def test_remote_download_mismatch_retains_verified_name_set_as_draft(self) -> None:
        """Detect remote bytes that conflict despite plausible size metadata."""

        gateway = FakeGateway()
        self.create_owned_draft(gateway)
        candidate = gateway.release_by_tag(TAG)
        for path in sorted(self.assets.iterdir()):
            gateway.add_asset(candidate, path.name, path.read_bytes())
        first = candidate["assets"][0]
        original = gateway.asset_bytes[first["id"]]
        gateway.asset_bytes[first["id"]] = bytes([original[0] ^ 0xFF]) + original[1:]
        first["digest"] = None

        with self.assertRaises(release.ReleaseError):
            self.publish(gateway)
        self.assertTrue(candidate["draft"])

    def test_matching_published_release_is_idempotent(self) -> None:
        """Verify and accept an already-published complete workflow release."""

        gateway = FakeGateway()
        self.assertEqual(self.publish(gateway), "published")
        uploads = gateway.successful_uploads
        self.assertEqual(self.publish(gateway), "already-published")
        self.assertEqual(gateway.successful_uploads, uploads)
        self.assertEqual(gateway.tag_commit, COMMIT)

    def test_unexpected_existing_asset_blocks_publication(self) -> None:
        """Reject additional remote assets rather than silently publishing them."""

        gateway = FakeGateway()
        self.create_owned_draft(gateway)
        candidate = gateway.release_by_tag(TAG)
        gateway.add_asset(candidate, "unexpected.bin", b"unexpected")
        with self.assertRaises(release_publish.PublishError):
            self.publish(gateway)
        self.assertTrue(candidate["draft"])

    def test_flatten_release_pages_rejects_malformed_pagination(self) -> None:
        """Flatten all release pages while failing closed on malformed API JSON."""

        self.assertEqual(
            release_publish.flatten_release_pages([[{"id": 1}], [{"id": 2}]]),
            [{"id": 1}, {"id": 2}],
        )
        for malformed in ({}, [[{"id": 1}], {}], [["not-an-object"]]):
            with self.subTest(malformed=malformed):
                with self.assertRaises(release_publish.PublishError):
                    release_publish.flatten_release_pages(malformed)


if __name__ == "__main__":
    unittest.main()
