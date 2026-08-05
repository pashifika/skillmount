#!/usr/bin/env python3
"""Reconcile the paired SkillMount Homebrew tap and Chocolatey package channels."""

from __future__ import annotations

import argparse
import base64
import binascii
import hashlib
import json
import os
import re
import subprocess
import sys
import urllib.error
import urllib.request
from dataclasses import dataclass
from pathlib import Path
from typing import Any, Literal, Mapping, Protocol, Sequence
from urllib.parse import quote
from xml.etree import ElementTree

import package_channels as channels
import release as release_assets

API_VERSION = "2026-03-10"
COMMUNITY_QUERY_SOURCE = "https://community.chocolatey.org/api/v2"
COMMUNITY_PUSH_SOURCE = "https://push.chocolatey.org/"
COMMUNITY_PACKAGE_HASH_ALGORITHM = "SHA512"
TAP_BRANCH_NAMESPACE = "skillmount"
HTTP_TIMEOUT_SECONDS = 30
USER_AGENT = "skillmount-package-publisher"
MODERATION_STATES = ("approved", "pending", "rejected")
HEX64_PATTERN = re.compile(r"[0-9a-f]{64}")
HEX128_PATTERN = re.compile(r"[0-9a-f]{128}")
FORMULA_URL_PATTERN = re.compile(r'^\s*url\s+"([^"\n]+)"', re.MULTILINE)
FORMULA_SHA256_PATTERN = re.compile(r'^\s*sha256\s+"([^"\n]+)"', re.MULTILINE)
FORMULA_VERSION_PATTERN = re.compile(r'^\s*version\s+"([^"\n]+)"', re.MULTILINE)
FORMULA_INSTALLED_BINARY_PATTERN = re.compile(
    r'^\s*bin\.install\s+"([^"\n]+)"', re.MULTILINE
)
FORMULA_COMMAND_PATTERN = re.compile(
    r'generate_completions_from_executable\(\s*bin\s*/\s*"([^"\n]+)"'
)
RELEASE_TAG_PATTERN = re.compile(r"/releases/download/(v[^/\s]+)/[^/\s]+\Z")
ODATA_NAMESPACES = {
    "atom": "http://www.w3.org/2005/Atom",
    "m": "http://schemas.microsoft.com/ado/2007/08/dataservices/metadata",
    "d": "http://schemas.microsoft.com/ado/2007/08/dataservices",
}


class PublicationError(RuntimeError):
    """External channel state cannot be changed safely."""


PublicationState = Literal["created", "resumed", "unchanged", "pending", "listed"]
FormulaState = Literal["absent", "identical", "stale", "conflicting"]


def require_pair(mapping: Mapping[str, Any], label: str) -> dict[str, Any]:
    """Require exactly one entry per package identity, in the immutable pair order."""

    expected = [identity.package_id for identity in channels.PACKAGES]
    if sorted(mapping) != sorted(expected):
        raise PublicationError(
            f"{label} keys are {sorted(mapping)!r}; expected exactly {expected!r}"
        )
    return {package_id: mapping[package_id] for package_id in expected}


def unique_match(pattern: re.Pattern[str], text: str) -> str | None:
    """Return the single distinct captured value, or None when it is ambiguous."""

    values = {match.group(1) for match in pattern.finditer(text)}
    return values.pop() if len(values) == 1 else None


def normalized_sha512_hash(value: object) -> str | None:
    """Return the lowercase hex form of a base64 or hex SHA-512 value."""

    if not isinstance(value, str):
        return None
    text = value.strip()
    if HEX128_PATTERN.fullmatch(text.lower()):
        return text.lower()
    try:
        raw = base64.b64decode(text, validate=True)
    except (binascii.Error, ValueError):
        return None
    return raw.hex() if len(raw) == 64 else None


@dataclass(frozen=True)
class FormulaIdentity:
    """Immutable Formula provenance compared before any tap write."""

    archive_url: str | None
    archive_sha256: str | None
    version: str | None
    installed_binary: str | None
    command: str | None

    @property
    def complete(self) -> bool:
        """Return whether every comparable field was resolved from the Formula."""

        return all(
            value is not None
            for value in (
                self.archive_url,
                self.archive_sha256,
                self.version,
                self.installed_binary,
                self.command,
            )
        )

    def describe(self) -> str:
        """Render every comparable field, naming unresolved ones explicitly."""

        return (
            f"url={self.archive_url or 'unparsed'} "
            f"sha256={self.archive_sha256 or 'unparsed'} "
            f"version={self.version or 'unparsed'} "
            f"installed={self.installed_binary or 'unparsed'} "
            f"command={self.command or 'unparsed'}"
        )


def formula_identity(text: str) -> FormulaIdentity:
    """Extract one Formula's comparable provenance from its Ruby source."""

    archive_url = unique_match(FORMULA_URL_PATTERN, text)
    version = unique_match(FORMULA_VERSION_PATTERN, text)
    if version is None and archive_url is not None:
        tag_match = RELEASE_TAG_PATTERN.search(archive_url)
        if tag_match is not None:
            try:
                version = release_assets.stable_version_from_tag(tag_match.group(1))
            except release_assets.ReleaseError:
                version = None
    return FormulaIdentity(
        archive_url=archive_url,
        archive_sha256=unique_match(FORMULA_SHA256_PATTERN, text),
        version=version,
        installed_binary=unique_match(FORMULA_INSTALLED_BINARY_PATTERN, text),
        command=unique_match(FORMULA_COMMAND_PATTERN, text),
    )


def expected_formula_identity(
    inputs: channels.PackageInputs, identity: channels.PackageIdentity
) -> FormulaIdentity:
    """Return the provenance every generated Formula in this pair must declare."""

    archive = inputs.archive(channels.MACOS_ARM64.triple)
    return FormulaIdentity(
        archive_url=archive.url,
        archive_sha256=archive.sha256,
        version=inputs.version,
        installed_binary=identity.command,
        command=identity.command,
    )


