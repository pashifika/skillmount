#!/usr/bin/env python3
"""Prove the SkillMount Homebrew Formula lifecycle inside a disposable local tap."""

from __future__ import annotations

import argparse
import hashlib
import json
import os
import re
import shutil
import subprocess
import sys
import tempfile
from dataclasses import dataclass, field
from pathlib import Path
from typing import Any, Iterable, Mapping, Protocol, Sequence

import release
import shell_completion_acceptance as native_shell

ENABLE_VARIABLE = "SKILLMOUNT_HOMEBREW_ACCEPTANCE"
ENABLE_VALUE = "1"
ACCEPTANCE_TAP = "skillmount-acceptance/homebrew-tap"
SUPPORTED_PREFIX = "/opt/homebrew"
FORMULA_IDS = ("skillmount", "skillmount-asm")
PRIOR_TAG = "v0.1.0"
REPORT_SCHEMA = 1
EVIDENCE_LIMIT = 4000
DEFAULT_TIMEOUT = 900
BUILD_TIMEOUT = 5400
SENTINEL_CONTENT = b"skillmount homebrew acceptance sentinel; unrelated to any Formula\n"
BUILD_DEPENDENCY_NOTICE = (
    "notice: both Formulae build from source, so Homebrew may install their declared build "
    "dependency `rust`; this harness never uninstalls a Formula it did not install, so an "
    "unrelated keg such as `rust` is left exactly as Homebrew leaves it"
)
SHARED_PLACEHOLDER = "<asm|skillmount>"
COMPLETIONS_MARKER = "completions"
COMPLETION_SHELLS = ("bash", "zsh", "fish")
COMPLETION_LOCATIONS: dict[str, tuple[tuple[str, str], ...]] = {
    "bash": (
        ("etc/bash_completion.d", "{command}"),
        ("share/bash-completion/completions", "{command}"),
    ),
    "zsh": (("share/zsh/site-functions", "_{command}"),),
    "fish": (("share/fish/vendor_completions.d", "{command}.fish"),),
}
REGISTRATION_MARKERS = {
    "bash": "complete -F _{command}",
    "zsh": "#compdef {command}",
    "fish": "complete -c {command}",
}
PROFILE_PATHS = (
    ".bashrc",
    ".bash_profile",
    ".profile",
    ".zshenv",
    ".zshrc",
    ".zprofile",
    ".config/fish/config.fish",
)
HOMEBREW_PINS = {
    "HOMEBREW_NO_ANALYTICS": "1",
    "HOMEBREW_NO_AUTO_UPDATE": "1",
    "HOMEBREW_NO_ENV_HINTS": "1",
    "HOMEBREW_NO_INSTALL_CLEANUP": "1",
    "HOMEBREW_NO_INSTALLED_DEPENDENTS_CHECK": "1",
    "HOMEBREW_NO_INSTALL_UPGRADE": "1",
}
PHASE_ORDER = (
    "style",
    "audit",
    "trust",
    "install-skillmount-alone",
    "selected-only",
    "completions",
    "brew-test",
    "uninstall",
    "install-asm-alone",
    "co-install",
    "cross-uninstall",
    "upgrade-from-prior",
    "sentinel-unchanged",
)
PHASE_REQUIREMENTS: dict[str, tuple[str, ...]] = {
    "selected-only": ("install-skillmount-alone", "install-asm-alone"),
    "completions": ("install-skillmount-alone", "install-asm-alone"),
    "brew-test": ("install-skillmount-alone", "install-asm-alone"),
    "uninstall": ("install-skillmount-alone", "install-asm-alone"),
    "cross-uninstall": ("co-install",),
    "install-skillmount-alone": ("trust",),
    "install-asm-alone": ("trust",),
    "co-install": ("trust",),
    "upgrade-from-prior": ("trust",),
}
PACKAGE_TABLE_PINS = {
    "skillmount": ("skillmount", "asm"),
    "skillmount-asm": ("asm", "skillmount"),
}
CARGO_PACKAGE_HEADER = "[package]"
CARGO_VERSION_PATTERN = re.compile(r'^version = "([^"\n]+)"[ \t]*$', re.MULTILINE)
CARGO_SECTION_PATTERN = re.compile(r"^\[", re.MULTILINE)
AUDIT_OFFENSE_PATTERN = re.compile(r"^\s*(?:\*\s+)?(?:line \d+:|.*\bline \d+, col \d+\b)", re.I)
LOCAL_SOURCE_AUDIT_ALLOWANCES = (
    # `brew audit --strict` requires an HTTPS stable URL. The local rehearsal deliberately
    # renders a `file://` source so the lifecycle can be proven before any tag is published.
    re.compile(r"(?i)file://"),
    re.compile(r"(?i)\bhttps\b"),
    re.compile(r"(?i)\bsecure\b.*\burl\b"),
    re.compile(r"(?i)\bunversioned\b|\bstable url\b|\bversion\b.*\bdetected from url\b"),
)
TRUST_JSON_VERSION = "v1"
TRUST_SECTIONS = ("casks", "commands", "formulae", "taps")
TRUST_STORE_UNDER_XDG = "homebrew/trust.json"
TRUST_STORE_UNDER_HOME = ".homebrew/trust.json"
TRUST_REFUSAL_TEMPLATE = (
    "Refusing to load formula {reference} from untrusted tap {tap}. Run "
    "'brew trust --formula {reference}' or 'brew trust {tap}' to trust it."
)
TRUST_HELP_NOTICE = (
    "`brew trust --help` must still document `--formula`, and `brew untrust --help` must still "
    "document `--formula`; Homebrew 6 refuses to load any third-party tap formula until it is "
    "trusted by name, and this harness owns a disposable tap"
)
TRUST_SCOPE_NOTICE = (
    "notice: `brew style` and `brew audit --strict --formula` load an untrusted third-party tap "
    "without complaint, so trust is scoped to the install path: `brew install`, `brew test`, "
    "`brew upgrade`, and `brew uninstall`"
)
TRUST_NAME_NOTICE = (
    "notice: Homebrew keys trust by name, never by content, so one `brew trust --formula` "
    "survives every later rewrite of the same Formula file; the upgrade rehearsal reuses it and "
    "an operator trusts a tap formula once rather than once per release"
)
TRUST_RESTORE_MECHANISM = (
    "`brew untrust --formula <reference>` removes exactly the names this harness added, so an "
    "entry the operator trusted before the run is never touched; when the store still differs "
    "from the captured state, cleanup rewrites that one plain JSON file with the exact bytes "
    "observed before trusting, or removes the file when it did not exist"
)


class HomebrewAcceptanceError(RuntimeError):
    """A required Homebrew lifecycle observation could not be proved."""


@dataclass(frozen=True)
class CommandEvidence:
    """One completed shell-free command retained as report evidence."""

    argv: tuple[str, ...]
    returncode: int
    stdout: str
    stderr: str

    @property
    def label(self) -> str:
        """Return a short human label for this command."""

        return " ".join(self.argv[:4])

    def to_json_object(self) -> dict[str, object]:
        """Return the bounded JSON shape recorded in the report."""

        return {
            "argv": list(self.argv),
            "status": self.returncode,
            "stdout": bounded_text(self.stdout),
            "stderr": bounded_text(self.stderr),
        }


@dataclass
class Phase:
    """Accumulating evidence for one named lifecycle phase."""

    name: str
    status: str = "pending"
    reason: str = ""
    findings: list[str] = field(default_factory=list)
    observations: list[str] = field(default_factory=list)
    commands: list[CommandEvidence] = field(default_factory=list)

    def record(self, evidence: CommandEvidence) -> CommandEvidence:
        """Retain one command as evidence for this phase."""

        self.commands.append(evidence)
        return evidence

    def note(self, message: str) -> None:
        """Retain one human-readable observation for this phase."""

        self.observations.append(message)

    def add(self, findings: Iterable[str]) -> None:
        """Retain zero or more failure findings for this phase."""

        self.findings.extend(findings)

    def skip(self, reason: str) -> None:
        """Mark this phase as deliberately not exercised."""

        self.status = "skipped"
        self.reason = reason

    def settle(self) -> None:
        """Resolve a pending phase from its accumulated findings."""

        if self.status in ("skipped", "failed"):
            return
        self.status = "failed" if self.findings else "passed"

    def to_json_object(self) -> dict[str, object]:
        """Return the JSON shape recorded in the report."""

        return {
            "name": self.name,
            "status": self.status,
            "reason": self.reason,
            "findings": list(self.findings),
            "observations": list(self.observations),
            "commands": [evidence.to_json_object() for evidence in self.commands],
        }


@dataclass(frozen=True)
class ScenarioCoverage:
    """One `homebrew-distribution` scenario mapped onto harness phases."""

    requirement: str
    scenario: str
    phases: tuple[str, ...]
    kind: str
    note: str

    def to_json_object(self) -> dict[str, object]:
        """Return the JSON shape recorded in the report."""

        return {
            "requirement": self.requirement,
            "scenario": self.scenario,
            "phases": list(self.phases),
            "kind": self.kind,
            "note": self.note,
        }


@dataclass(frozen=True)
class SourceArchive:
    """The source tarball identity both rendered Formulae pin."""

    url: str
    sha256: str
    path: Path | None

    @property
    def local(self) -> bool:
        """Return whether the pinned source is a locally built tarball."""

        return self.url.startswith("file://")

    def to_json_object(self) -> dict[str, object]:
        """Return the JSON shape recorded in the report."""

        return {
            "url": self.url,
            "sha256": self.sha256,
            "path": str(self.path) if self.path is not None else None,
            "local": self.local,
        }


@dataclass(frozen=True)
class UpgradeDecision:
    """Whether the prior released source can serve as an upgrade rehearsal."""

    status: str
    reason: str

    @property
    def eligible(self) -> bool:
        """Return whether the rehearsal can run."""

        return self.status == "eligible"


@dataclass(frozen=True)
class TrustStore:
    """Homebrew's trust file and its parsed sections at one observed moment."""

    path: Path
    existed: bool
    content: bytes
    sections: Mapping[str, tuple[str, ...]]

    @property
    def digest(self) -> str | None:
        """Return the SHA-256 of the observed file, or `None` when it was absent."""

        return hashlib.sha256(self.content).hexdigest() if self.existed else None

    def to_json_object(self) -> dict[str, object]:
        """Return the JSON shape recorded in the report, naming no unrelated tap."""

        return {
            "path": str(self.path),
            "existed": self.existed,
            "bytes": len(self.content),
            "sha256": self.digest,
            "trusted_formulae": len(self.sections.get("formulae", ())),
        }


@dataclass(frozen=True)
class HarnessOptions:
    """Fully resolved, immutable harness inputs."""

    repository: Path
    template_directory: Path
    formula_ids: tuple[str, ...]
    phases: tuple[str, ...]
    version: str
    tag: str
    commit: str | None
    source: SourceArchive | None
    require_upgrade: bool
    prior_tag: str


SCENARIO_COVERAGE: tuple[ScenarioCoverage, ...] = (
    ScenarioCoverage(
        requirement="An upstream tap owns two selectable SkillMount Formulae",
        scenario="Operator selects the descriptive command",
        phases=("install-skillmount-alone", "selected-only"),
        kind="direct",
        note="Installs the tap-resolved skillmount Formula and proves only skillmount lands.",
    ),
    ScenarioCoverage(
        requirement="An upstream tap owns two selectable SkillMount Formulae",
        scenario="Operator selects the short command",
        phases=("install-asm-alone", "selected-only"),
        kind="direct",
        note="Installs the tap-resolved skillmount-asm Formula and proves only asm lands.",
    ),
    ScenarioCoverage(
        requirement="An upstream tap owns two selectable SkillMount Formulae",
        scenario="Untrusted tap is refused",
        phases=("trust",),
        kind="analogue",
        note=(
            "Homebrew 6 refuses to load a third-party tap formula until it is trusted by name. "
            "The harness records the prior trust store, trusts only its own disposable tap "
            "formulae with `brew trust --formula`, refuses to install any reference it did not "
            "trust so nothing builds, and untrusts exactly what it added; the refusal itself is "
            "Homebrew's, quoted verbatim in the failure message rather than provoked by a real "
            "untrusted install."
        ),
    ),
    ScenarioCoverage(
        requirement="An upstream tap owns two selectable SkillMount Formulae",
        scenario="Tap repository is unavailable or unauthorized",
        phases=("style",),
        kind="analogue",
        note=(
            "Every Formula reference resolves through the disposable tap this harness owns, and "
            "the run stops before any install when that tap cannot be created or does not own "
            "the resolved file; upstream ownership is proved by package_publish.reconcile_tap."
        ),
    ),
    ScenarioCoverage(
        requirement="Both Formulae build one immutable source version on the supported platform",
        scenario="Supported paired source builds run",
        phases=("audit", "install-skillmount-alone", "install-asm-alone", "brew-test"),
        kind="direct",
        note="Both Formulae pin one rendered source identity and build the selected Cargo binary.",
    ),
    ScenarioCoverage(
        requirement="Both Formulae build one immutable source version on the supported platform",
        scenario="One Formula source identity differs",
        phases=("audit",),
        kind="direct",
        note="package_channels.inspect_formulae compares the rendered pair before any install.",
    ),
    ScenarioCoverage(
        requirement="Both Formulae build one immutable source version on the supported platform",
        scenario="Unsupported platform requests installation",
        phases=("audit",),
        kind="static",
        note=(
            "The rendered Formulae are asserted to require macOS and arm64 and to declare no "
            "Linux or Intel fallback; an Apple Silicon runner cannot execute the rejection."
        ),
    ),
    ScenarioCoverage(
        requirement="Each Formula installs exactly its selected executable",
        scenario="Each Formula is installed alone",
        phases=("selected-only",),
        kind="direct",
        note="Keg and prefix inspection require exactly one product executable per keg.",
    ),
    ScenarioCoverage(
        requirement="Each Formula installs exactly its selected executable",
        scenario="Both Formulae are co-installed",
        phases=("co-install",),
        kind="direct",
        note="Both commands report the same version from their own kegs with no link conflict.",
    ),
    ScenarioCoverage(
        requirement="Each Formula installs exactly its selected executable",
        scenario="A Formula contains the unselected executable",
        phases=("selected-only", "brew-test"),
        kind="direct",
        note="keg_findings fails a keg holding both executables; brew test repeats the assertion.",
    ),
    ScenarioCoverage(
        requirement="Each Formula generates completion only for its selected command",
        scenario="Descriptive-command completions are installed",
        phases=("completions",),
        kind="direct",
        note="Bash, Zsh, and Fish files register skillmount, parse natively, and never name asm.",
    ),
    ScenarioCoverage(
        requirement="Each Formula generates completion only for its selected command",
        scenario="Short-command completions are installed",
        phases=("completions",),
        kind="direct",
        note="Bash, Zsh, and Fish files register asm, parse natively, and never name skillmount.",
    ),
    ScenarioCoverage(
        requirement="Each Formula generates completion only for its selected command",
        scenario="Both completion sets are co-installed",
        phases=("co-install", "cross-uninstall"),
        kind="direct",
        note="Each shell owns one file per Formula, and uninstalling one keeps the other's files.",
    ),
    ScenarioCoverage(
        requirement="Paired Formula updates use protected review and CI",
        scenario="New paired Formula update is valid",
        phases=("style", "audit", "brew-test", "upgrade-from-prior"),
        kind="analogue",
        note=(
            "The harness runs the same style, audit, build, test, and upgrade battery a tap "
            "change must pass; merge eligibility itself is package_publish.reconcile_tap."
        ),
    ),
    ScenarioCoverage(
        requirement="Paired Formula updates use protected review and CI",
        scenario="Existing tap state is partially matching",
        phases=("co-install",),
        kind="analogue",
        note=(
            "Adding the second Formula while the first stays installed proves the pair members "
            "are independent; resuming a partial tap change is package_publish.reconcile_tap."
        ),
    ),
    ScenarioCoverage(
        requirement="Paired Formula updates use protected review and CI",
        scenario="Existing tap state conflicts",
        phases=("audit",),
        kind="analogue",
        note=(
            "A pair whose source identity disagrees is rejected before any install; refusing to "
            "overwrite conflicting tap state is package_publish.reconcile_tap."
        ),
    ),
    ScenarioCoverage(
        requirement="Homebrew lifecycle is clean and independently owned",
        scenario="Complete paired lifecycle succeeds",
        phases=PHASE_ORDER,
        kind="direct",
        note="Every phase runs in one isolated tap and the report names Homebrew, Rust, shell, "
        "and OS versions plus observed ownership paths.",
    ),
    ScenarioCoverage(
        requirement="Homebrew lifecycle is clean and independently owned",
        scenario="One Formula is removed from a co-installation",
        phases=("cross-uninstall",),
        kind="direct",
        note="The removed Formula's keg, link, and completions vanish while the other still runs.",
    ),
    ScenarioCoverage(
        requirement="Homebrew lifecycle is clean and independently owned",
        scenario="Uninstall encounters unrelated files",
        phases=("uninstall", "sentinel-unchanged"),
        kind="direct",
        note="Sentinels outside the kegs, including sibling completion files, stay byte-identical.",
    ),
)


def bounded_text(text: str, limit: int = EVIDENCE_LIMIT) -> str:
    """Return *text* trimmed to its most diagnostic tail."""

    if limit <= 0:
        raise HomebrewAcceptanceError(f"evidence limit is {limit}; expected a positive value")
    if len(text) <= limit:
        return text
    elided = len(text) - limit
    return f"[{elided} earlier characters elided]\n{text[-limit:]}"


def require_enabled(environment: Mapping[str, str]) -> None:
    """Refuse to touch Homebrew unless the operator opted in explicitly."""

    observed = environment.get(ENABLE_VARIABLE)
    if observed != ENABLE_VALUE:
        raise HomebrewAcceptanceError(
            f"{ENABLE_VARIABLE} is {observed!r}; expected {ENABLE_VALUE!r} because this harness "
            "installs, upgrades, and uninstalls Homebrew formulae on this machine"
        )


def parse_formula_list(output: str) -> tuple[str, ...]:
    """Parse `brew list --formula` output into sorted formula names."""

    return tuple(sorted(set(output.split())))


def require_clean_formula_state(output: str) -> None:
    """Refuse to run when either product Formula is already installed."""

    installed = tuple(
        name for name in parse_formula_list(output) if name.rsplit("/", 1)[-1] in FORMULA_IDS
    )
    if installed:
        raise HomebrewAcceptanceError(
            f"brew list --formula already reports {list(installed)}; expected neither of "
            f"{list(FORMULA_IDS)} so this harness never removes a keg it did not install"
        )


def parse_single_path(output: str, *, label: str) -> Path:
    """Parse one absolute path printed on its own line."""

    lines = [line.strip() for line in output.splitlines() if line.strip()]
    if len(lines) != 1:
        raise HomebrewAcceptanceError(
            f"{label} printed {len(lines)} path lines ({lines}); expected exactly 1"
        )
    path = Path(lines[0])
    if not path.is_absolute():
        raise HomebrewAcceptanceError(f"{label} printed {lines[0]!r}; expected an absolute path")
    return path


def require_supported_prefix(output: str) -> Path:
    """Refuse to run outside an Apple Silicon Homebrew prefix."""

    prefix = parse_single_path(output, label="brew --prefix")
    supported = Path(SUPPORTED_PREFIX)
    if prefix != supported and supported not in prefix.parents:
        raise HomebrewAcceptanceError(
            f"brew --prefix is {prefix}; expected {SUPPORTED_PREFIX} or a path under it because "
            "both Formulae are restricted to Apple Silicon macOS"
        )
    return prefix


def select_formulae(requested: Sequence[str] | None) -> tuple[str, ...]:
    """Return the selected package ids in immutable pair order."""

    if not requested:
        return FORMULA_IDS
    unknown = sorted(set(requested).difference(FORMULA_IDS))
    if unknown:
        raise HomebrewAcceptanceError(
            f"--formula named {unknown}; expected values from {list(FORMULA_IDS)}"
        )
    chosen = set(requested)
    return tuple(package_id for package_id in FORMULA_IDS if package_id in chosen)


def expand_phases(requested: Sequence[str] | None) -> tuple[str, ...]:
    """Return the selected phases plus every prerequisite, in canonical order."""

    if not requested:
        return PHASE_ORDER
    unknown = sorted(set(requested).difference(PHASE_ORDER))
    if unknown:
        raise HomebrewAcceptanceError(
            f"--phase named {unknown}; expected values from {list(PHASE_ORDER)}"
        )
    selected = set(requested)
    pending = list(selected)
    while pending:
        name = pending.pop()
        for prerequisite in PHASE_REQUIREMENTS.get(name, ()):
            if prerequisite not in selected:
                selected.add(prerequisite)
                pending.append(prerequisite)
    return tuple(name for name in PHASE_ORDER if name in selected)


def validate_digest(value: str, *, label: str) -> str:
    """Validate one lowercase hexadecimal SHA-256 digest."""

    if re.fullmatch(r"[0-9a-f]{64}", value) is None:
        raise HomebrewAcceptanceError(
            f"{label} is {value!r}; expected 64 lowercase hexadecimal characters"
        )
    return value


def source_override(url: str | None, digest: str | None) -> SourceArchive | None:
    """Validate an operator-supplied source URL and digest pair."""

    if url is None:
        if digest is not None:
            raise HomebrewAcceptanceError(
                "--source-sha256 was passed without --source-url-override; expected both or "
                "neither because the locally built tarball is digested by this harness"
            )
        return None
    if digest is None:
        raise HomebrewAcceptanceError(
            f"--source-url-override is {url!r} without --source-sha256; expected the exact "
            "digest of that tarball because Homebrew verifies the download before building"
        )
    if not (url.startswith("https://") or url.startswith("file://")):
        raise HomebrewAcceptanceError(
            f"--source-url-override is {url!r}; expected an https:// or file:// URL"
        )
    if any(character.isspace() for character in url):
        raise HomebrewAcceptanceError(f"--source-url-override is {url!r}; expected no whitespace")
    digest = validate_digest(digest, label="--source-sha256")
    return SourceArchive(url=url, sha256=digest, path=None)


def cargo_version_from_manifest(text: str) -> str:
    """Return the `[package]` version declared in a Cargo manifest."""

    start = text.find(CARGO_PACKAGE_HEADER)
    if start < 0:
        raise HomebrewAcceptanceError(
            f"Cargo manifest has no {CARGO_PACKAGE_HEADER} table; expected exactly one"
        )
    body = text[start + len(CARGO_PACKAGE_HEADER) :]
    next_section = CARGO_SECTION_PATTERN.search(body)
    if next_section is not None:
        body = body[: next_section.start()]
    matches = CARGO_VERSION_PATTERN.findall(body)
    if len(matches) != 1:
        raise HomebrewAcceptanceError(
            f"Cargo {CARGO_PACKAGE_HEADER} table declares {len(matches)} version keys ({matches}); "
            "expected exactly 1"
        )
    return release.validate_stable_version(matches[0])


def version_order(version: str) -> tuple[int, int, int]:
    """Return one validated stable version as a comparable tuple."""

    release.validate_stable_version(version)
    major, minor, patch = version.split(".")
    return (int(major), int(minor), int(patch))