def classify_formula(
    text: str | None, *, expected_text: str, expected: FormulaIdentity
) -> tuple[FormulaState, str | None]:
    """Classify one observed Formula against the generated candidate for this run."""

    if text is None:
        return "absent", None
    if text == expected_text:
        return "identical", None
    observed = formula_identity(text)
    if not observed.complete:
        return "conflicting", f"provenance is not parseable ({observed.describe()})"
    if observed != expected:
        return "conflicting", f"provenance differs ({observed.describe()})"
    return "stale", None


@dataclass(frozen=True)
class FormulaMember:
    """One Formula's generated candidate beside its observed tap state."""

    identity: channels.PackageIdentity
    expected_text: str
    expected: FormulaIdentity
    default_branch: str
    branch: str
    default_text: str | None
    branch_text: str | None

    @property
    def default_classification(self) -> tuple[FormulaState, str | None]:
        """Classify this Formula as observed on the tap default branch."""

        return classify_formula(
            self.default_text, expected_text=self.expected_text, expected=self.expected
        )

    @property
    def branch_classification(self) -> tuple[FormulaState, str | None]:
        """Classify this Formula as observed on the version branch."""

        return classify_formula(
            self.branch_text, expected_text=self.expected_text, expected=self.expected
        )

    @property
    def conflicts(self) -> tuple[str, ...]:
        """Return every blocking conflict observed for this Formula."""

        reasons: list[str] = []
        for ref, (state, reason) in (
            (self.default_branch, self.default_classification),
            (self.branch, self.branch_classification),
        ):
            if state == "conflicting":
                reasons.append(f"{self.identity.formula_path} on {ref}: {reason}")
        return tuple(reasons)

    @property
    def publication_state(self) -> PublicationState:
        """Return this Formula's reconciliation outcome for the current run."""

        if self.default_classification[0] == "identical":
            return "unchanged"
        if self.branch_classification[0] == "identical":
            return "resumed"
        return "created"

    @property
    def needs_write(self) -> bool:
        """Return whether the version branch still needs this Formula's bytes."""

        return self.publication_state == "created"

    def report(self) -> tuple[str, ...]:
        """Render expected and observed identity lines for one pair member."""

        lines = [f"{self.identity.package_id}: expected {self.expected.describe()}"]
        for ref, text, (state, _reason) in (
            (self.default_branch, self.default_text, self.default_classification),
            (self.branch, self.branch_text, self.branch_classification),
        ):
            if text is None:
                lines.append(f"{self.identity.package_id}: observed on {ref} absent")
            else:
                lines.append(
                    f"{self.identity.package_id}: observed on {ref} {state} "
                    f"({formula_identity(text).describe()})"
                )
        return tuple(lines)


class TapGateway(Protocol):
    """Homebrew tap boundary that owns the tap-scoped publication credential."""

    def default_branch(self) -> str:
        """Return the tap's protected default branch name."""

    def default_branch_commit(self) -> str:
        """Return the commit the default branch currently points at."""

    def file_text(self, ref: str, path: str) -> str | None:
        """Return one file's UTF-8 text at *ref*, or None when it is absent."""

    def branch_commit(self, branch: str) -> str | None:
        """Return the branch head commit, or None when the branch is absent."""

    def create_branch(self, branch: str, commit: str) -> None:
        """Create one new branch at *commit* without moving an existing branch."""

    def put_file(self, branch: str, path: str, text: str, message: str) -> str:
        """Write one file on *branch* and return the resulting commit."""

    def pull_request_for_branch(self, branch: str) -> dict[str, Any] | None:
        """Return the pull request for *branch* as `{url, state, head, base, merged}`."""

    def open_pull_request(
        self, *, head: str, base: str, title: str, body: str
    ) -> dict[str, Any]:
        """Open one pull request and return it in the same normalized shape."""


def tap_branch(inputs: channels.PackageInputs) -> str:
    """Return the immutable version-specific tap branch name."""

    version = release_assets.validate_stable_version(inputs.version)
    return f"{TAP_BRANCH_NAMESPACE}/{version}"


@dataclass(frozen=True)
class TapOutcome:
    """Observed result of one paired tap reconciliation."""

    branch: str
    pull_request_url: str
    formula_states: dict[str, PublicationState]


def formula_commit_message(
    inputs: channels.PackageInputs, identity: channels.PackageIdentity
) -> str:
    """Return the reviewable commit message for one Formula update."""

    archive = inputs.archive(channels.MACOS_ARM64.triple)
    return (
        f"{identity.package_id}: {inputs.version}\n\n"
        f"Tag: {inputs.tag}\n"
        f"Commit: {inputs.commit}\n"
        f"Release archive: {archive.url}\n"
        f"Archive SHA-256: {archive.sha256}\n"
        f"Installed binary: {identity.command}\n"
        f"Selected command: {identity.command}\n"
    )


def pull_request_title(inputs: channels.PackageInputs) -> str:
    """Return the version-specific title of the paired tap change."""

    return f"{channels.PRODUCT_NAME} {inputs.version}: paired Formula update"


def pull_request_body(
    inputs: channels.PackageInputs, members: Sequence[FormulaMember]
) -> str:
    """Return a reviewable body naming the shared provenance and both members."""

    archive = inputs.archive(channels.MACOS_ARM64.triple)
    lines = [
        f"Generated from {inputs.repository} {inputs.tag} ({inputs.commit}).",
        "",
        f"Release: {inputs.release_url}",
        f"Release archive: {archive.url}",
        f"Archive SHA-256: {archive.sha256}",
        "",
        "| Formula | installed archive member | selected command | state |",
        "| --- | --- | --- | --- |",
    ]
    for member in members:
        lines.append(
            f"| `{member.identity.formula_path}` | `{member.identity.command}` "
            f"| `{member.identity.command}` | {member.publication_state} |"
        )
    lines.extend(
        (
            "",
            "Merge only after tap CI proves style, audit, both archive installs, both "
            "Formula tests, selected-only install, co-installation, cross-uninstall, "
            "and completion ownership.",
        )
    )
    return "\n".join(lines)