def upgrade_decision(
    *,
    prior_tag: str,
    prior_version: str | None,
    candidate_version: str,
    prior_cli_source: str | None,
    require_upgrade: bool,
) -> UpgradeDecision:
    """Decide whether the prior released source can be upgraded from."""

    reason = ""
    if prior_cli_source is None:
        reason = (
            f"{prior_tag} has no src/cli.rs, so it predates the public "
            f"`{COMPLETIONS_MARKER}` command the Formula requires"
        )
    elif COMPLETIONS_MARKER not in prior_cli_source.lower():
        reason = (
            f"{prior_tag} src/cli.rs never mentions `{COMPLETIONS_MARKER}`, so it predates the "
            "public completion command the Formula requires"
        )
    elif prior_version is None:
        reason = f"{prior_tag} has no readable Cargo manifest version"
    elif prior_version == candidate_version:
        reason = (
            f"prior version {prior_version} equals candidate version {candidate_version}, so an "
            "upgrade rehearsal would observe no version change"
        )
    elif version_order(prior_version) > version_order(candidate_version):
        reason = (
            f"prior version {prior_version} is newer than candidate version {candidate_version}"
        )
    if not reason:
        return UpgradeDecision(
            status="eligible",
            reason=f"upgrading {prior_version} to {candidate_version} from {prior_tag}",
        )
    if require_upgrade:
        return UpgradeDecision(status="failed", reason=f"--require-upgrade was passed but {reason}")
    return UpgradeDecision(status="skipped", reason=reason)


def digest_or_none(path: Path) -> str | None:
    """Return a SHA-256 digest for one readable file, following Homebrew's links."""

    if not path.is_file():
        return None
    digest = hashlib.sha256()
    with path.open("rb") as handle:
        for chunk in iter(lambda: handle.read(1024 * 1024), b""):
            digest.update(chunk)
    return digest.hexdigest()


def capture_digests(paths: Iterable[Path]) -> dict[str, str | None]:
    """Return digests for every named path, recording absence as `None`."""

    return {str(path): digest_or_none(path) for path in paths}


def sentinel_findings(
    before: Mapping[str, str | None], after: Mapping[str, str | None]
) -> tuple[str, ...]:
    """Require every unrelated observed path to be byte-for-byte unchanged."""

    findings: list[str] = []
    for name in sorted(set(before).union(after)):
        if name not in after:
            findings.append(f"unrelated path {name} was not re-observed after the lifecycle")
            continue
        if name not in before:
            findings.append(f"unrelated path {name} was observed only after the lifecycle")
            continue
        first, second = before[name], after[name]
        if first == second:
            continue
        if first is None:
            findings.append(f"unrelated path {name} was created with digest {second}")
        elif second is None:
            findings.append(f"unrelated path {name} was removed; expected digest {first}")
        else:
            findings.append(f"unrelated path {name} digest is {second}; expected {first}")
    return tuple(findings)


def keg_versions(cellar: Path) -> tuple[str, ...]:
    """Return the installed keg version directory names for one Formula."""

    if not cellar.is_dir():
        return ()
    return tuple(sorted(child.name for child in cellar.iterdir() if child.is_dir()))


def select_keg(cellar: Path, *, version: str) -> Path:
    """Return the only installed keg, which must be *version*."""

    observed = keg_versions(cellar)
    if observed != (version,):
        raise HomebrewAcceptanceError(
            f"cellar {cellar} holds kegs {list(observed)}; expected exactly ['{version}']"
        )
    return cellar / version


def require_keg(cellar: Path, *, version: str) -> Path:
    """Return one named keg directory, which must exist."""

    keg = cellar / version
    if not keg.is_dir():
        raise HomebrewAcceptanceError(
            f"keg {keg} is missing; cellar {cellar} holds {list(keg_versions(cellar))}"
        )
    return keg


def executable_names_in(directory: Path) -> tuple[str, ...]:
    """Return sorted names of executable regular files directly in *directory*."""

    if not directory.is_dir():
        return ()
    names: list[str] = []
    for child in sorted(directory.iterdir()):
        if not child.is_file():
            continue
        if child.stat().st_mode & 0o111:
            names.append(child.name)
    return tuple(names)


def keg_findings(keg: Path, *, command: str, other_command: str) -> tuple[str, ...]:
    """Require exactly the selected executable inside one keg."""

    if not keg.is_dir():
        return (f"keg {keg} is not a directory",)
    findings: list[str] = []
    binaries = executable_names_in(keg / "bin")
    if binaries != (command,):
        findings.append(
            f"keg {keg} bin holds executables {list(binaries)}; expected exactly ['{command}']"
        )
    stray = sorted(str(path.relative_to(keg)) for path in keg.rglob(other_command))
    if stray:
        findings.append(
            f"keg {keg} contains pair-member entries {stray}; expected no entry named "
            f"{other_command!r}"
        )
    return tuple(findings)


def prefix_findings(
    prefix: Path, keg: Path, *, command: str, other_command: str, other_installed: bool
) -> tuple[str, ...]:
    """Require the prefix to expose only the commands whose Formulae are installed."""

    findings: list[str] = []
    linked = prefix / "bin" / command
    if not linked.exists():
        findings.append(f"{linked} is absent; expected a link into {keg}")
    else:
        target = Path(os.path.realpath(linked))
        owner = Path(os.path.realpath(keg))
        if owner != target and owner not in target.parents:
            findings.append(f"{linked} resolves to {target}; expected a path inside {owner}")
    other = prefix / "bin" / other_command
    observed = other.exists() or other.is_symlink()
    if observed is not other_installed:
        findings.append(
            f"{other} is {'present' if observed else 'absent'}; expected "
            f"{'present' if other_installed else 'absent'}"
        )
    return tuple(findings)


def completion_candidates(root: Path, *, command: str) -> dict[str, tuple[Path, ...]]:
    """Return every completion path Homebrew may own for one command."""

    return {
        shell: tuple(
            root / directory / template.format(command=command)
            for directory, template in COMPLETION_LOCATIONS[shell]
        )
        for shell in COMPLETION_SHELLS
    }


def completion_layout(root: Path, *, command: str) -> dict[str, tuple[Path, ...]]:
    """Return the existing completion files for one command under *root*."""

    return {
        shell: tuple(path for path in candidates if path.exists())
        for shell, candidates in completion_candidates(root, command=command).items()
    }


def completion_layout_findings(
    layout: Mapping[str, Sequence[Path]], *, command: str, label: str
) -> tuple[str, ...]:
    """Require exactly one completion file per shell for one command."""

    findings: list[str] = []
    for shell in COMPLETION_SHELLS:
        found = tuple(layout.get(shell, ()))
        if len(found) != 1:
            findings.append(
                f"{label} holds {len(found)} {shell} completion files for {command!r} "
                f"({[str(path) for path in found]}); expected exactly 1"
            )
    return tuple(findings)


def completion_text_findings(
    shell: str, text: str, *, command: str, other_command: str
) -> tuple[str, ...]:
    """Classify one completion script the way the shipped completion tests do."""

    if shell not in REGISTRATION_MARKERS:
        raise HomebrewAcceptanceError(
            f"completion shell is {shell!r}; expected one of {list(COMPLETION_SHELLS)}"
        )
    findings: list[str] = []
    if not text.strip():
        findings.append(f"{shell} completion for {command!r} is empty")
    marker = REGISTRATION_MARKERS[shell].format(command=command)
    if marker not in text:
        findings.append(
            f"{shell} completion for {command!r} lacks its registration {marker!r}"
        )
    other_marker = REGISTRATION_MARKERS[shell].format(command=other_command)
    if other_marker in text:
        findings.append(
            f"{shell} completion for {command!r} registers the pair member "
            f"{other_command!r} ({other_marker!r})"
        )
    if other_command in text:
        findings.append(
            f"{shell} completion for {command!r} mentions the pair member {other_command!r}"
        )
    if SHARED_PLACEHOLDER in text:
        findings.append(
            f"{shell} completion for {command!r} contains the shared command-model placeholder "
            f"{SHARED_PLACEHOLDER!r}"
        )
    return tuple(findings)


def linked_completion_findings(prefix: Path, keg: Path, *, command: str) -> tuple[str, ...]:
    """Require each keg completion file to be visible in the prefix with identical bytes."""

    findings: list[str] = []
    owned = completion_layout(keg, command=command)
    exposed = completion_layout(prefix, command=command)
    for shell in COMPLETION_SHELLS:
        keg_files = owned[shell]
        prefix_files = exposed[shell]
        if len(keg_files) != 1:
            continue
        if len(prefix_files) != 1:
            findings.append(
                f"prefix {prefix} exposes {len(prefix_files)} {shell} completion files for "
                f"{command!r} ({[str(path) for path in prefix_files]}); expected exactly 1 for "
                f"keg file {keg_files[0]}"
            )
            continue
        keg_digest = digest_or_none(keg_files[0])
        prefix_digest = digest_or_none(prefix_files[0])
        if keg_digest != prefix_digest:
            findings.append(
                f"{prefix_files[0]} digest is {prefix_digest}; expected {keg_digest} from "
                f"{keg_files[0]}"
            )
    return tuple(findings)


def uninstall_findings(
    prefix: Path, keg: Path, *, command: str, owned: Mapping[str, str | None]
) -> tuple[str, ...]:
    """Require one Formula's keg, link, and completion files to be gone."""

    findings: list[str] = []
    if keg.exists():
        findings.append(f"keg {keg} still exists after uninstall")
    linked = prefix / "bin" / command
    if linked.exists() or linked.is_symlink():
        findings.append(f"{linked} survived the uninstall of {command!r}")
    for name, digest in sorted(owned.items()):
        path = Path(name)
        if path.is_symlink() and not path.exists():
            findings.append(f"{path} is a dangling link after uninstall")
            continue
        current = digest_or_none(path)
        if current is not None and current == digest:
            findings.append(
                f"Formula-owned completion file {path} survived uninstall with digest {digest}"
            )
    return tuple(findings)


def version_findings(output: str, *, command: str, version: str) -> tuple[str, ...]:
    """Require one executable to report exactly the Formula version."""

    expected = f"{release.PRODUCT_NAME} {version}\n"
    if output != expected:
        return (f"{command} --version printed {output!r}; expected {expected!r}",)
    return ()


def platform_findings(text: str, *, formula_class: str) -> tuple[str, ...]:
    """Require one rendered Formula to build only on Apple Silicon macOS."""

    findings: list[str] = []
    for required in ('depends_on "rust" => :build', "depends_on :macos", "depends_on arch: :arm64"):
        if required not in text:
            findings.append(f"{formula_class} does not declare `{required}`")
    for forbidden in ("conflicts_with", "on_linux", "depends_on :linux", "bottle do"):
        if forbidden in text:
            findings.append(f"{formula_class} declares `{forbidden}`; expected it to be absent")
    return tuple(findings)


def audit_findings(output: str, *, local_source: bool) -> tuple[str, ...]:
    """Return `brew audit` offences that the local rehearsal does not explain."""

    findings: list[str] = []
    for line in output.splitlines():
        candidate = line.strip()
        if not candidate or AUDIT_OFFENSE_PATTERN.match(candidate) is None:
            continue
        if local_source and any(
            allowance.search(candidate) for allowance in LOCAL_SOURCE_AUDIT_ALLOWANCES
        ):
            continue
        findings.append(f"brew audit reported {candidate!r}")
    return tuple(findings)


def canonical_tap_name(tap: str) -> str:
    """Return the `user/name` tap Homebrew reports for a `user/homebrew-name` repository."""

    user, separator, repository = tap.partition("/")
    if not user or not separator or not repository or "/" in repository:
        raise HomebrewAcceptanceError(
            f"tap {tap!r} is not a `user/repository` pair; expected exactly one `/`"
        )
    return f"{user}/{repository.removeprefix('homebrew-')}"


def canonical_reference(reference: str) -> str:
    """Return the tap-qualified formula name Homebrew records in its trust store."""

    tap, separator, name = reference.rpartition("/")
    if not tap or not separator or not name:
        raise HomebrewAcceptanceError(
            f"formula reference {reference!r} is not `user/repository/formula`; expected two `/`"
        )
    return f"{canonical_tap_name(tap)}/{name}"


def trust_spellings(reference: str) -> tuple[str, ...]:
    """Return every spelling Homebrew may store for one tap-qualified formula."""

    canonical = canonical_reference(reference)
    return (reference,) if canonical == reference else (reference, canonical)


def trust_refusal(reference: str) -> str:
    """Return the exact refusal Homebrew prints for an untrusted third-party tap."""

    canonical = canonical_reference(reference)
    tap, _, _ = canonical.rpartition("/")
    return TRUST_REFUSAL_TEMPLATE.format(reference=canonical, tap=tap)


def require_absolute(value: str, *, label: str) -> Path:
    """Return one environment path that Homebrew resolves its trust store under."""

    path = Path(value)
    if not path.is_absolute():
        raise HomebrewAcceptanceError(
            f"{label} is {value!r}; expected an absolute path because Homebrew resolves its "
            "trust store under it"
        )
    return path


def trust_store_path(environment: Mapping[str, str]) -> Path:
    """Return the trust file Homebrew reads, honouring `XDG_CONFIG_HOME`."""

    configuration = environment.get("XDG_CONFIG_HOME", "").strip()
    if configuration:
        return require_absolute(configuration, label="XDG_CONFIG_HOME") / TRUST_STORE_UNDER_XDG
    home = environment.get("HOME", "").strip()
    if not home:
        raise HomebrewAcceptanceError(
            "neither XDG_CONFIG_HOME nor HOME is set; expected one because Homebrew stores "
            f"trusted entries in ${{XDG_CONFIG_HOME}}/{TRUST_STORE_UNDER_XDG} or "
            f"~/{TRUST_STORE_UNDER_HOME}"
        )
    return require_absolute(home, label="HOME") / TRUST_STORE_UNDER_HOME


def read_trust_file(path: Path) -> tuple[bool, bytes]:
    """Return whether Homebrew's trust file exists and the exact bytes it holds."""

    try:
        return True, path.read_bytes()
    except FileNotFoundError:
        return False, b""
    except OSError as error:
        raise HomebrewAcceptanceError(
            f"{path} could not be read ({error}); this harness refuses to trust anything without "
            "a restorable copy of the prior trust store"
        ) from error


def parse_trust_json(text: str) -> dict[str, tuple[str, ...]]:
    """Parse `brew trust --json v1` into its four name lists, failing closed on drift."""

    stripped = text.strip()
    try:
        document = json.loads(stripped)
    except json.JSONDecodeError as error:
        raise HomebrewAcceptanceError(
            f"`brew trust --json {TRUST_JSON_VERSION}` printed {bounded_text(stripped)!r}, which "
            f"is not JSON: {error}"
        ) from error
    if not isinstance(document, dict) or sorted(document) != sorted(TRUST_SECTIONS):
        observed = sorted(document) if isinstance(document, dict) else type(document).__name__
        raise HomebrewAcceptanceError(
            f"`brew trust --json {TRUST_JSON_VERSION}` reported {observed}; expected exactly the "
            f"sections {sorted(TRUST_SECTIONS)}"
        )
    parsed: dict[str, tuple[str, ...]] = {}
    for section in TRUST_SECTIONS:
        values = document[section]
        if not isinstance(values, list) or not all(isinstance(name, str) for name in values):
            raise HomebrewAcceptanceError(
                f"`brew trust --json {TRUST_JSON_VERSION}` section {section!r} is {values!r}; "
                "expected a list of names"
            )
        parsed[section] = tuple(values)
    return parsed


def parse_tap_list(output: str) -> tuple[str, ...]:
    """Parse `brew tap` output into sorted tap names."""

    return tuple(sorted(set(output.split())))


def trust_sections_match(
    expected: Mapping[str, Sequence[str]], observed: Mapping[str, Sequence[str]]
) -> bool:
    """Return whether two trust snapshots hold the same names in every section."""

    return all(
        set(expected.get(section, ())) == set(observed.get(section, ()))
        for section in TRUST_SECTIONS
    )


def trust_query_failure(evidence: CommandEvidence) -> str:
    """Return the named failure an unusable `brew trust --json` query produces."""

    return (
        f"{' '.join(evidence.argv)} exited {evidence.returncode}; this harness must observe the "
        "prior trust state before it trusts anything so cleanup can restore exactly what it "
        f"added. {TRUST_HELP_NOTICE}: "
        f"{bounded_text(evidence.stderr or evidence.stdout).strip()}"
    )


def trust_failure(evidence: CommandEvidence, *, reference: str) -> str:
    """Return the named failure a refused or renamed `brew trust` produces."""

    return (
        f"{' '.join(evidence.argv)} exited {evidence.returncode}; expected 0 because Homebrew "
        f'refuses an untrusted third-party tap with "{trust_refusal(reference)}". '
        f"{TRUST_HELP_NOTICE}: "
        f"{bounded_text(evidence.stderr or evidence.stdout).strip()}"
    )


def untrusted_install_finding(reference: str) -> str:
    """Return the named failure an install attempted before trusting produces."""

    return (
        f"{reference} was never trusted, so Homebrew would refuse the install with "
        f'"{trust_refusal(reference)}"; the trust phase must record it before any install'
    )


def trust_drift_findings(
    before: Mapping[str, Sequence[str]],
    after: Mapping[str, Sequence[str]],
    *,
    added: Sequence[str],
) -> tuple[str, ...]:
    """Require the trust store to differ from its prior state only by *added* formulae."""

    permitted: set[str] = set()
    for reference in added:
        permitted.update(trust_spellings(reference))
    findings: list[str] = []
    for section in TRUST_SECTIONS:
        prior = set(before.get(section, ()))
        current = set(after.get(section, ()))
        lost = sorted(prior.difference(current))
        if lost:
            findings.append(
                f"trust section {section!r} lost {lost}; this harness must never remove an entry "
                "it did not add"
            )
        allowed = permitted if section == "formulae" else set()
        gained = sorted(current.difference(prior).difference(allowed))
        if gained:
            findings.append(
                f"trust section {section!r} gained {gained}; expected only {sorted(allowed)} to "
                "appear"
            )
    return tuple(findings)


def coverage_gaps() -> tuple[str, ...]:
    """Return every phase or scenario mapping inconsistency in this module."""

    findings: list[str] = []
    mapped: set[str] = set()
    for coverage in SCENARIO_COVERAGE:
        if coverage.kind not in ("direct", "static", "analogue"):
            findings.append(f"scenario {coverage.scenario!r} has unknown kind {coverage.kind!r}")
        if not coverage.phases:
            findings.append(f"scenario {coverage.scenario!r} maps to no phase")
        unknown = sorted(set(coverage.phases).difference(PHASE_ORDER))
        if unknown:
            findings.append(f"scenario {coverage.scenario!r} maps to unknown phases {unknown}")
        mapped.update(coverage.phases)
    uncovered = sorted(set(PHASE_ORDER).difference(mapped))
    if uncovered:
        findings.append(f"phases {uncovered} prove no scenario")
    return tuple(findings)


def aggregate_status(phases: Sequence[Phase], cleanup_findings: Sequence[str] = ()) -> str:
    """Return the report status implied by every phase and by cleanup."""

    statuses = {phase.status for phase in phases}
    if cleanup_findings or "failed" in statuses:
        return "failed"
    if "pending" in statuses:
        return "incomplete"
    if "passed" in statuses:
        return "passed"
    return "skipped"


def build_report(
    *,
    options: HarnessOptions,
    commit: str,
    source: SourceArchive | None,
    prefix: Path | None,
    environment: Mapping[str, object],
    trust: Mapping[str, object],
    phases: Sequence[Phase],
    cleanup: Sequence[CommandEvidence],
    cleanup_findings: Sequence[str],
) -> dict[str, object]:
    """Return the complete acceptance report document."""

    return {
        "schema": REPORT_SCHEMA,
        "product": release.PRODUCT_NAME,
        "status": aggregate_status(phases, cleanup_findings),
        "tap": ACCEPTANCE_TAP,
        "version": options.version,
        "tag": options.tag,
        "commit": commit,
        "prefix": str(prefix) if prefix is not None else None,
        "formulae": list(options.formula_ids),
        "selected_phases": list(options.phases),
        "require_upgrade": options.require_upgrade,
        "source": source.to_json_object() if source is not None else None,
        "environment": dict(environment),
        "trust": dict(trust),
        "phases": [phase.to_json_object() for phase in phases],
        "cleanup": {
            "commands": [evidence.to_json_object() for evidence in cleanup],
            "findings": list(cleanup_findings),
        },
        "scenario_coverage": [coverage.to_json_object() for coverage in SCENARIO_COVERAGE],
        "coverage_gaps": list(coverage_gaps()),
    }


def report_text(document: Mapping[str, object]) -> str:
    """Return the deterministic JSON serialization written to `--report`."""

    return json.dumps(document, sort_keys=True, indent=2) + "\n"


def coverage_text() -> str:
    """Return the human-readable scenario-to-phase mapping."""

    lines = ["homebrew-distribution scenario coverage:"]
    for coverage in SCENARIO_COVERAGE:
        lines.append(f"  {coverage.requirement}")
        lines.append(f"    {coverage.scenario} [{coverage.kind}]")
        lines.append(f"      phases: {', '.join(coverage.phases)}")
        lines.append(f"      note: {coverage.note}")
    gaps = coverage_gaps()
    lines.append(f"  gaps: {', '.join(gaps) if gaps else 'none'}")
    return "\n".join(lines)


def summary_text(document: Mapping[str, object]) -> str:
    """Return the phase status summary printed at the end of a run."""

    phases = document.get("phases")
    if not isinstance(phases, list):
        raise HomebrewAcceptanceError("report document has no phase list")
    lines = [f"Homebrew acceptance status: {document.get('status')}"]
    for phase in phases:
        if not isinstance(phase, dict):
            raise HomebrewAcceptanceError("report phase entry is not an object")
        detail = phase.get("reason") or ""
        findings = phase.get("findings") or []
        if findings:
            detail = "; ".join(str(finding) for finding in findings)
        suffix = f" ({detail})" if detail else ""
        lines.append(f"  {phase.get('name')}: {phase.get('status')}{suffix}")
    return "\n".join(lines)


def import_package_channels() -> Any:
    """Import the shared channel model only when a rendering step needs it."""

    try:
        import package_channels
    except ModuleNotFoundError as error:
        raise HomebrewAcceptanceError(
            "package_channels.py is required to render both Formulae; expected it next to "
            f"{Path(__file__).name}"
        ) from error
    observed = tuple(identity.package_id for identity in package_channels.PACKAGES)
    if observed != FORMULA_IDS:
        raise HomebrewAcceptanceError(
            f"package_channels.PACKAGES declares {list(observed)}; expected {list(FORMULA_IDS)}"
        )
    for package_id, (command, other) in PACKAGE_TABLE_PINS.items():
        identity = package_channels.package_for(package_id)
        if (identity.command, identity.other.command) != (command, other):
            raise HomebrewAcceptanceError(
                f"package_channels selects {identity.command!r}/{identity.other.command!r} for "
                f"{package_id}; expected {command!r}/{other!r}"
            )
    return package_channels


def run_command(
    argv: Sequence[str],
    *,
    cwd: Path | None = None,
    environment: Mapping[str, str] | None = None,
    timeout: int = DEFAULT_TIMEOUT,
) -> CommandEvidence:
    """Run one shell-free command and retain its bounded output as evidence."""

    arguments = tuple(str(argument) for argument in argv)
    try:
        completed = subprocess.run(
            arguments,
            cwd=str(cwd) if cwd is not None else None,
            env=dict(environment) if environment is not None else None,
            stdin=subprocess.DEVNULL,
            stdout=subprocess.PIPE,
            stderr=subprocess.PIPE,
            check=False,
            timeout=timeout,
        )
    except FileNotFoundError as error:
        raise HomebrewAcceptanceError(f"{arguments[0]} is not installed: {error}") from error
    except subprocess.TimeoutExpired as error:
        raise HomebrewAcceptanceError(
            f"{' '.join(arguments[:4])} exceeded {timeout}s"
        ) from error
    return CommandEvidence(
        argv=arguments,
        returncode=completed.returncode,
        stdout=completed.stdout.decode("utf-8", errors="replace"),
        stderr=completed.stderr.decode("utf-8", errors="replace"),
    )