def tap_conflict_message(
    branch: str, members: Sequence[FormulaMember], conflicts: Sequence[str]
) -> str:
    """Render every expected and observed pair identity for a blocked tap change."""

    lines = [
        f"Homebrew tap reconciliation for {branch} is blocked; no Formula was written.",
    ]
    for member in members:
        lines.extend(member.report())
    lines.extend(f"conflict: {reason}" for reason in conflicts)
    return "\n".join(lines)


def require_formula_texts(formulae: Mapping[str, str]) -> dict[str, str]:
    """Require non-empty generated Formula text for both pair members."""

    texts = require_pair(formulae, "generated Formula text")
    for package_id, text in texts.items():
        if not isinstance(text, str) or not text.strip():
            raise PublicationError(
                f"generated Formula text for {package_id!r} is empty; "
                "expected the rendered Formula source"
            )
    return {package_id: str(text) for package_id, text in texts.items()}


def pull_request_url(
    pull_request: Mapping[str, Any], *, branch: str, base: str
) -> str:
    """Validate one pull-request payload's identity and return its review URL."""

    url = pull_request.get("url")
    if not isinstance(url, str) or not url.startswith("https://github.com/"):
        raise PublicationError(
            f"tap pull request for {branch} has URL {url!r}; "
            "expected a trusted https://github.com/ URL"
        )
    head = pull_request.get("head")
    if head is not None and head != branch:
        raise PublicationError(
            f"tap pull request {url} has head {head!r}; expected {branch!r}"
        )
    observed_base = pull_request.get("base")
    if observed_base is not None and observed_base != base:
        raise PublicationError(
            f"tap pull request {url} targets base {observed_base!r}; expected {base!r}"
        )
    return url


def pull_request_state(pull_request: Mapping[str, Any]) -> str:
    """Return `open`, `merged`, or `closed` for one pull-request payload."""

    state = pull_request.get("state")
    if state not in ("open", "closed"):
        raise PublicationError(
            f"tap pull request state is {state!r}; expected 'open' or 'closed'"
        )
    if state == "closed" and pull_request.get("merged") is True:
        return "merged"
    return str(state)


def reconcile_tap(
    gateway: TapGateway,
    inputs: channels.PackageInputs,
    formulae: Mapping[str, str],
) -> TapOutcome:
    """Propose or resume one paired tap change without overwriting either Formula."""

    branch = tap_branch(inputs)
    texts = require_formula_texts(formulae)
    default_branch = gateway.default_branch()
    if not isinstance(default_branch, str) or not default_branch:
        raise PublicationError(
            f"tap gateway reported default branch {default_branch!r}; expected a name"
        )
    if branch == default_branch:
        raise PublicationError(
            f"version branch {branch!r} collides with the tap default branch "
            f"{default_branch!r}; refusing to write the default branch"
        )

    existing_branch_commit = gateway.branch_commit(branch)
    members: list[FormulaMember] = []
    for identity in channels.PACKAGES:
        path = identity.formula_path
        members.append(
            FormulaMember(
                identity=identity,
                expected_text=texts[identity.package_id],
                expected=expected_formula_identity(inputs, identity),
                default_branch=default_branch,
                branch=branch,
                default_text=gateway.file_text(default_branch, path),
                branch_text=(
                    None
                    if existing_branch_commit is None
                    else gateway.file_text(branch, path)
                ),
            )
        )

    conflicts = [reason for member in members for reason in member.conflicts]
    if conflicts:
        raise PublicationError(tap_conflict_message(branch, members, conflicts))

    states: dict[str, PublicationState] = {
        member.identity.package_id: member.publication_state for member in members
    }
    existing = gateway.pull_request_for_branch(branch)
    url = ""
    if existing is not None:
        if not isinstance(existing, dict):
            raise PublicationError(
                f"tap pull-request lookup for {branch} returned "
                f"{type(existing).__name__}; expected a mapping or None"
            )
        url = pull_request_url(existing, branch=branch, base=default_branch)

    outstanding = [member for member in members if member.publication_state != "unchanged"]
    if not outstanding:
        return TapOutcome(branch=branch, pull_request_url=url, formula_states=states)

    if existing is not None and pull_request_state(existing) != "open":
        pending = ", ".join(member.identity.package_id for member in outstanding)
        raise PublicationError(
            f"tap pull request {url} for {branch} is "
            f"{pull_request_state(existing)!r} while {pending} still requires review; "
            "refusing to reopen, force-push, or bypass it"
        )

    writes = [member for member in members if member.needs_write]
    if writes:
        if existing_branch_commit is None:
            base_commit = release_assets.validate_commit(gateway.default_branch_commit())
            gateway.create_branch(branch, base_commit)
        for member in writes:
            gateway.put_file(
                branch,
                member.identity.formula_path,
                member.expected_text,
                formula_commit_message(inputs, member.identity),
            )

    if existing is None:
        opened = gateway.open_pull_request(
            head=branch,
            base=default_branch,
            title=pull_request_title(inputs),
            body=pull_request_body(inputs, members),
        )
        if not isinstance(opened, dict):
            raise PublicationError(
                f"tap did not return the pull request opened for {branch}"
            )
        url = pull_request_url(opened, branch=branch, base=default_branch)
    return TapOutcome(branch=branch, pull_request_url=url, formula_states=states)


class CommunityGateway(Protocol):
    """Chocolatey Community Repository boundary that owns the channel API key."""

    def package_version(self, package_id: str, version: str) -> dict[str, Any] | None:
        """Return `{version, listed, moderation_status, package_hash,
        package_hash_algorithm}` for one package version, or None when absent."""

    def push(self, path: Path, *, package_id: str, version: str) -> dict[str, Any]:
        """Push one validated nupkg and return the observed upload response."""


@dataclass(frozen=True)
class ChocolateyMember:
    """One package id's validated candidate beside its observed feed state."""

    identity: channels.PackageIdentity
    version: str
    nupkg: Path
    package_sha256: str
    package_sha512: str
    payload: dict[str, Any] | None

    @property
    def absent(self) -> bool:
        """Return whether the Community Repository has no such package version."""

        return self.payload is None

    def expected(self) -> str:
        """Render the identity this run requires for one package id."""

        return (
            f"{self.identity.package_id}: expected version={self.version} "
            f"package_sha256={self.package_sha256} "
            f"package_sha512={self.package_sha512} "
            f"algorithm={COMMUNITY_PACKAGE_HASH_ALGORITHM} "
            f"nupkg={self.nupkg.name}"
        )

    def observed(self) -> str:
        """Render the identity actually observed for one package id."""

        if self.payload is None:
            return f"{self.identity.package_id}: observed absent"
        normalized = normalized_sha512_hash(self.payload.get("package_hash"))
        return (
            f"{self.identity.package_id}: observed "
            f"version={self.payload.get('version')!r} "
            f"hash={self.payload.get('package_hash')!r} "
            f"hash_sha512={normalized or 'unparsed'} "
            f"algorithm={self.payload.get('package_hash_algorithm')!r} "
            f"moderation={self.payload.get('moderation_status')!r} "
            f"listed={self.payload.get('listed')!r}"
        )

    def classify(self) -> tuple[PublicationState | None, str | None]:
        """Return this member's state, or the single reason it blocks the pair.

        A `(None, None)` result means the version is absent and is the only work
        this run may perform. An approved member is reported as `listed` only when
        the supported exact approved-only `choco search` path resolves this
        version; approved metadata without that public resolution is `unchanged`,
        an accepted member awaiting moderation is `pending`, and a rejected member
        is a conflict.
        """

        payload = self.payload
        if payload is None:
            return None, None
        version = payload.get("version")
        if version != self.version:
            return None, f"version is {version!r}; expected {self.version!r}"
        algorithm = payload.get("package_hash_algorithm")
        if algorithm != COMMUNITY_PACKAGE_HASH_ALGORITHM:
            return None, (
                f"package hash algorithm is {algorithm!r}; "
                f"expected {COMMUNITY_PACKAGE_HASH_ALGORITHM!r}"
            )
        observed_hash = normalized_sha512_hash(payload.get("package_hash"))
        if observed_hash is None:
            return None, (
                f"package hash {payload.get('package_hash')!r} is neither a "
                "128-hex nor a base64 SHA-512 value"
            )
        if observed_hash != self.package_sha512:
            return None, (
                f"package hash is {observed_hash}; expected {self.package_sha512}"
            )
        status = payload.get("moderation_status")
        if status not in MODERATION_STATES:
            return None, (
                f"moderation status is {status!r}; "
                f"expected one of {list(MODERATION_STATES)!r}"
            )
        if status == "rejected":
            return None, "the published version was rejected by moderation"
        listed = payload.get("listed")
        if listed is not None and not isinstance(listed, bool):
            return None, f"listed flag is {listed!r}; expected a boolean or null"
        if status == "pending":
            return "pending", None
        return ("listed" if listed is True else "unchanged"), None


def chocolatey_conflict_message(
    version: str, members: Sequence[ChocolateyMember], conflicts: Sequence[str]
) -> str:
    """Render every expected and observed pair identity for a blocked push."""

    lines = [
        f"Chocolatey reconciliation for {version} is blocked; "
        "neither package id was pushed."
    ]
    for member in members:
        lines.append(member.expected())
        lines.append(member.observed())
    lines.extend(f"conflict: {reason}" for reason in conflicts)
    return "\n".join(lines)


def validated_candidate(
    identity: channels.PackageIdentity,
    version: str,
    path: object,
    digest: object,
) -> tuple[Path, str]:
    """Prove one local nupkg's name and digest before any credentialed call."""

    if not isinstance(path, Path):
        raise PublicationError(
            f"Chocolatey candidate for {identity.package_id!r} is "
            f"{type(path).__name__}; expected a Path"
        )
    expected_name = channels.nupkg_name(identity, version)
    if path.name != expected_name:
        raise PublicationError(
            f"Chocolatey candidate for {identity.package_id!r} is {path.name!r}; "
            f"expected {expected_name!r}"
        )
    if not path.is_file():
        raise PublicationError(
            f"Chocolatey candidate {path} is not a regular file; "
            "expected the packed nupkg"
        )
    if not isinstance(digest, str) or not HEX64_PATTERN.fullmatch(digest):
        raise PublicationError(
            f"Chocolatey digest for {identity.package_id!r} is {digest!r}; "
            "expected a lowercase 64-hex SHA-256 value"
        )
    observed = release_assets.sha256_file(path)
    if observed != digest:
        raise PublicationError(
            f"Chocolatey candidate {path.name} hashes to {observed}; expected {digest}"
        )
    return path, digest


def sha512_file(path: Path) -> str:
    """Return the Community Repository's streaming SHA-512 package digest."""

    digest = hashlib.sha512()
    with path.open("rb") as input_file:
        while chunk := input_file.read(1024 * 1024):
            digest.update(chunk)
    return digest.hexdigest()


def query_package_version(
    gateway: CommunityGateway, identity: channels.PackageIdentity, version: str
) -> dict[str, Any] | None:
    """Read one package version's observed state, failing closed on an outage."""

    try:
        payload = gateway.package_version(identity.package_id, version)
    except (OSError, ValueError) as error:
        raise PublicationError(
            f"Community Repository state for {identity.package_id} {version} could "
            f"not be observed, so neither package id was pushed: {error}"
        ) from error
    if payload is None:
        return None
    if not isinstance(payload, dict):
        raise PublicationError(
            f"Community Repository returned {type(payload).__name__} for "
            f"{identity.package_id} {version}; expected a mapping or None"
        )
    return payload