class CommandGateway(Protocol):
    """The only boundary through which this harness reaches the machine."""

    environment: Mapping[str, str]
    """The environment every command sees, and which locates Homebrew's trust store."""

    def brew(self, *arguments: str, timeout: int = DEFAULT_TIMEOUT) -> CommandEvidence:
        """Run one `brew` subcommand."""

    def git(self, *arguments: str, timeout: int = DEFAULT_TIMEOUT) -> CommandEvidence:
        """Run one `git` subcommand inside the repository."""

    def tool(
        self, executable: str, *arguments: str, timeout: int = DEFAULT_TIMEOUT
    ) -> CommandEvidence:
        """Run one auxiliary executable such as `rustc` or an installed product command."""


class SubprocessGateway:
    """Thin shell-free adapter over `brew`, `git`, and installed executables."""

    def __init__(self, repository: Path, *, environment: Mapping[str, str] | None = None) -> None:
        """Bind every command to one repository and one pinned environment."""

        self.repository = repository.resolve()
        base = dict(os.environ if environment is None else environment)
        base.update(HOMEBREW_PINS)
        self.environment = base

    def brew(self, *arguments: str, timeout: int = DEFAULT_TIMEOUT) -> CommandEvidence:
        """Run one `brew` subcommand with Homebrew's implicit behaviour pinned off."""

        return run_command(
            ("brew", *arguments),
            cwd=self.repository,
            environment=self.environment,
            timeout=timeout,
        )

    def git(self, *arguments: str, timeout: int = DEFAULT_TIMEOUT) -> CommandEvidence:
        """Run one `git` subcommand inside the repository."""

        return run_command(
            ("git", *arguments),
            cwd=self.repository,
            environment=self.environment,
            timeout=timeout,
        )

    def tool(
        self, executable: str, *arguments: str, timeout: int = DEFAULT_TIMEOUT
    ) -> CommandEvidence:
        """Run one auxiliary executable outside Homebrew's control."""

        return run_command(
            (executable, *arguments),
            cwd=self.repository,
            environment=self.environment,
            timeout=timeout,
        )


class Harness:
    """Ordered Homebrew lifecycle stages recorded as named phase evidence."""

    def __init__(self, gateway: CommandGateway, options: HarnessOptions) -> None:
        """Bind one gateway and one immutable option set to a fresh phase table."""

        self.gateway = gateway
        self.options = options
        self.phases: dict[str, Phase] = {}
        for name in PHASE_ORDER:
            phase = Phase(name=name)
            if name not in options.phases:
                phase.skip("not selected by --phase")
            self.phases[name] = phase
        self.active: Phase | None = None
        self.prefix: Path | None = None
        self.commit = ""
        self.source: SourceArchive | None = None
        self.tap_root: Path | None = None
        self.tapped = False
        self.trusted: list[str] = []
        self.added_trust: list[str] = []
        self.trust_snapshot: TrustStore | None = None
        self.trust_evidence: dict[str, object] = {}
        self.installed: dict[str, Path] = {}
        self.commands: dict[str, tuple[str, str]] = {}
        self.formula_paths: dict[str, Path] = {}
        self.sentinels: dict[str, str | None] = {}
        self.created_sentinels: tuple[Path, ...] = ()
        self.profiles: dict[str, str | None] = {}
        self.cleanup_commands: list[CommandEvidence] = []
        self.cleanup_findings: list[str] = []
        self.environment_evidence: dict[str, object] = {}

    # Evidence helpers.

    def enabled(self, name: str) -> bool:
        """Return whether one phase was selected for this run."""

        return name in self.options.phases

    def phase(self, name: str) -> Phase:
        """Return and activate the accumulating record for one phase."""

        try:
            phase = self.phases[name]
        except KeyError as error:
            raise HomebrewAcceptanceError(f"unknown phase {name!r}") from error
        self.active = phase
        return phase

    def require(self, evidence: CommandEvidence) -> CommandEvidence:
        """Require one command to have succeeded, keeping it as evidence."""

        if self.active is not None:
            self.active.record(evidence)
        if evidence.returncode != 0:
            raise HomebrewAcceptanceError(
                f"{evidence.label} failed with status {evidence.returncode}: "
                f"{bounded_text(evidence.stderr or evidence.stdout).strip()}"
            )
        return evidence

    def check(self, phase: Phase, evidence: CommandEvidence, *, expectation: str) -> bool:
        """Record one command and turn a nonzero status into a finding."""

        phase.record(evidence)
        if evidence.returncode == 0:
            return True
        phase.add(
            (
                f"{evidence.label} exited {evidence.returncode}; expected 0 because {expectation}: "
                f"{bounded_text(evidence.stderr or evidence.stdout).strip()}",
            )
        )
        return False

    def formula_reference(self, package_id: str) -> str:
        """Return the fully qualified tap reference for one package id."""

        return f"{ACCEPTANCE_TAP}/{package_id}"

    def selection(self, package_id: str) -> tuple[str, str]:
        """Return the selected and pair-member commands for one package id."""

        try:
            return self.commands[package_id]
        except KeyError as error:
            raise HomebrewAcceptanceError(
                f"package {package_id!r} has no resolved command selection"
            ) from error

    # Environment and safety.

    def probe_environment(self) -> dict[str, object]:
        """Collect and print the Homebrew, Rust, OS, and shell versions."""

        evidence: list[CommandEvidence] = []
        brew_version = self.require_bare(self.gateway.brew("--version"), evidence)
        rustc_version = self.require_bare(self.gateway.tool("rustc", "--version"), evidence)
        os_version = self.require_bare(self.gateway.tool("sw_vers"), evidence)
        shells: dict[str, str] = {}
        for shell in COMPLETION_SHELLS:
            try:
                _, version = native_shell.require_interpreter(shell)
            except native_shell.AcceptanceError as error:
                shells[shell] = f"unavailable: {error}"
            else:
                shells[shell] = version
        record = {
            "brew": brew_version.stdout.strip().splitlines()[:2],
            "rustc": rustc_version.stdout.strip(),
            "sw_vers": os_version.stdout.strip().splitlines(),
            "shells": shells,
            "commands": [item.to_json_object() for item in evidence],
            "build_dependencies": BUILD_DEPENDENCY_NOTICE,
        }
        print(f"brew: {' | '.join(record['brew'])}")
        print(f"rustc: {record['rustc']}")
        print(f"sw_vers: {' | '.join(record['sw_vers'])}")
        for shell, version in shells.items():
            print(f"{shell}: {version}")
        print(BUILD_DEPENDENCY_NOTICE)
        self.environment_evidence = record
        return record

    def require_bare(
        self, evidence: CommandEvidence, collected: list[CommandEvidence]
    ) -> CommandEvidence:
        """Require one preamble command to have succeeded outside any phase."""

        collected.append(evidence)
        if evidence.returncode != 0:
            raise HomebrewAcceptanceError(
                f"{evidence.label} failed with status {evidence.returncode}: "
                f"{bounded_text(evidence.stderr or evidence.stdout).strip()}"
            )
        return evidence

    def require_safe_state(self) -> Path:
        """Refuse to run against an unsupported prefix or an occupied Formula name."""

        prefix = require_supported_prefix(self.gateway.brew("--prefix").stdout)
        listing = self.gateway.brew("list", "--formula")
        if listing.returncode != 0:
            raise HomebrewAcceptanceError(
                f"brew list --formula failed with status {listing.returncode}: "
                f"{bounded_text(listing.stderr or listing.stdout).strip()}"
            )
        require_clean_formula_state(listing.stdout)
        return prefix

    # Identity, source, and rendering.

    def resolve_identity(self) -> tuple[str, str]:
        """Return the candidate commit and whether the worktree is dirty."""

        requested = self.options.commit
        if requested is not None:
            self.require_bare(self.gateway.git("cat-file", "-e", f"{requested}^{{commit}}"), [])
            commit = requested
        else:
            head = self.require_bare(self.gateway.git("rev-parse", "HEAD"), [])
            commit = release.validate_commit(head.stdout.strip())
        status = self.gateway.git("status", "--porcelain")
        dirty = bool(status.stdout.strip())
        return commit, "dirty" if dirty else "clean"

    def build_source(self, work: Path) -> SourceArchive:
        """Return the candidate source identity, building it when none was given."""

        if self.options.source is not None:
            return self.options.source
        return self.archive_source(work, tag=self.options.tag, commit=self.commit)

    def archive_source(self, work: Path, *, tag: str, commit: str) -> SourceArchive:
        """Build and digest one source tarball with `git archive`."""

        archive = work / f"skillmount-{tag}.tar.gz"
        self.require_bare(
            self.gateway.git(
                "archive",
                "--format=tar.gz",
                f"--prefix=skillmount-{tag}/",
                "--output",
                str(archive),
                commit,
            ),
            [],
        )
        return SourceArchive(
            url=archive.as_uri(), sha256=release.sha256_file(archive), path=archive
        )

    def channel_inputs(
        self, channels: Any, *, source: SourceArchive, version: str, tag: str
    ) -> Any:
        """Return in-process `PackageInputs` pinning one already-digested source."""

        repository = channels.DEFAULT_REPOSITORY
        try:
            return channels.PackageInputs(
                repository=repository,
                version=version,
                tag=tag,
                commit=self.commit,
                release_url=f"https://github.com/{repository}/releases/tag/{tag}",
                source_url=source.url,
                source_sha256=source.sha256,
                archives=(),
            )
        except channels.ChannelError as error:
            raise HomebrewAcceptanceError(
                f"package_channels.PackageInputs rejected the source-only rehearsal identity "
                f"({error}); this harness observes no release archive, so it records none. Pass "
                "--inputs with a real preflight artifact, or let PackageInputs accept "
                "archives=() and keep archive completeness in from_json and preflight"
            ) from error

    def render_formulae(
        self, output: Path, *, source: SourceArchive, version: str, tag: str
    ) -> dict[str, Path]:
        """Render both Formulae from the tracked templates."""

        channels = import_package_channels()
        if not self.commands:
            self.commands = {
                identity.package_id: (identity.command, identity.other.command)
                for identity in channels.PACKAGES
            }
        inputs = self.channel_inputs(channels, source=source, version=version, tag=tag)
        try:
            rendered = channels.generate_formulae(
                inputs,
                template_directory=self.options.template_directory,
                output_directory=output,
            )
        except channels.ChannelError as error:
            raise HomebrewAcceptanceError(
                f"package_channels.generate_formulae rejected the templates in "
                f"{self.options.template_directory}: {error}"
            ) from error
        return {package_id: Path(path) for package_id, path in rendered.items()}

    def inspect_rendered_pair(self, phase: Phase, rendered: Mapping[str, Path]) -> None:
        """Require the rendered pair to share one source identity."""

        channels = import_package_channels()
        if len(rendered) != len(FORMULA_IDS):
            phase.note(
                f"pair inspection skipped: {sorted(rendered)} of {list(FORMULA_IDS)} rendered"
            )
            return
        inputs = self.channel_inputs(
            channels,
            source=self.require_source(),
            version=self.options.version,
            tag=self.options.tag,
        )
        try:
            channels.inspect_formulae(dict(rendered), inputs)
        except channels.ChannelError as error:
            phase.add((f"package_channels.inspect_formulae rejected the rendered pair: {error}",))
        else:
            phase.note("package_channels.inspect_formulae accepted the rendered pair")

    def require_source(self) -> SourceArchive:
        """Return the resolved source identity."""

        if self.source is None:
            raise HomebrewAcceptanceError("the source archive identity was never resolved")
        return self.source

    def require_prefix(self) -> Path:
        """Return the validated Homebrew prefix."""

        if self.prefix is None:
            raise HomebrewAcceptanceError("the Homebrew prefix was never validated")
        return self.prefix

    # Tap lifecycle.

    def create_tap(self) -> Path:
        """Create the disposable tap, refusing to reuse leftover state."""

        located = parse_single_path(
            self.require_bare(self.gateway.brew("--repository", ACCEPTANCE_TAP), []).stdout,
            label=f"brew --repository {ACCEPTANCE_TAP}",
        )
        if located.exists():
            raise HomebrewAcceptanceError(
                f"disposable tap directory {located} already exists; run "
                f"`brew untap {ACCEPTANCE_TAP}` before rerunning this harness"
            )
        self.require_bare(self.gateway.brew("tap-new", "--no-git", ACCEPTANCE_TAP), [])
        self.tapped = True
        formula_directory = located / "Formula"
        formula_directory.mkdir(parents=True, exist_ok=True)
        self.tap_root = located
        return located

    def place_formulae(self, rendered: Mapping[str, Path]) -> dict[str, Path]:
        """Copy the rendered Formulae into the disposable tap."""

        tap_root = self.tap_root
        if tap_root is None:
            raise HomebrewAcceptanceError("the disposable tap was never created")
        placed: dict[str, Path] = {}
        for package_id in self.options.formula_ids:
            source = rendered.get(package_id)
            if source is None:
                raise HomebrewAcceptanceError(
                    f"rendering produced no Formula for {package_id}; observed {sorted(rendered)}"
                )
            destination = tap_root / "Formula" / f"{package_id}.rb"
            shutil.copyfile(source, destination)
            placed[package_id] = destination
        self.formula_paths = placed
        return placed

    def uninstall(self, package_id: str, *, phase: Phase | None = None) -> CommandEvidence:
        """Uninstall exactly one Formula this harness installed."""

        evidence = self.gateway.brew(
            "uninstall", "--formula", self.formula_reference(package_id), timeout=DEFAULT_TIMEOUT
        )
        if phase is not None:
            phase.record(evidence)
        else:
            self.cleanup_commands.append(evidence)
        if evidence.returncode == 0:
            self.installed.pop(package_id, None)
        return evidence

    def read_trust(self, *, phase: Phase | None) -> dict[str, tuple[str, ...]]:
        """Return Homebrew's four trust sections, retaining the query as evidence."""

        evidence = self.gateway.brew("trust", "--json", TRUST_JSON_VERSION)
        if phase is not None:
            phase.record(evidence)
        else:
            self.cleanup_commands.append(evidence)
        if evidence.returncode != 0:
            raise HomebrewAcceptanceError(trust_query_failure(evidence))
        return parse_trust_json(evidence.stdout)

    def capture_trust(self, phase: Phase) -> TrustStore:
        """Record the trust store exactly as it was before this harness trusted anything."""

        sections = self.read_trust(phase=phase)
        path = trust_store_path(self.gateway.environment)
        existed, content = read_trust_file(path)
        store = TrustStore(path=path, existed=existed, content=content, sections=sections)
        self.trust_snapshot = store
        phase.note(
            f"captured the prior trust store {path} (existed={existed}, "
            f"{len(sections['formulae'])} trusted formulae) so cleanup can restore it"
        )
        return store

    def observed_brew_version(self) -> list[str]:
        """Return the `brew --version` lines this run observed."""

        observed = self.environment_evidence.get("brew")
        return [str(line) for line in observed] if isinstance(observed, list) else []

    def phase_trust(self) -> None:
        """Trust exactly the disposable tap's Formulae so Homebrew will load them."""

        if not self.enabled("trust"):
            return
        phase = self.phase("trust")
        if self.tap_root is None:
            raise HomebrewAcceptanceError("the disposable tap was never created")
        before = self.capture_trust(phase)
        for package_id in self.options.formula_ids:
            reference = self.formula_reference(package_id)
            evidence = phase.record(self.gateway.brew("trust", "--formula", reference))
            if evidence.returncode != 0:
                phase.add((trust_failure(evidence, reference=reference),))
                continue
            self.trusted.append(reference)
            if set(trust_spellings(reference)).isdisjoint(before.sections["formulae"]):
                self.added_trust.append(reference)
                phase.note(f"trusted {reference} with `{' '.join(evidence.argv)}`")
            else:
                phase.note(
                    f"{reference} was already trusted before this run, so cleanup leaves that "
                    "entry exactly as it found it"
                )
        after = self.read_trust(phase=phase)
        phase.add(trust_drift_findings(before.sections, after, added=self.added_trust))
        for reference in self.trusted:
            if set(trust_spellings(reference)).isdisjoint(after["formulae"]):
                phase.add(
                    (
                        f"`brew trust --formula {reference}` succeeded but `brew trust --json "
                        f"{TRUST_JSON_VERSION}` does not list {canonical_reference(reference)}; "
                        "expected Homebrew to record it so `brew install` does not fail with "
                        f'"{trust_refusal(reference)}"',
                    )
                )
        phase.note(TRUST_SCOPE_NOTICE)
        phase.note(TRUST_NAME_NOTICE)
        self.trust_evidence = {
            "argv": [list(evidence.argv) for evidence in phase.commands],
            "brew": self.observed_brew_version(),
            "store": before.to_json_object(),
            "trusted": [canonical_reference(reference) for reference in self.trusted],
            "added": [canonical_reference(reference) for reference in self.added_trust],
            "restore": TRUST_RESTORE_MECHANISM,
            "restored": None,
        }
        phase.settle()

    def rewrite_trust_file(self, snapshot: TrustStore) -> str:
        """Rewrite Homebrew's trust file with the exact bytes captured before trusting."""

        try:
            if snapshot.existed:
                snapshot.path.parent.mkdir(parents=True, exist_ok=True)
                snapshot.path.write_bytes(snapshot.content)
            else:
                snapshot.path.unlink(missing_ok=True)
            existed, content = read_trust_file(snapshot.path)
        except (OSError, HomebrewAcceptanceError) as error:
            self.cleanup_findings.append(
                f"cleanup could not restore the trust store {snapshot.path}: {error}; untrust "
                f"{[canonical_reference(name) for name in self.added_trust]} by hand with "
                "`brew untrust --formula <reference>`"
            )
            return "failed"
        if (existed, content) != (snapshot.existed, snapshot.content):
            self.cleanup_findings.append(
                f"the trust store {snapshot.path} still differs from the state observed before "
                f"trusting (existed={existed}, {len(content)} bytes); expected "
                f"existed={snapshot.existed} and {len(snapshot.content)} bytes"
            )
            return "failed"
        return "trust file rewrite"

    def restore_trust(self) -> None:
        """Leave Homebrew's trust store holding exactly what it held before this run."""

        snapshot = self.trust_snapshot
        if snapshot is None:
            return
        self.trust_snapshot = None
        for reference in reversed(self.added_trust):
            evidence = self.gateway.brew("untrust", "--formula", reference)
            self.cleanup_commands.append(evidence)
            if evidence.returncode != 0:
                self.cleanup_findings.append(
                    f"cleanup could not untrust {reference}: "
                    f"{bounded_text(evidence.stderr or evidence.stdout).strip()}"
                )
        try:
            observed: Mapping[str, Sequence[str]] | None = self.read_trust(phase=None)
        except HomebrewAcceptanceError as error:
            self.cleanup_findings.append(
                f"cleanup could not re-read the trust store after untrusting: {error}"
            )
            observed = None
        if observed is not None and trust_sections_match(snapshot.sections, observed):
            # `brew untrust` removes names, never the file, so a store Homebrew created for this
            # run still has to go even when its remaining names already match.
            try:
                existed, _ = read_trust_file(snapshot.path)
            except HomebrewAcceptanceError:
                existed = not snapshot.existed
            if existed == snapshot.existed:
                self.trust_evidence["restored"] = (
                    "brew untrust --formula" if self.added_trust else "nothing was trusted"
                )
                return
        self.trust_evidence["restored"] = self.rewrite_trust_file(snapshot)

    def require_untapped(self) -> None:
        """Require the disposable tap to be gone from `brew tap` after untapping."""

        listing = self.gateway.brew("tap")
        self.cleanup_commands.append(listing)
        if listing.returncode != 0:
            self.cleanup_findings.append(
                f"cleanup could not list taps after untapping {ACCEPTANCE_TAP}: "
                f"{bounded_text(listing.stderr or listing.stdout).strip()}"
            )
            return
        canonical = canonical_tap_name(ACCEPTANCE_TAP)
        surviving = sorted(
            set(parse_tap_list(listing.stdout)).intersection((ACCEPTANCE_TAP, canonical))
        )
        if surviving:
            self.cleanup_findings.append(
                f"`brew untap {ACCEPTANCE_TAP}` reported success but `brew tap` still lists "
                f"{surviving}; removing the tap directory does not deregister a tap, so remove "
                f"it with `brew untap {canonical}` before rerunning this harness"
            )

    def cleanup(self) -> None:
        """Undo every install, the disposable tap, and every trust entry it added."""

        for package_id in list(self.installed):
            evidence = self.uninstall(package_id)
            if evidence.returncode != 0:
                self.cleanup_findings.append(
                    f"cleanup could not uninstall {package_id}: "
                    f"{bounded_text(evidence.stderr or evidence.stdout).strip()}"
                )
        if self.tapped:
            evidence = self.gateway.brew("untap", ACCEPTANCE_TAP)
            self.cleanup_commands.append(evidence)
            if evidence.returncode != 0:
                self.cleanup_findings.append(
                    f"cleanup could not untap {ACCEPTANCE_TAP}: "
                    f"{bounded_text(evidence.stderr or evidence.stdout).strip()}"
                )
            else:
                self.tapped = False
                self.require_untapped()
        self.restore_trust()

    # Sentinels.

    def create_sentinels(self, work: Path) -> None:
        """Write unrelated sentinel files outside every keg and digest them."""

        prefix = self.require_prefix()
        candidates = [work / "unrelated-user-file.txt"]
        directories = [prefix / "share"]
        for shell in COMPLETION_SHELLS:
            for directory, _ in COMPLETION_LOCATIONS[shell]:
                directories.append(prefix / directory)
        marker = f"skillmount-acceptance-sentinel-{os.getpid()}"
        for directory in directories:
            if directory.is_dir():
                candidates.append(directory / marker)
        created: list[Path] = []
        for path in candidates:
            if path.exists():
                raise HomebrewAcceptanceError(
                    f"sentinel path {path} already exists; expected to create it exclusively"
                )
            try:
                path.write_bytes(SENTINEL_CONTENT)
            except OSError as error:
                print(f"sentinel skipped for {path}: {error}", file=sys.stderr)
                continue
            created.append(path)
        self.created_sentinels = tuple(created)
        self.sentinels = capture_digests(created)
        self.profiles = capture_digests(
            Path.home() / relative for relative in PROFILE_PATHS
        )

    def remove_sentinels(self) -> None:
        """Remove only the sentinel files this harness created."""

        for path in self.created_sentinels:
            try:
                path.unlink(missing_ok=True)
            except OSError as error:
                self.cleanup_findings.append(f"cleanup could not remove sentinel {path}: {error}")

    def observe_sentinels(self, phase: Phase) -> None:
        """Require every unrelated sentinel and profile file to be unchanged."""

        phase.add(sentinel_findings(self.sentinels, capture_digests(map(Path, self.sentinels))))
        phase.add(sentinel_findings(self.profiles, capture_digests(map(Path, self.profiles))))
        phase.note(
            f"compared {len(self.sentinels)} sentinel and {len(self.profiles)} profile paths"
        )

    # Phases.

    def phase_style(self) -> None:
        """Resolve each Formula through the owned tap and run `brew style`."""

        if not self.enabled("style"):
            return
        phase = self.phase("style")
        tap_root = self.tap_root
        if tap_root is None:
            raise HomebrewAcceptanceError("the disposable tap was never created")
        for package_id in self.options.formula_ids:
            reference = self.formula_reference(package_id)
            located = self.gateway.brew("formula", reference)
            if self.check(phase, located, expectation=f"{reference} must resolve through the tap"):
                resolved = parse_single_path(located.stdout, label=f"brew formula {reference}")
                owner = Path(os.path.realpath(tap_root))
                if owner not in Path(os.path.realpath(resolved)).parents:
                    phase.add(
                        (
                            f"{reference} resolved to {resolved}; expected a file inside the "
                            f"disposable tap {owner}",
                        )
                    )
                else:
                    phase.note(f"{reference} is owned by {resolved}")
            self.check(
                phase,
                self.gateway.brew("style", "--formula", reference),
                expectation="brew style must accept the rendered Formula",
            )
        phase.settle()

    def phase_audit(self, rendered: Mapping[str, Path]) -> None:
        """Audit each Formula and require the platform and pair invariants."""

        if not self.enabled("audit"):
            return
        phase = self.phase("audit")
        source = self.require_source()
        for package_id in self.options.formula_ids:
            reference = self.formula_reference(package_id)
            evidence = phase.record(self.gateway.brew("audit", "--strict", "--formula", reference))
            offences = audit_findings(
                f"{evidence.stdout}\n{evidence.stderr}", local_source=source.local
            )
            phase.add(offences)
            if evidence.returncode != 0 and not offences:
                phase.note(
                    f"brew audit exited {evidence.returncode} for {reference} with only "
                    "local-source offences, which the file:// rehearsal explains"
                )
            text = self.formula_paths[package_id].read_text(encoding="utf-8")
            phase.add(platform_findings(text, formula_class=package_id))
        self.inspect_rendered_pair(phase, rendered)
        phase.settle()

    def install(self, phase: Phase, package_id: str) -> bool:
        """Build and install one Formula from source, recording the attempt."""

        reference = self.formula_reference(package_id)
        if reference not in self.trusted:
            phase.add((untrusted_install_finding(reference),))
            return False
        evidence = self.gateway.brew(
            "install", "--formula", "--build-from-source", reference, timeout=BUILD_TIMEOUT
        )
        if not self.check(
            phase, evidence, expectation=f"{reference} must build and install from source"
        ):
            return False
        cellar = parse_single_path(
            self.require(self.gateway.brew("--cellar", reference)).stdout,
            label=f"brew --cellar {reference}",
        )
        self.installed[package_id] = cellar
        phase.note(f"installed {reference} into {cellar}")
        return True

    def keg_for(self, package_id: str, *, version: str) -> Path:
        """Return the single installed keg for one package id."""

        cellar = self.installed.get(package_id)
        if cellar is None:
            raise HomebrewAcceptanceError(f"{package_id} is not recorded as installed")
        return select_keg(cellar, version=version)

    def observe_selected_only(self, package_id: str, keg: Path, *, other_installed: bool) -> None:
        """Require exactly the selected executable in the keg and the prefix."""

        phase = self.phase("selected-only")
        prefix = self.require_prefix()
        command, other = self.selection(package_id)
        phase.add(keg_findings(keg, command=command, other_command=other))
        phase.add(
            prefix_findings(
                prefix,
                keg,
                command=command,
                other_command=other,
                other_installed=other_installed,
            )
        )
        evidence = phase.record(self.gateway.tool(str(prefix / "bin" / command), "--version"))
        if evidence.returncode != 0:
            phase.add(
                (
                    f"{command} --version exited {evidence.returncode}; expected 0: "
                    f"{bounded_text(evidence.stderr or evidence.stdout).strip()}",
                )
            )
        phase.add(
            version_findings(evidence.stdout, command=command, version=self.options.version)
        )
        help_evidence = phase.record(self.gateway.tool(str(prefix / "bin" / command), "--help"))
        if help_evidence.returncode != 0:
            phase.add(
                (
                    f"{command} --help exited {help_evidence.returncode}; expected 0: "
                    f"{bounded_text(help_evidence.stderr or help_evidence.stdout).strip()}",
                )
            )
        phase.note(f"{package_id} keg {keg} exposes only {command}")

    def owned_completions(self, package_id: str, keg: Path) -> dict[str, str | None]:
        """Return prefix completion digests owned by one installed Formula."""

        prefix = self.require_prefix()
        command, _ = self.selection(package_id)
        owned: dict[str, str | None] = {}
        keg_layout = completion_layout(keg, command=command)
        prefix_layout = completion_layout(prefix, command=command)
        for shell in COMPLETION_SHELLS:
            if not keg_layout[shell]:
                continue
            for path in prefix_layout[shell]:
                owned[str(path)] = digest_or_none(path)
        return owned

    def observe_completions(self, package_id: str, keg: Path) -> None:
        """Require Formula-owned completion files for exactly the selected command."""

        phase = self.phase("completions")
        prefix = self.require_prefix()
        command, other = self.selection(package_id)
        keg_layout = completion_layout(keg, command=command)
        phase.add(completion_layout_findings(keg_layout, command=command, label=f"keg {keg}"))
        other_layout = completion_layout(keg, command=other)
        stray = [str(path) for paths in other_layout.values() for path in paths]
        if stray:
            phase.add(
                (
                    f"keg {keg} owns completion files for the pair member {other!r}: {stray}; "
                    "expected none",
                )
            )
        phase.add(linked_completion_findings(prefix, keg, command=command))
        for shell in COMPLETION_SHELLS:
            files = keg_layout[shell]
            if len(files) != 1:
                continue
            path = files[0]
            text = path.read_text(encoding="utf-8", errors="replace")
            phase.add(
                completion_text_findings(shell, text, command=command, other_command=other)
            )
            phase.add(self.parse_with_shell(shell, path))
            phase.note(f"{package_id} owns {shell} completion {path}")
        phase.add(sentinel_findings(self.profiles, capture_digests(map(Path, self.profiles))))

    def parse_with_shell(self, shell: str, script: Path) -> tuple[str, ...]:
        """Parse one completion file with its real interpreter when available."""

        try:
            interpreter, _ = native_shell.require_interpreter(shell)
        except native_shell.AcceptanceError:
            return ()
        installation = native_shell.ShellInstallation(
            command=(interpreter,), environment=dict(os.environ), script=script
        )
        try:
            native_shell.syntax_check(shell, installation)
        except native_shell.AcceptanceError as error:
            return (f"{shell} could not parse {script}: {error}",)
        return ()

    def observe_brew_test(self, package_id: str) -> None:
        """Run the Formula's own `test do` block."""

        phase = self.phase("brew-test")
        reference = self.formula_reference(package_id)
        self.check(
            phase,
            self.gateway.brew("test", reference, timeout=DEFAULT_TIMEOUT),
            expectation="the Formula test block must pass",
        )
        phase.note(f"brew test ran for {reference}")

    def observe_uninstall(
        self, package_id: str, keg: Path, owned: Mapping[str, str | None]
    ) -> None:
        """Uninstall one Formula and require only its own files to disappear."""

        phase = self.phase("uninstall")
        prefix = self.require_prefix()
        command, _ = self.selection(package_id)
        evidence = self.uninstall(package_id, phase=phase)
        if evidence.returncode != 0:
            phase.add(
                (
                    f"brew uninstall {package_id} exited {evidence.returncode}; expected 0: "
                    f"{bounded_text(evidence.stderr or evidence.stdout).strip()}",
                )
            )
            return
        phase.add(uninstall_findings(prefix, keg, command=command, owned=owned))
        phase.add(sentinel_findings(self.sentinels, capture_digests(map(Path, self.sentinels))))
        phase.note(f"uninstalled {package_id} and kept every unrelated path")

    def stage_alone(self, package_id: str, phase_name: str) -> None:
        """Install one Formula alone, inspect it, and uninstall it again."""

        if not self.enabled(phase_name):
            return
        phase = self.phase(phase_name)
        if package_id not in self.options.formula_ids:
            phase.skip(f"{package_id} was not selected by --formula")
            return
        if not self.install(phase, package_id):
            phase.settle()
            return
        phase.settle()
        keg = self.installed[package_id] / self.options.version
        owned: dict[str, str | None] = {}
        try:
            keg = self.keg_for(package_id, version=self.options.version)
            owned = self.owned_completions(package_id, keg)
            if self.enabled("selected-only"):
                self.observe_selected_only(package_id, keg, other_installed=False)
            if self.enabled("completions"):
                self.observe_completions(package_id, keg)
            if self.enabled("brew-test"):
                self.observe_brew_test(package_id)
        finally:
            if self.enabled("uninstall"):
                self.observe_uninstall(package_id, keg, owned)
            else:
                self.uninstall(package_id)

    def stage_co_install(self) -> None:
        """Install both Formulae together and then remove one of them."""

        if not self.enabled("co-install"):
            return
        phase = self.phase("co-install")
        if len(self.options.formula_ids) != len(FORMULA_IDS):
            phase.skip(
                f"co-installation needs both of {list(FORMULA_IDS)}; "
                f"--formula selected {list(self.options.formula_ids)}"
            )
            return
        prefix = self.require_prefix()
        try:
            for package_id in FORMULA_IDS:
                if not self.install(phase, package_id):
                    phase.settle()
                    return
            kegs: dict[str, Path] = {}
            owned: dict[str, dict[str, str | None]] = {}
            for package_id in FORMULA_IDS:
                command, other = self.selection(package_id)
                keg = self.keg_for(package_id, version=self.options.version)
                kegs[package_id] = keg
                owned[package_id] = self.owned_completions(package_id, keg)
                phase.add(keg_findings(keg, command=command, other_command=other))
                phase.add(
                    prefix_findings(
                        prefix,
                        keg,
                        command=command,
                        other_command=other,
                        other_installed=True,
                    )
                )
                evidence = phase.record(
                    self.gateway.tool(str(prefix / "bin" / command), "--version")
                )
                phase.add(
                    version_findings(
                        evidence.stdout, command=command, version=self.options.version
                    )
                )
                layout = completion_layout(keg, command=command)
                phase.add(
                    completion_layout_findings(layout, command=command, label=f"keg {keg}")
                )
                phase.add(linked_completion_findings(prefix, keg, command=command))
                for shell in COMPLETION_SHELLS:
                    if len(layout[shell]) != 1:
                        continue
                    text = layout[shell][0].read_text(encoding="utf-8", errors="replace")
                    phase.add(
                        completion_text_findings(
                            shell, text, command=command, other_command=other
                        )
                    )
            if len(set(map(str, kegs.values()))) != len(FORMULA_IDS):
                phase.add(
                    (f"both Formulae resolved to the same keg: {sorted(map(str, kegs.values()))}",)
                )
            phase.settle()
            self.stage_cross_uninstall(kegs, owned)
        finally:
            for package_id in list(self.installed):
                self.uninstall(package_id)

    def stage_cross_uninstall(
        self, kegs: Mapping[str, Path], owned: Mapping[str, Mapping[str, str | None]]
    ) -> None:
        """Remove one co-installed Formula and require the other to survive."""

        if not self.enabled("cross-uninstall"):
            return
        phase = self.phase("cross-uninstall")
        prefix = self.require_prefix()
        removed, retained = FORMULA_IDS
        removed_command, _ = self.selection(removed)
        retained_command, _ = self.selection(retained)
        retained_completions = dict(owned[retained])
        evidence = self.uninstall(removed, phase=phase)
        if evidence.returncode != 0:
            phase.add(
                (
                    f"brew uninstall {removed} exited {evidence.returncode}; expected 0: "
                    f"{bounded_text(evidence.stderr or evidence.stdout).strip()}",
                )
            )
            phase.settle()
            return
        phase.add(
            uninstall_findings(
                prefix, kegs[removed], command=removed_command, owned=owned[removed]
            )
        )
        retained_keg = kegs[retained]
        phase.add(
            prefix_findings(
                prefix,
                retained_keg,
                command=retained_command,
                other_command=removed_command,
                other_installed=False,
            )
        )
        version_evidence = phase.record(
            self.gateway.tool(str(prefix / "bin" / retained_command), "--version")
        )
        phase.add(
            version_findings(
                version_evidence.stdout,
                command=retained_command,
                version=self.options.version,
            )
        )
        for name, digest in sorted(retained_completions.items()):
            current = digest_or_none(Path(name))
            if current != digest:
                phase.add(
                    (
                        f"retained completion file {name} digest is {current}; expected {digest} "
                        f"after uninstalling {removed}",
                    )
                )
        phase.add(sentinel_findings(self.sentinels, capture_digests(map(Path, self.sentinels))))
        phase.note(f"{retained_command} still reports {self.options.version} without {removed}")
        phase.settle()

    def stage_upgrade(self, work: Path) -> None:
        """Install the prior released source and upgrade it to the candidate."""

        if not self.enabled("upgrade-from-prior"):
            return
        phase = self.phase("upgrade-from-prior")
        package_id = self.options.formula_ids[0]
        prior_tag = self.options.prior_tag
        resolved = phase.record(self.gateway.git("rev-parse", f"{prior_tag}^{{commit}}"))
        if resolved.returncode != 0:
            decision = upgrade_decision(
                prior_tag=prior_tag,
                prior_version=None,
                candidate_version=self.options.version,
                prior_cli_source=None,
                require_upgrade=self.options.require_upgrade,
            )
            self.settle_upgrade(phase, decision)
            return
        prior_commit = release.validate_commit(resolved.stdout.strip())
        manifest = phase.record(self.gateway.git("show", f"{prior_commit}:Cargo.toml"))
        prior_version = (
            cargo_version_from_manifest(manifest.stdout) if manifest.returncode == 0 else None
        )
        sources = phase.record(self.gateway.git("show", f"{prior_commit}:src/cli.rs"))
        prior_cli = sources.stdout if sources.returncode == 0 else None
        decision = upgrade_decision(
            prior_tag=prior_tag,
            prior_version=prior_version,
            candidate_version=self.options.version,
            prior_cli_source=prior_cli,
            require_upgrade=self.options.require_upgrade,
        )
        if not decision.eligible or prior_version is None:
            self.settle_upgrade(phase, decision)
            return
        prior = self.archive_source(work, tag=prior_tag, commit=prior_commit)
        prior_directory = work / f"prior-{prior_version}"
        rendered = self.render_formulae(
            prior_directory, source=prior, version=prior_version, tag=prior_tag
        )
        candidate_formula = self.formula_paths[package_id]
        candidate_text = candidate_formula.read_text(encoding="utf-8")
        reference = self.formula_reference(package_id)
        try:
            shutil.copyfile(rendered[package_id], candidate_formula)
            if not self.install(phase, package_id):
                phase.settle()
                return
            command, _ = self.selection(package_id)
            prefix = self.require_prefix()
            before = phase.record(self.gateway.tool(str(prefix / "bin" / command), "--version"))
            phase.add(version_findings(before.stdout, command=command, version=prior_version))
            candidate_formula.write_text(candidate_text, encoding="utf-8")
            upgraded = self.gateway.brew(
                "upgrade", "--formula", reference, timeout=BUILD_TIMEOUT
            )
            if not self.check(
                phase, upgraded, expectation=f"{reference} must upgrade to {self.options.version}"
            ):
                phase.settle()
                return
            cellar = self.installed[package_id]
            keg = require_keg(cellar, version=self.options.version)
            after = phase.record(self.gateway.tool(str(prefix / "bin" / command), "--version"))
            phase.add(
                version_findings(after.stdout, command=command, version=self.options.version)
            )
            phase.add(
                prefix_findings(
                    prefix,
                    keg,
                    command=command,
                    other_command=self.selection(package_id)[1],
                    other_installed=False,
                )
            )
            phase.note(
                f"{command} upgraded from {prior_version} to {self.options.version} in {keg}"
            )
            phase.settle()
        finally:
            candidate_formula.write_text(candidate_text, encoding="utf-8")
            for installed in list(self.installed):
                self.uninstall(installed)

    def settle_upgrade(self, phase: Phase, decision: UpgradeDecision) -> None:
        """Record an upgrade rehearsal that could not run."""

        if decision.status == "failed":
            phase.add((decision.reason,))
            phase.settle()
            return
        phase.skip(decision.reason)

    def phase_sentinels(self) -> None:
        """Require every unrelated path to be unchanged after every uninstall."""

        if not self.enabled("sentinel-unchanged"):
            return
        phase = self.phase("sentinel-unchanged")
        self.observe_sentinels(phase)
        phase.settle()

    # Orchestration.

    def lifecycle(self, work: Path) -> None:
        """Render, tap, and run every selected phase in canonical order."""

        source = self.build_source(work)
        self.source = source
        rendered = self.render_formulae(
            work / "candidate",
            source=source,
            version=self.options.version,
            tag=self.options.tag,
        )
        self.create_tap()
        self.place_formulae(rendered)
        self.phase_style()
        self.phase_audit(rendered)
        self.phase_trust()
        self.stage_alone(FORMULA_IDS[0], "install-skillmount-alone")
        self.stage_alone(FORMULA_IDS[1], "install-asm-alone")
        self.stage_co_install()
        self.stage_upgrade(work)
        for name in ("selected-only", "completions", "brew-test", "uninstall"):
            phase = self.phases[name]
            if not self.enabled(name):
                continue
            if phase.commands or phase.observations or phase.findings:
                phase.settle()
            else:
                phase.skip("no single-Formula install stage produced an observation")

    def execute(self) -> dict[str, object]:
        """Run the harness end to end and return its report document."""

        environment = self.probe_environment()
        self.prefix = self.require_safe_state()
        self.commit, worktree = self.resolve_identity()
        environment["worktree"] = worktree
        environment["prefix"] = str(self.prefix)
        with tempfile.TemporaryDirectory(prefix="skillmount-homebrew-acceptance-") as raw:
            work = Path(raw)
            self.create_sentinels(work)
            try:
                try:
                    self.lifecycle(work)
                finally:
                    self.cleanup()
                self.phase_sentinels()
            except HomebrewAcceptanceError as error:
                self.record_failure(error)
            finally:
                self.remove_sentinels()
        return build_report(
            options=self.options,
            commit=self.commit,
            source=self.source,
            prefix=self.prefix,
            environment=environment,
            trust=self.trust_evidence,
            phases=[self.phases[name] for name in PHASE_ORDER],
            cleanup=self.cleanup_commands,
            cleanup_findings=self.cleanup_findings,
        )

    def record_failure(self, error: HomebrewAcceptanceError) -> None:
        """Attribute one aborting failure to the active or first pending phase."""

        target = self.active
        if target is None or target.status == "skipped":
            target = next(
                (
                    self.phases[name]
                    for name in PHASE_ORDER
                    if self.phases[name].status == "pending"
                ),
                None,
            )
        if target is None:
            self.cleanup_findings.append(f"harness aborted: {error}")
            return
        target.add((f"harness aborted: {error}",))
        target.status = "failed"