def push_member(gateway: CommunityGateway, member: ChocolateyMember) -> PublicationState:
    """Push one absent version exactly once and report it as awaiting moderation."""

    try:
        response = gateway.push(
            member.nupkg,
            package_id=member.identity.package_id,
            version=member.version,
        )
    except (OSError, ValueError) as error:
        raise PublicationError(
            f"push of {member.nupkg.name} for {member.identity.package_id} "
            f"{member.version} failed and was not retried: {error}"
        ) from error
    if not isinstance(response, dict):
        raise PublicationError(
            f"push of {member.identity.package_id} {member.version} returned "
            f"{type(response).__name__}; expected a response mapping"
        )
    status = response.get("moderation_status")
    if status == "rejected":
        raise PublicationError(
            f"{member.identity.package_id} {member.version} was rejected on upload: "
            f"{response.get('message', 'no moderator message was reported')}"
        )
    if status is not None and status not in MODERATION_STATES:
        raise PublicationError(
            f"push response for {member.identity.package_id} {member.version} "
            f"declared moderation status {status!r}; "
            f"expected one of {list(MODERATION_STATES)!r}"
        )
    return "pending"


def reconcile_chocolatey(
    gateway: CommunityGateway,
    inputs: channels.PackageInputs,
    nupkgs: Mapping[str, Path],
    digests: Mapping[str, str],
) -> dict[str, PublicationState]:
    """Push only absent Community Repository members and report both states."""

    paths = require_pair(nupkgs, "Chocolatey nupkg path")
    hashes = require_pair(digests, "Chocolatey nupkg digest")
    candidates: dict[str, tuple[Path, str, str]] = {}
    for identity in channels.PACKAGES:
        candidate, package_sha256 = validated_candidate(
            identity,
            inputs.version,
            paths[identity.package_id],
            hashes[identity.package_id],
        )
        candidates[identity.package_id] = (
            candidate,
            package_sha256,
            sha512_file(candidate),
        )

    members: list[ChocolateyMember] = []
    for identity in channels.PACKAGES:
        path, package_sha256, package_sha512 = candidates[identity.package_id]
        members.append(
            ChocolateyMember(
                identity=identity,
                version=inputs.version,
                nupkg=path,
                package_sha256=package_sha256,
                package_sha512=package_sha512,
                payload=query_package_version(gateway, identity, inputs.version),
            )
        )

    classifications = {
        member.identity.package_id: member.classify() for member in members
    }
    conflicts = [
        f"{package_id} {inputs.version}: {reason}"
        for package_id, (_state, reason) in classifications.items()
        if reason is not None
    ]
    if conflicts:
        raise PublicationError(
            chocolatey_conflict_message(inputs.version, members, conflicts)
        )

    states: dict[str, PublicationState] = {}
    pushed: set[str] = set()
    for member in members:
        package_id = member.identity.package_id
        state, _reason = classifications[package_id]
        if state is not None:
            states[package_id] = state
            continue
        if package_id in pushed:
            raise PublicationError(
                f"refusing to push {package_id} {member.version} twice in one run"
            )
        pushed.add(package_id)
        states[package_id] = push_member(gateway, member)
    return states