def preflight_identity(path: Path) -> tuple[str, str, str, SourceArchive]:
    """Return the version, tag, commit, and source identity a preflight artifact pins."""

    channels = import_package_channels()
    try:
        inputs = channels.PackageInputs.from_json(path.read_text(encoding="utf-8"))
    except channels.ChannelError as error:
        raise HomebrewAcceptanceError(
            f"{path} is not a valid preflight artifact: {error}"
        ) from error
    source = SourceArchive(
        url=inputs.source_url,
        sha256=validate_digest(inputs.source_sha256, label=f"{path} source_sha256"),
        path=None,
    )
    return inputs.version, inputs.tag, inputs.commit, source


def resolve_options(options: argparse.Namespace) -> HarnessOptions:
    """Validate command-line inputs into one immutable option set."""

    repository = options.repository.resolve()
    commit = release.validate_commit(options.commit) if options.commit else None
    if options.inputs is not None:
        conflicting = sorted(
            name
            for name, value in (
                ("--version", options.version),
                ("--tag", options.tag),
                ("--source-url-override", options.source_url_override),
                ("--source-sha256", options.source_sha256),
            )
            if value is not None
        )
        if conflicting:
            raise HomebrewAcceptanceError(
                f"--inputs was passed with {conflicting}; expected --inputs to be the only "
                "source of the release identity"
            )
        version, tag, inputs_commit, source = preflight_identity(options.inputs)
        commit = commit or inputs_commit
    else:
        manifest = repository / "Cargo.toml"
        if not manifest.is_file():
            raise HomebrewAcceptanceError(f"{manifest} is missing; expected a Cargo manifest")
        version = options.version or cargo_version_from_manifest(
            manifest.read_text(encoding="utf-8")
        )
        release.validate_stable_version(version)
        tag = options.tag or f"v{version}"
        source = source_override(options.source_url_override, options.source_sha256)
    if release.stable_version_from_tag(tag) != version:
        raise HomebrewAcceptanceError(f"tag {tag!r} does not describe version {version!r}")
    template_directory = (
        options.template_directory.resolve()
        if options.template_directory is not None
        else repository / "packaging" / "homebrew"
    )
    if not template_directory.is_dir():
        raise HomebrewAcceptanceError(
            f"{template_directory} is missing; expected the tracked Homebrew templates"
        )
    return HarnessOptions(
        repository=repository,
        template_directory=template_directory,
        formula_ids=select_formulae(options.formula),
        phases=expand_phases(options.phase),
        version=version,
        tag=tag,
        commit=commit,
        source=source,
        require_upgrade=options.require_upgrade,
        prior_tag=options.prior_tag,
    )