class GhTapGateway:
    """Shell-free adapter that writes only a version branch of one Homebrew tap."""

    def __init__(self, repository: str, working_directory: Path | None = None) -> None:
        """Bind tap API calls to the authenticated tap-scoped GitHub App token."""

        if repository.count("/") != 1 or not all(repository.split("/")):
            raise PublicationError(
                f"tap repository is {repository!r}; expected exactly '<owner>/<name>'"
            )
        self.repository = repository
        self.owner = repository.split("/", 1)[0]
        self.working_directory = (working_directory or Path.cwd()).resolve()
        if not os.environ.get("GH_TOKEN"):
            raise PublicationError("GH_TOKEN is required for Homebrew tap publication")
        self._default_branch: str | None = None

    def _run(
        self, arguments: Sequence[str], *, input_bytes: bytes | None = None,
        allow_missing: bool = False,
    ) -> bytes | None:
        completed = subprocess.run(
            arguments,
            cwd=self.working_directory,
            input=input_bytes,
            stdout=subprocess.PIPE,
            stderr=subprocess.PIPE,
            check=False,
        )
        if completed.returncode != 0:
            stderr = completed.stderr.decode(errors="replace").strip()
            if allow_missing and "HTTP 404" in stderr:
                return None
            command = " ".join(arguments[:3])
            raise PublicationError(
                f"{command} failed with status {completed.returncode}: {stderr}"
            )
        return completed.stdout

    def _api(
        self,
        endpoint: str,
        *,
        method: str = "GET",
        payload: dict[str, Any] | None = None,
        allow_missing: bool = False,
    ) -> Any:
        arguments = [
            "gh",
            "api",
            endpoint,
            "--method",
            method,
            "--header",
            f"X-GitHub-Api-Version: {API_VERSION}",
        ]
        input_bytes = None
        if payload is not None:
            arguments.extend(("--input", "-"))
            input_bytes = json.dumps(payload, separators=(",", ":")).encode()
        output = self._run(
            arguments, input_bytes=input_bytes, allow_missing=allow_missing
        )
        if output is None:
            return None
        if not output.strip():
            return {}
        try:
            return json.loads(output)
        except (UnicodeDecodeError, json.JSONDecodeError) as error:
            raise PublicationError(
                f"tap API returned invalid JSON for {endpoint}"
            ) from error

    def default_branch(self) -> str:
        """Return the tap's protected default branch name."""

        if self._default_branch is None:
            payload = self._api(f"repos/{self.repository}")
            branch = payload.get("default_branch") if isinstance(payload, dict) else None
            if not isinstance(branch, str) or not branch:
                raise PublicationError(
                    f"tap {self.repository} reported default branch {branch!r}"
                )
            self._default_branch = branch
        return self._default_branch

    def _reference_commit(self, branch: str) -> str | None:
        reference = self._api(
            f"repos/{self.repository}/git/ref/heads/{quote(branch, safe='/')}",
            allow_missing=True,
        )
        if reference is None:
            return None
        if (
            not isinstance(reference, dict)
            or reference.get("ref") != f"refs/heads/{branch}"
        ):
            raise PublicationError(
                f"tap reference lookup for {branch!r} returned {reference!r}; "
                f"expected refs/heads/{branch}"
            )
        target = reference.get("object")
        if not isinstance(target, dict) or target.get("type") != "commit":
            raise PublicationError(
                f"tap branch {branch!r} points at object {target!r}; expected a commit"
            )
        return release_assets.validate_commit(str(target.get("sha")))

    def default_branch_commit(self) -> str:
        """Return the commit the tap default branch currently points at."""

        branch = self.default_branch()
        commit = self._reference_commit(branch)
        if commit is None:
            raise PublicationError(
                f"tap default branch {branch!r} has no resolvable head commit"
            )
        return commit

    def branch_commit(self, branch: str) -> str | None:
        """Return the branch head commit, or None when the branch is absent."""

        return self._reference_commit(branch)

    def file_text(self, ref: str, path: str) -> str | None:
        """Return one file's UTF-8 text at *ref*, or None when it is absent."""

        payload = self._api(
            f"repos/{self.repository}/contents/{quote(path)}"
            f"?ref={quote(ref, safe='')}",
            allow_missing=True,
        )
        if payload is None:
            return None
        if not isinstance(payload, dict) or payload.get("type") != "file":
            raise PublicationError(
                f"tap path {path!r} on {ref!r} is not a regular file"
            )
        if payload.get("encoding") != "base64":
            raise PublicationError(
                f"tap file {path!r} on {ref!r} used encoding "
                f"{payload.get('encoding')!r}; expected 'base64'"
            )
        try:
            return base64.b64decode(str(payload.get("content", ""))).decode("utf-8")
        except (binascii.Error, UnicodeDecodeError, ValueError) as error:
            raise PublicationError(
                f"tap file {path!r} on {ref!r} is not base64-encoded UTF-8 text"
            ) from error

    def create_branch(self, branch: str, commit: str) -> None:
        """Create one new branch at *commit* without moving an existing branch."""

        if branch == self.default_branch():
            raise PublicationError(
                f"refusing to create the tap default branch {branch!r}"
            )
        self._api(
            f"repos/{self.repository}/git/refs",
            method="POST",
            payload={
                "ref": f"refs/heads/{branch}",
                "sha": release_assets.validate_commit(commit),
            },
        )

    def put_file(self, branch: str, path: str, text: str, message: str) -> str:
        """Write one file on *branch* and return the resulting commit."""

        if branch == self.default_branch():
            raise PublicationError(
                f"refusing to write {path!r} to the tap default branch {branch!r}"
            )
        endpoint = f"repos/{self.repository}/contents/{quote(path)}"
        payload: dict[str, Any] = {
            "message": message,
            "content": base64.b64encode(text.encode("utf-8")).decode("ascii"),
            "branch": branch,
        }
        existing = self._api(
            f"{endpoint}?ref={quote(branch, safe='')}", allow_missing=True
        )
        if isinstance(existing, dict) and isinstance(existing.get("sha"), str):
            payload["sha"] = existing["sha"]
        response = self._api(endpoint, method="PUT", payload=payload)
        commit = response.get("commit") if isinstance(response, dict) else None
        sha = commit.get("sha") if isinstance(commit, dict) else None
        if not isinstance(sha, str):
            raise PublicationError(
                f"tap write of {path!r} on {branch!r} returned no commit SHA"
            )
        return release_assets.validate_commit(sha)

    def _normalize_pull_request(self, payload: Mapping[str, Any]) -> dict[str, Any]:
        head = payload.get("head")
        base = payload.get("base")
        url = payload.get("html_url")
        state = payload.get("state")
        if not isinstance(url, str) or not isinstance(state, str):
            raise PublicationError(
                f"tap pull request payload is malformed: {dict(payload)!r}"
            )
        return {
            "url": url,
            "state": state,
            "head": head.get("ref") if isinstance(head, dict) else None,
            "base": base.get("ref") if isinstance(base, dict) else None,
            "merged": payload.get("merged_at") is not None,
            "number": payload.get("number"),
        }

    def pull_request_for_branch(self, branch: str) -> dict[str, Any] | None:
        """Return the newest pull request for *branch*, preferring an open one."""

        reference = quote(f"{self.owner}:{branch}", safe="")
        payload = self._api(
            f"repos/{self.repository}/pulls?head={reference}&state=all&per_page=100"
        )
        if not isinstance(payload, list):
            raise PublicationError(
                f"tap pull-request listing for {branch} was not a JSON array"
            )
        candidates = [item for item in payload if isinstance(item, dict)]
        if len(candidates) != len(payload):
            raise PublicationError(
                f"tap pull-request listing for {branch} contained a non-object"
            )
        openings = [item for item in candidates if item.get("state") == "open"]
        if len(openings) > 1:
            raise PublicationError(
                f"tap branch {branch!r} already has {len(openings)} open pull requests"
            )
        pool = openings or candidates
        if not pool:
            return None
        newest = max(
            pool, key=lambda item: item["number"] if isinstance(item.get("number"), int) else -1
        )
        return self._normalize_pull_request(newest)

    def open_pull_request(
        self, *, head: str, base: str, title: str, body: str
    ) -> dict[str, Any]:
        """Open one pull request against the protected tap default branch."""

        response = self._api(
            f"repos/{self.repository}/pulls",
            method="POST",
            payload={
                "title": title,
                "head": head,
                "base": base,
                "body": body,
                "draft": False,
                "maintainer_can_modify": True,
            },
        )
        if not isinstance(response, dict):
            raise PublicationError(
                f"tap did not return the pull request opened for {head}"
            )
        return self._normalize_pull_request(response)


class ChocolateyGateway:
    """Shell-free adapter over `choco push` and the Community Repository feed."""

    def __init__(
        self,
        *,
        query_source: str = COMMUNITY_QUERY_SOURCE,
        push_source: str = COMMUNITY_PUSH_SOURCE,
        choco: str = "choco",
        working_directory: Path | None = None,
        timeout: int = HTTP_TIMEOUT_SECONDS,
    ) -> None:
        """Bind reads and pushes to their distinct official endpoints."""

        for label, source in (
            ("query", query_source),
            ("push", push_source),
        ):
            if not source.startswith("https://"):
                raise PublicationError(
                    f"Chocolatey {label} source is {source!r}; expected an https URL"
                )
        self.query_source = query_source.rstrip("/")
        self.push_source = f"{push_source.rstrip('/')}/"
        self.choco = choco
        self.working_directory = (working_directory or Path.cwd()).resolve()
        self.timeout = timeout
        self._api_key = os.environ.get("CHOCOLATEY_API_KEY", "")
        if not self._api_key:
            raise PublicationError(
                "CHOCOLATEY_API_KEY is required for Chocolatey publication"
            )

    @staticmethod
    def _boolean(value: str | None) -> bool | None:
        if value is None:
            return None
        text = value.strip().lower()
        if text == "true":
            return True
        if text == "false":
            return False
        return None

    def _moderation_status(
        self, values: Mapping[str, str], package_id: str, version: str
    ) -> str:
        status = (values.get("PackageStatus") or "").strip().lower()
        approved = self._boolean(values.get("IsApproved"))
        if status == "rejected" or self._boolean(values.get("IsRejected")) is True:
            return "rejected"
        if status in ("approved", "exempted") or approved is True:
            return "approved"
        if status or approved is False:
            return "pending"
        raise PublicationError(
            f"Community Repository entry for {package_id} {version} reported neither "
            "PackageStatus nor IsApproved, so its moderation state is unknown"
        )

    def _entry_properties(self, body: bytes, package_id: str, version: str) -> dict[str, str]:
        try:
            root = ElementTree.fromstring(body)
        except ElementTree.ParseError as error:
            raise PublicationError(
                f"Community Repository response for {package_id} {version} was not "
                "valid XML"
            ) from error
        entry = root
        if root.tag == f"{{{ODATA_NAMESPACES['atom']}}}feed":
            entries = root.findall("atom:entry", ODATA_NAMESPACES)
            if len(entries) != 1:
                raise PublicationError(
                    f"Community Repository returned {len(entries)} entries for "
                    f"{package_id} {version}; expected exactly one"
                )
            entry = entries[0]
        properties = entry.find("m:properties", ODATA_NAMESPACES)
        if properties is None:
            properties = entry.find("atom:content/m:properties", ODATA_NAMESPACES)
        if properties is None:
            raise PublicationError(
                f"Community Repository entry for {package_id} {version} declared no "
                "properties element"
            )
        null_attribute = f"{{{ODATA_NAMESPACES['m']}}}null"
        return {
            element.tag.rsplit("}", 1)[-1]: (element.text or "")
            for element in properties
            if element.get(null_attribute) != "true"
        }

    def _publicly_listed(self, package_id: str, version: str) -> bool:
        """Prove that the supported CLI resolves this exact current version."""

        search_environment = os.environ.copy()
        search_environment.pop("CHOCOLATEY_API_KEY", None)
        completed = subprocess.run(
            [
                self.choco,
                "search",
                package_id,
                f"--version={version}",
                "--exact",
                "--all-versions",
                "--approved-only",
                "--limit-output",
                "--source",
                self.query_source,
            ],
            cwd=self.working_directory,
            env=search_environment,
            stdout=subprocess.PIPE,
            stderr=subprocess.PIPE,
            check=False,
        )
        stdout = completed.stdout.decode(errors="replace").strip()
        stderr = completed.stderr.decode(errors="replace").strip()
        if completed.returncode != 0:
            raise PublicationError(
                f"choco search for {package_id} {version} failed with status "
                f"{completed.returncode}: {stderr or stdout}"
            )
        lines = [line.strip() for line in stdout.splitlines() if line.strip()]
        if not lines:
            return False
        if len(lines) != 1 or "|" not in lines[0]:
            raise PublicationError(
                f"choco search for {package_id} returned unexpected limited output "
                f"{stdout!r}"
            )
        observed_id, observed_version = lines[0].split("|", 1)
        if observed_id.casefold() != package_id.casefold():
            raise PublicationError(
                f"choco search for {package_id} returned unexpected package "
                f"{observed_id!r}"
            )
        return observed_version == version

    def package_version(self, package_id: str, version: str) -> dict[str, Any] | None:
        """Read one package version from the OData feed, or None when it is absent."""

        endpoint = (
            f"{self.query_source}/Packages(Id='{quote(package_id, safe='')}',"
            f"Version='{quote(version, safe='')}')"
        )
        request = urllib.request.Request(
            endpoint,
            headers={"Accept": "application/atom+xml", "User-Agent": USER_AGENT},
        )
        try:
            with urllib.request.urlopen(request, timeout=self.timeout) as response:
                body = response.read()
        except urllib.error.HTTPError as error:
            if error.code == 404:
                return None
            raise PublicationError(
                f"Community Repository returned HTTP {error.code} for "
                f"{package_id} {version}"
            ) from error
        except urllib.error.URLError as error:
            raise PublicationError(
                f"Community Repository is unreachable for {package_id} {version}: "
                f"{error.reason}"
            ) from error
        values = self._entry_properties(body, package_id, version)
        moderation_status = self._moderation_status(values, package_id, version)
        return {
            "version": values.get("Version", ""),
            "listed": (
                self._publicly_listed(package_id, version)
                if moderation_status == "approved"
                else False
            ),
            "moderation_status": moderation_status,
            "package_hash": values.get("PackageHash", ""),
            "package_hash_algorithm": values.get("PackageHashAlgorithm", ""),
        }

    def push(self, path: Path, *, package_id: str, version: str) -> dict[str, Any]:
        """Push one validated nupkg once, never revealing the API key in output."""

        expected_name = f"{package_id}.{version}.nupkg"
        if path.name != expected_name:
            raise PublicationError(
                f"refusing to push {path.name!r} for {package_id} {version}; "
                f"expected {expected_name!r}"
            )
        completed = subprocess.run(
            [
                self.choco,
                "push",
                str(path),
                "--source",
                self.push_source,
                "--api-key",
                self._api_key,
                "--limit-output",
            ],
            cwd=self.working_directory,
            stdout=subprocess.PIPE,
            stderr=subprocess.PIPE,
            check=False,
        )
        stdout = completed.stdout.decode(errors="replace").strip()
        stderr = completed.stderr.decode(errors="replace").strip()
        if completed.returncode != 0:
            raise PublicationError(
                f"choco push of {path.name} failed with status "
                f"{completed.returncode} and was not retried: {stderr or stdout}"
            )
        return {
            "package_id": package_id,
            "version": version,
            "source": self.push_source,
            "status": "accepted",
            "output": stdout,
        }