def argument_parser() -> argparse.ArgumentParser:
    """Build the Homebrew acceptance command-line parser."""

    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--repository", type=Path, default=Path.cwd())
    parser.add_argument("--template-directory", type=Path, default=None)
    parser.add_argument("--formula", action="append", choices=list(FORMULA_IDS))
    parser.add_argument("--phase", action="append", choices=list(PHASE_ORDER))
    parser.add_argument("--inputs", type=Path, default=None)
    parser.add_argument("--commit", default=None)
    parser.add_argument("--version", default=None)
    parser.add_argument("--tag", default=None)
    parser.add_argument("--prior-tag", default=PRIOR_TAG)
    parser.add_argument("--source-url-override", default=None)
    parser.add_argument("--source-sha256", default=None)
    parser.add_argument("--require-upgrade", action="store_true")
    parser.add_argument("--report", type=Path, default=None)
    parser.add_argument("--print-coverage", action="store_true")
    return parser


def run(
    arguments: Sequence[str],
    *,
    environment: Mapping[str, str] | None = None,
    gateway: CommandGateway | None = None,
) -> int:
    """Refuse unsafe runs, execute the lifecycle, and write the report."""

    options = argument_parser().parse_args(arguments)
    if options.print_coverage:
        print(coverage_text())
        return 0
    require_enabled(os.environ if environment is None else environment)
    resolved = resolve_options(options)
    harness = Harness(
        SubprocessGateway(resolved.repository) if gateway is None else gateway, resolved
    )
    document = harness.execute()
    if options.report is not None:
        options.report.parent.mkdir(parents=True, exist_ok=True)
        options.report.write_text(report_text(document), encoding="utf-8")
        print(f"Wrote {options.report}")
    print(summary_text(document))
    return 0 if document["status"] in ("passed", "skipped") else 1


def main(
    arguments: Sequence[str] | None = None,
    *,
    environment: Mapping[str, str] | None = None,
    gateway: CommandGateway | None = None,
) -> int:
    """Convert unproved Homebrew observations into a stable nonzero status."""

    try:
        return run(
            sys.argv[1:] if arguments is None else arguments,
            environment=environment,
            gateway=gateway,
        )
    except (
        OSError,
        HomebrewAcceptanceError,
        native_shell.AcceptanceError,
        release.ReleaseError,
        UnicodeError,
        json.JSONDecodeError,
    ) as error:
        print(f"homebrew acceptance failed: {error}", file=sys.stderr)
        return 1


if __name__ == "__main__":
    raise SystemExit(main())