def format_states(states: Mapping[str, PublicationState]) -> str:
    """Render both members' states on one line, never collapsing them into one."""

    ordered = [identity.package_id for identity in channels.PACKAGES]
    return " ".join(f"{package_id}={states[package_id]}" for package_id in ordered)


def load_inputs(path: Path) -> channels.PackageInputs:
    """Load and strictly re-validate the preflight inputs artifact."""

    return channels.PackageInputs.from_json(path.read_text(encoding="utf-8"))


def load_generated_formulae(directory: Path) -> dict[str, str]:
    """Read both generated Formula candidates from a generation output tree."""

    texts: dict[str, str] = {}
    for identity in channels.PACKAGES:
        path = directory / identity.formula_path
        if not path.is_file():
            raise PublicationError(
                f"generated Formula {path} is missing; expected both pair members"
            )
        texts[identity.package_id] = path.read_text(encoding="utf-8")
    return texts


def write_report(path: Path, payload: Mapping[str, Any]) -> None:
    """Write one deterministic channel evidence report."""

    path.parent.mkdir(parents=True, exist_ok=True)
    path.write_text(
        json.dumps(payload, indent=2, sort_keys=True) + "\n", encoding="utf-8"
    )


def argument_parser() -> argparse.ArgumentParser:
    """Build the package-channel publisher command-line parser."""

    parser = argparse.ArgumentParser(description=__doc__)
    subparsers = parser.add_subparsers(dest="command", required=True)

    homebrew = subparsers.add_parser(
        "homebrew", help="reconcile the paired Homebrew tap change"
    )
    homebrew.add_argument("--inputs", type=Path, required=True)
    homebrew.add_argument("--formula-directory", type=Path, required=True)
    homebrew.add_argument("--tap-repository", required=True)
    homebrew.add_argument("--repository-path", type=Path, default=Path.cwd())
    homebrew.add_argument("--report", type=Path)
    homebrew.add_argument("--github-output", type=Path)

    chocolatey = subparsers.add_parser(
        "chocolatey", help="reconcile both Community Repository package ids"
    )
    chocolatey.add_argument("--inputs", type=Path, required=True)
    chocolatey.add_argument("--nupkg-directory", type=Path, required=True)
    chocolatey.add_argument("--query-source", default=COMMUNITY_QUERY_SOURCE)
    chocolatey.add_argument("--push-source", default=COMMUNITY_PUSH_SOURCE)
    chocolatey.add_argument("--report", type=Path)
    chocolatey.add_argument("--github-output", type=Path)
    return parser


def run(arguments: Sequence[str]) -> int:
    """Reconcile exactly one channel and report both members' states."""

    options = argument_parser().parse_args(arguments)
    inputs = load_inputs(options.inputs)
    if options.command == "homebrew":
        outcome = reconcile_tap(
            GhTapGateway(options.tap_repository, options.repository_path),
            inputs,
            load_generated_formulae(options.formula_directory),
        )
        states = outcome.formula_states
        report: dict[str, Any] = {
            "channel": "homebrew",
            "tap_repository": options.tap_repository,
            "branch": outcome.branch,
            "pull_request_url": outcome.pull_request_url,
            "version": inputs.version,
            "tag": inputs.tag,
            "commit": inputs.commit,
            "formula_states": dict(states),
        }
        outputs = {
            "tap_branch": outcome.branch,
            "tap_pull_request_url": outcome.pull_request_url,
            "homebrew_states": format_states(states),
        }
        print(
            f"Homebrew tap {options.tap_repository} {outcome.branch}: "
            f"{format_states(states)}"
        )
    elif options.command == "chocolatey":
        nupkgs = {
            identity.package_id: options.nupkg_directory
            / channels.nupkg_name(identity, inputs.version)
            for identity in channels.PACKAGES
        }
        digests = channels.inspect_nupkg_pair(nupkgs, inputs)
        states = reconcile_chocolatey(
            ChocolateyGateway(
                query_source=options.query_source,
                push_source=options.push_source,
            ),
            inputs,
            nupkgs,
            digests,
        )
        report = {
            "channel": "chocolatey",
            "query_source": options.query_source,
            "push_source": options.push_source,
            "version": inputs.version,
            "tag": inputs.tag,
            "commit": inputs.commit,
            "digests": dict(digests),
            "package_states": dict(states),
        }
        outputs = {"chocolatey_states": format_states(states)}
        print(
            f"Chocolatey query={options.query_source} push={options.push_source} "
            f"{inputs.version}: {format_states(states)}"
        )
    else:
        raise PublicationError(f"unhandled publisher command {options.command!r}")

    if options.report is not None:
        write_report(options.report, report)
    if options.github_output is not None:
        release_assets.append_github_outputs(options.github_output, outputs)
    return 0


def main(arguments: Sequence[str] | None = None) -> int:
    """Convert channel conflicts into a stable nonzero status."""

    try:
        return run(sys.argv[1:] if arguments is None else arguments)
    except (
        OSError,
        PublicationError,
        channels.ChannelError,
        release_assets.ReleaseError,
        UnicodeError,
        json.JSONDecodeError,
    ) as error:
        print(f"package publication failed: {error}", file=sys.stderr)
        return 1


if __name__ == "__main__":
    raise SystemExit(main())
