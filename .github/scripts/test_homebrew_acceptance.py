#!/usr/bin/env python3
"""Fixture tests for the native Homebrew Formula lifecycle harness."""

from __future__ import annotations

import contextlib
import io
import json
import os
import shutil
import stat
import sys
import tempfile
import types
import unittest
from dataclasses import replace
from pathlib import Path
from typing import Sequence

import homebrew_acceptance as harness
import release

VERSION = "0.2.0"
TAG = "v0.2.0"
COMMIT = "b" * 40
DIGEST = "c" * 64
BASH_TEMPLATE = """_{command}() {{
    COMPREPLY=()
}}

_{command}_before_passthrough() {{
    _{command} "$@"
}}

if [[ "${{BASH_VERSINFO[0]}}" -ge 4 ]]; then
    complete -F _{command}_before_passthrough -o nosort {command}
fi
"""
ZSH_TEMPLATE = """#compdef {command}

_{command}_complete() {{
    local context state
}}

if [ "$funcstack[1]" = "_{command}" ]; then
    _{command} "$@"
else
    compdef _{command} {command}
fi
"""
FISH_TEMPLATE = """function _{command}_before_passthrough
    not contains -- -- (commandline -opc)
end

complete -c {command} -n '_{command}_before_passthrough' -l help -d 'Print help'
complete -c {command} -n 'not _{command}_before_passthrough' -f
"""
COMPLETION_TEMPLATES = {
    "bash": BASH_TEMPLATE,
    "zsh": ZSH_TEMPLATE,
    "fish": FISH_TEMPLATE,
}
FORMULA_TEMPLATE = """# Generated for @TAG@ at @COMMIT@.
class Skillmount < Formula
  desc "SkillMount skill mounter"
  homepage "https://github.com/pashifika/skillmount"
  url "file:///tmp/skillmount-v0.2.0-aarch64-apple-darwin.tar.gz"
  sha256 "{digest}"
  license any_of: ["MIT", "Apache-2.0"]

  depends_on arch: :arm64
  depends_on :macos

  def install
    bin.install "skillmount"
    pkgshare.install "LICENSE-APACHE", "LICENSE-MIT", "VERSION"
    generate_completions_from_executable(bin/"skillmount", "completions", base_name: "skillmount")
  end

  test do
    assert_match "0.2.0", shell_output("#{{bin}}/skillmount --version")
    assert_match "Usage:", shell_output("#{{bin}}/skillmount --help")

    refute_path_exists bin/"asm"

    [
      [bash_completion/"skillmount", "bash"],
      [zsh_completion/"_skillmount", "zsh"],
      [fish_completion/"skillmount.fish", "fish"],
    ].each do |completion, shell|
      assert_path_exists completion, "no #{{shell}} completion was generated"
      contents = completion.read
      assert_match "skillmount", contents
      refute_match(/\\basm\\b/, contents)
    end
  end
end
"""
TRUST_STORE = {
    "taps": ["beeftornado/rmtree"],
    "formulae": ["hashicorp/tap/terraform"],
    "casks": [],
    "commands": [],
}


def trust_json(**sections: list[str]) -> str:
    """Return one `brew trust --json v1` document with the named sections replaced."""

    return json.dumps({**TRUST_STORE, **sections})


def acceptance_reference(package_id: str) -> str:
    """Return the tap-qualified reference the harness passes to every `brew` call."""

    return f"{harness.ACCEPTANCE_TAP}/{package_id}"


def trusted_name(package_id: str) -> str:
    """Return the canonical name Homebrew records for one acceptance-tap Formula."""

    return harness.canonical_reference(acceptance_reference(package_id))


class ScriptedGateway:
    """Fake command boundary answering only the argv prefixes a test scripts."""

    def __init__(
        self,
        responses: dict[tuple[str, ...], tuple[int, str, str]],
        *,
        environment: dict[str, str] | None = None,
    ) -> None:
        """Bind one response table and start with an empty call log."""

        self.responses = responses
        self.environment = {} if environment is None else environment
        self.calls: list[tuple[str, ...]] = []

    def answer(self, argv: tuple[str, ...]) -> harness.CommandEvidence:
        """Return the scripted evidence for one argv, failing on a surprise."""

        self.calls.append(argv)
        for prefix, (status, stdout, stderr) in self.responses.items():
            if argv[: len(prefix)] == prefix:
                return harness.CommandEvidence(
                    argv=argv, returncode=status, stdout=stdout, stderr=stderr
                )
        raise AssertionError(f"harness ran an unscripted command {argv}")

    def brew(
        self, *arguments: str, timeout: int = harness.DEFAULT_TIMEOUT
    ) -> harness.CommandEvidence:
        """Answer one scripted `brew` call."""

        del timeout
        return self.answer(("brew", *arguments))

    def git(
        self, *arguments: str, timeout: int = harness.DEFAULT_TIMEOUT
    ) -> harness.CommandEvidence:
        """Answer one scripted `git` call."""

        del timeout
        return self.answer(("git", *arguments))

    def tool(
        self, executable: str, *arguments: str, timeout: int = harness.DEFAULT_TIMEOUT
    ) -> harness.CommandEvidence:
        """Answer one scripted auxiliary call."""

        del timeout
        return self.answer((executable, *arguments))


class ForbiddenGateway:
    """Gateway proving a refusal happened before the machine was touched."""

    def __init__(self) -> None:
        """Start with an empty attempt log."""

        self.environment: dict[str, str] = {}
        self.calls: list[tuple[str, ...]] = []

    def record(self, argv: tuple[str, ...]) -> harness.CommandEvidence:
        """Record and reject one forbidden attempt."""

        self.calls.append(argv)
        raise AssertionError(f"harness ran {argv} before proving it was safe to do so")

    def brew(
        self, *arguments: str, timeout: int = harness.DEFAULT_TIMEOUT
    ) -> harness.CommandEvidence:
        """Reject any `brew` call."""

        del timeout
        return self.record(("brew", *arguments))

    def git(
        self, *arguments: str, timeout: int = harness.DEFAULT_TIMEOUT
    ) -> harness.CommandEvidence:
        """Reject any `git` call."""

        del timeout
        return self.record(("git", *arguments))

    def tool(
        self, executable: str, *arguments: str, timeout: int = harness.DEFAULT_TIMEOUT
    ) -> harness.CommandEvidence:
        """Reject any auxiliary call."""

        del timeout
        return self.record((executable, *arguments))


def options_for(
    repository: Path,
    *,
    formula_ids: Sequence[str] = harness.FORMULA_IDS,
    phases: Sequence[str] = harness.PHASE_ORDER,
    require_upgrade: bool = False,
) -> harness.HarnessOptions:
    """Return option values that need no packaging tree on disk."""

    return harness.HarnessOptions(
        repository=repository,
        template_directory=repository / "packaging" / "homebrew",
        formula_ids=tuple(formula_ids),
        phases=tuple(phases),
        version=VERSION,
        tag=TAG,
        commit=COMMIT,
        archive=harness.ReleaseArchive(
            url="file:///tmp/skillmount-v0.2.0-aarch64-apple-darwin.tar.gz",
            sha256=DIGEST,
            path=None,
        ),
        require_upgrade=require_upgrade,
        prior_tag=harness.PRIOR_TAG,
    )


class FixtureCase(unittest.TestCase):
    """Shared on-disk fixture builders for keg, prefix, and completion trees."""

    def setUp(self) -> None:
        """Create one owned temporary tree removed after the test."""

        self.root = Path(tempfile.mkdtemp(prefix="skillmount-homebrew-test-"))
        self.addCleanup(shutil.rmtree, self.root, ignore_errors=True)

    def write_executable(self, path: Path) -> Path:
        """Write one executable regular file."""

        path.parent.mkdir(parents=True, exist_ok=True)
        path.write_text("#!/bin/sh\nexit 0\n", encoding="utf-8")
        path.chmod(path.stat().st_mode | stat.S_IXUSR | stat.S_IXGRP | stat.S_IXOTH)
        return path

    def completion_text(self, shell: str, command: str) -> str:
        """Return a realistic generated completion script for one command."""

        return COMPLETION_TEMPLATES[shell].format(command=command)

    def write_completions(self, root: Path, command: str, *, shells: Sequence[str] = ()) -> None:
        """Write the canonical Homebrew-owned completion file per shell."""

        for shell in shells or harness.COMPLETION_SHELLS:
            directory, template = harness.COMPLETION_LOCATIONS[shell][0]
            path = root / directory / template.format(command=command)
            path.parent.mkdir(parents=True, exist_ok=True)
            path.write_text(self.completion_text(shell, command), encoding="utf-8")

    def build_keg(
        self,
        package_id: str,
        command: str,
        *,
        extra_executables: Sequence[str] = (),
        version: str = VERSION,
        completions: bool = True,
    ) -> Path:
        """Build one Cellar keg tree and return its version directory."""

        keg = self.root / "Cellar" / package_id / version
        self.write_executable(keg / "bin" / command)
        for extra in extra_executables:
            self.write_executable(keg / "bin" / extra)
        if completions:
            self.write_completions(keg, command)
        return keg

    def link_prefix(self, keg: Path, command: str, *, prefix: Path | None = None) -> Path:
        """Link one keg executable and copy its completion files into a prefix."""

        prefix = prefix if prefix is not None else self.root / "prefix"
        (prefix / "bin").mkdir(parents=True, exist_ok=True)
        (prefix / "bin" / command).symlink_to(keg / "bin" / command)
        for shell in harness.COMPLETION_SHELLS:
            for path in harness.completion_layout(keg, command=command)[shell]:
                relative = path.relative_to(keg)
                destination = prefix / relative
                destination.parent.mkdir(parents=True, exist_ok=True)
                shutil.copyfile(path, destination)
        return prefix


class ArchivePreparationTests(FixtureCase):
    """Candidate archive construction and exact-commit safeguards."""

    def test_build_archive_packages_the_release_binary_pair(self) -> None:
        """Build both bins once and package only the deterministic release members."""

        target = release.target_for("aarch64-apple-darwin")
        work = self.root / "work"
        binary_directory = work / "cargo-target" / target.triple / "release"
        for executable in release.executable_names(target):
            self.write_executable(binary_directory / executable)
        for license_name in release.LICENSE_FILES:
            (self.root / license_name).write_text(f"{license_name}\n", encoding="utf-8")
        gateway = ScriptedGateway({("cargo", "build"): (0, "", "")})
        options = replace(options_for(self.root), archive=None)
        subject = harness.Harness(gateway, options)
        subject.commit = COMMIT

        archive = subject.build_archive(work)

        self.assertEqual(
            gateway.calls,
            [
                (
                    "cargo",
                    "build",
                    "--locked",
                    "--release",
                    "--target",
                    target.triple,
                    "--target-dir",
                    str(work / "cargo-target"),
                    "--bins",
                )
            ],
        )
        expected_path = (work / "release" / release.asset_name(TAG, target)).resolve()
        self.assertEqual(archive.path, expected_path)
        self.assertEqual(archive.url, archive.path.as_uri())
        self.assertEqual(archive.sha256, release.sha256_file(archive.path))
        release.inspect_archive(
            archive.path,
            target=target,
            version=VERSION,
            tag=TAG,
            commit=COMMIT,
        )
        self.assertEqual(
            subject.environment_evidence["archive_commands"][0]["argv"],
            list(gateway.calls[0]),
        )

    def test_local_build_requires_the_clean_checked_out_commit(self) -> None:
        """Refuse to label current-worktree bytes as another or dirty commit."""

        other = "d" * 40
        for head, dirty, expected in (
            (COMMIT, " M src/lib.rs\n", "clean worktree"),
            (other, "", "would build HEAD"),
        ):
            with self.subTest(head=head, dirty=bool(dirty)):
                gateway = ScriptedGateway(
                    {
                        ("git", "rev-parse", "HEAD"): (0, f"{head}\n", ""),
                        ("git", "cat-file", "-e"): (0, "", ""),
                        ("git", "status", "--porcelain"): (0, dirty, ""),
                    }
                )
                options = replace(options_for(self.root), archive=None)
                subject = harness.Harness(gateway, options)
                with self.assertRaisesRegex(harness.HomebrewAcceptanceError, expected):
                    subject.resolve_identity()

    def test_checked_archive_may_be_observed_from_a_dirty_checkout(self) -> None:
        """Use preflight archive identity without pretending the checkout built it."""

        gateway = ScriptedGateway(
            {
                ("git", "rev-parse", "HEAD"): (0, f"{COMMIT}\n", ""),
                ("git", "cat-file", "-e"): (0, "", ""),
                ("git", "status", "--porcelain"): (0, " M docs/packaging.md\n", ""),
            }
        )
        subject = harness.Harness(gateway, options_for(self.root))
        self.assertEqual(subject.resolve_identity(), (COMMIT, "dirty"))


class SafetyRefusalTests(FixtureCase):
    """The three refusals that keep a developer machine untouched."""

    def test_missing_opt_in_refuses_before_any_command(self) -> None:
        """Refuse and touch nothing when the opt-in variable is absent."""

        gateway = ForbiddenGateway()
        stderr = io.StringIO()
        with contextlib.redirect_stderr(stderr):
            status = harness.main(
                ["--report", str(self.root / "report.json")], environment={}, gateway=gateway
            )
        self.assertEqual(status, 1)
        self.assertEqual(gateway.calls, [])
        self.assertIn(harness.ENABLE_VARIABLE, stderr.getvalue())
        self.assertFalse((self.root / "report.json").exists())

    def test_wrong_opt_in_value_refuses(self) -> None:
        """Refuse when the opt-in variable is set to any other value."""

        for observed in ("", "0", "true", "yes"):
            with self.subTest(observed=observed):
                with self.assertRaises(harness.HomebrewAcceptanceError) as caught:
                    harness.require_enabled({harness.ENABLE_VARIABLE: observed})
                self.assertIn(repr(observed), str(caught.exception))
        harness.require_enabled({harness.ENABLE_VARIABLE: harness.ENABLE_VALUE})

    def test_unsupported_prefix_refuses_before_listing_formulae(self) -> None:
        """Refuse an Intel or Linuxbrew prefix after exactly one probe."""

        gateway = ScriptedGateway({("brew", "--prefix"): (0, "/usr/local\n", "")})
        subject = harness.Harness(gateway, options_for(self.root))
        with self.assertRaises(harness.HomebrewAcceptanceError) as caught:
            subject.require_safe_state()
        message = str(caught.exception)
        self.assertIn("/usr/local", message)
        self.assertIn(harness.SUPPORTED_PREFIX, message)
        self.assertEqual(gateway.calls, [("brew", "--prefix")])

    def test_supported_prefix_is_accepted(self) -> None:
        """Accept the Apple Silicon prefix and paths under it."""

        self.assertEqual(
            harness.require_supported_prefix("/opt/homebrew\n"), Path("/opt/homebrew")
        )
        self.assertEqual(
            harness.require_supported_prefix("/opt/homebrew/Cellar\n"),
            Path("/opt/homebrew/Cellar"),
        )
        for malformed in ("", "\n", "opt/homebrew\n", "/opt/homebrew\n/opt/homebrew\n"):
            with self.subTest(malformed=malformed):
                with self.assertRaises(harness.HomebrewAcceptanceError):
                    harness.require_supported_prefix(malformed)

    def test_installed_product_formula_refuses(self) -> None:
        """Refuse when either product Formula is already installed."""

        for listing in (
            "jq\nskillmount\nripgrep\n",
            "skillmount-asm\n",
            "pashifika/tap/skillmount-asm\njq\n",
        ):
            with self.subTest(listing=listing):
                with self.assertRaises(harness.HomebrewAcceptanceError) as caught:
                    harness.require_clean_formula_state(listing)
                self.assertIn("skillmount", str(caught.exception))
        harness.require_clean_formula_state("jq\nripgrep\nskillmount-tools\n")

    def test_occupied_formula_name_refuses_through_the_gateway(self) -> None:
        """Refuse an occupied Formula name without attempting an install."""

        gateway = ScriptedGateway(
            {
                ("brew", "--prefix"): (0, "/opt/homebrew\n", ""),
                ("brew", "list", "--formula"): (0, "jq\nskillmount\n", ""),
            }
        )
        subject = harness.Harness(gateway, options_for(self.root))
        with self.assertRaises(harness.HomebrewAcceptanceError) as caught:
            subject.require_safe_state()
        self.assertIn("skillmount", str(caught.exception))
        self.assertEqual(
            gateway.calls, [("brew", "--prefix"), ("brew", "list", "--formula")]
        )

    def test_failed_listing_is_not_treated_as_clean(self) -> None:
        """Fail closed when Homebrew cannot report installed formulae."""

        gateway = ScriptedGateway(
            {
                ("brew", "--prefix"): (0, "/opt/homebrew\n", ""),
                ("brew", "list", "--formula"): (1, "", "brew: command failed\n"),
            }
        )
        subject = harness.Harness(gateway, options_for(self.root))
        with self.assertRaises(harness.HomebrewAcceptanceError) as caught:
            subject.require_safe_state()
        self.assertIn("brew list --formula", str(caught.exception))

    def test_coverage_flag_touches_nothing(self) -> None:
        """Print the scenario mapping without opting in or running a command."""

        gateway = ForbiddenGateway()
        stdout = io.StringIO()
        with contextlib.redirect_stdout(stdout):
            status = harness.run(["--print-coverage"], environment={}, gateway=gateway)
        self.assertEqual(status, 0)
        self.assertEqual(gateway.calls, [])
        self.assertIn("Complete paired lifecycle succeeds", stdout.getvalue())


class KegInspectionTests(FixtureCase):
    """Keg and prefix classification against real directory trees."""

    def test_cellar_output_parsing(self) -> None:
        """Parse exactly one absolute path from `brew --cellar`."""

        self.assertEqual(
            harness.parse_single_path("/opt/homebrew/Cellar/skillmount\n", label="brew --cellar"),
            Path("/opt/homebrew/Cellar/skillmount"),
        )
        self.assertEqual(
            harness.parse_single_path(
                "\n  /opt/homebrew/Cellar/skillmount-asm  \n\n", label="brew --cellar"
            ),
            Path("/opt/homebrew/Cellar/skillmount-asm"),
        )
        for malformed in ("", "Cellar/skillmount\n", "/a\n/b\n"):
            with self.subTest(malformed=malformed):
                with self.assertRaises(harness.HomebrewAcceptanceError) as caught:
                    harness.parse_single_path(malformed, label="brew --cellar")
                self.assertIn("brew --cellar", str(caught.exception))

    def test_single_keg_selection(self) -> None:
        """Require exactly one installed keg for the expected version."""

        keg = self.build_keg("skillmount", "skillmount")
        cellar = keg.parent
        self.assertEqual(harness.select_keg(cellar, version=VERSION), keg)
        (cellar / "0.1.0").mkdir()
        with self.assertRaises(harness.HomebrewAcceptanceError) as caught:
            harness.select_keg(cellar, version=VERSION)
        message = str(caught.exception)
        self.assertIn("0.1.0", message)
        self.assertIn(VERSION, message)
        self.assertEqual(harness.keg_versions(cellar), ("0.1.0", VERSION))
        self.assertEqual(harness.require_keg(cellar, version="0.1.0"), cellar / "0.1.0")
        with self.assertRaises(harness.HomebrewAcceptanceError):
            harness.require_keg(cellar, version="9.9.9")

    def test_executable_listing_ignores_data_files(self) -> None:
        """List only executable regular files directly in a directory."""

        directory = self.root / "bin"
        self.write_executable(directory / "skillmount")
        (directory / "notes.txt").write_text("data\n", encoding="utf-8")
        (directory / "nested").mkdir()
        self.assertEqual(harness.executable_names_in(directory), ("skillmount",))
        self.assertEqual(harness.executable_names_in(self.root / "absent"), ())

    def test_keg_with_only_the_selected_executable_passes(self) -> None:
        """Accept a keg holding exactly its selected command."""

        for package_id, command, other in (
            ("skillmount", "skillmount", "asm"),
            ("skillmount-asm", "asm", "skillmount"),
        ):
            with self.subTest(package_id=package_id):
                keg = self.build_keg(package_id, command)
                self.assertEqual(
                    harness.keg_findings(keg, command=command, other_command=other), ()
                )

    def test_keg_holding_both_executables_fails(self) -> None:
        """Reject a keg that installed the pair member as well."""

        keg = self.build_keg("skillmount", "skillmount", extra_executables=("asm",))
        findings = harness.keg_findings(keg, command="skillmount", other_command="asm")
        self.assertTrue(findings)
        joined = " ".join(findings)
        self.assertIn("asm", joined)
        self.assertIn("skillmount", joined)
        self.assertIn(str(keg), joined)

    def test_keg_missing_the_selected_executable_fails(self) -> None:
        """Reject a keg whose selected command never landed."""

        keg = self.build_keg("skillmount-asm", "asm")
        (keg / "bin" / "asm").unlink()
        findings = harness.keg_findings(keg, command="asm", other_command="skillmount")
        self.assertTrue(findings)
        self.assertIn("expected exactly ['asm']", " ".join(findings))

    def test_absent_keg_fails(self) -> None:
        """Reject a keg directory that does not exist."""

        findings = harness.keg_findings(
            self.root / "missing", command="asm", other_command="skillmount"
        )
        self.assertEqual(len(findings), 1)
        self.assertIn("is not a directory", findings[0])

    def test_prefix_exposes_only_installed_commands(self) -> None:
        """Require the prefix link to resolve into the owning keg."""

        keg = self.build_keg("skillmount", "skillmount")
        prefix = self.link_prefix(keg, "skillmount")
        self.assertEqual(
            harness.prefix_findings(
                prefix,
                keg,
                command="skillmount",
                other_command="asm",
                other_installed=False,
            ),
            (),
        )
        findings = harness.prefix_findings(
            prefix, keg, command="skillmount", other_command="asm", other_installed=True
        )
        self.assertTrue(findings)
        self.assertIn("expected present", " ".join(findings))

    def test_prefix_link_outside_the_keg_fails(self) -> None:
        """Reject a product link that resolves outside its owning keg."""

        keg = self.build_keg("skillmount-asm", "asm")
        stray = self.write_executable(self.root / "elsewhere" / "asm")
        prefix = self.root / "prefix"
        (prefix / "bin").mkdir(parents=True)
        (prefix / "bin" / "asm").symlink_to(stray)
        findings = harness.prefix_findings(
            prefix, keg, command="asm", other_command="skillmount", other_installed=False
        )
        self.assertTrue(findings)
        self.assertIn(str(stray), " ".join(findings))

    def test_prefix_missing_link_fails(self) -> None:
        """Reject an install whose command never reached the prefix."""

        keg = self.build_keg("skillmount", "skillmount")
        prefix = self.root / "prefix"
        (prefix / "bin").mkdir(parents=True)
        findings = harness.prefix_findings(
            prefix, keg, command="skillmount", other_command="asm", other_installed=False
        )
        self.assertTrue(findings)
        self.assertIn("is absent", " ".join(findings))

    def test_unrelated_pair_member_in_prefix_fails(self) -> None:
        """Reject a prefix exposing the pair member when it is not installed."""

        keg = self.build_keg("skillmount", "skillmount")
        prefix = self.link_prefix(keg, "skillmount")
        self.write_executable(prefix / "bin" / "asm")
        findings = harness.prefix_findings(
            prefix, keg, command="skillmount", other_command="asm", other_installed=False
        )
        self.assertTrue(findings)
        self.assertIn("expected absent", " ".join(findings))


class CompletionClassificationTests(FixtureCase):
    """Completion ownership and registration classification."""

    def test_layout_requires_exactly_one_file_per_shell(self) -> None:
        """Accept one Formula-owned file per shell and reject any other count."""

        keg = self.build_keg("skillmount", "skillmount")
        layout = harness.completion_layout(keg, command="skillmount")
        self.assertEqual(
            harness.completion_layout_findings(layout, command="skillmount", label=f"keg {keg}"),
            (),
        )
        duplicate = keg / "share" / "bash-completion" / "completions" / "skillmount"
        duplicate.parent.mkdir(parents=True)
        duplicate.write_text(self.completion_text("bash", "skillmount"), encoding="utf-8")
        findings = harness.completion_layout_findings(
            harness.completion_layout(keg, command="skillmount"),
            command="skillmount",
            label=f"keg {keg}",
        )
        self.assertEqual(len(findings), 1)
        self.assertIn("holds 2 bash completion files", findings[0])

    def test_layout_reports_a_missing_shell(self) -> None:
        """Report a shell whose completion file was never generated."""

        keg = self.build_keg("skillmount-asm", "asm", completions=False)
        self.write_completions(keg, "asm", shells=("bash", "zsh"))
        findings = harness.completion_layout_findings(
            harness.completion_layout(keg, command="asm"), command="asm", label=f"keg {keg}"
        )
        self.assertEqual(len(findings), 1)
        self.assertIn("0 fish completion files", findings[0])

    def test_text_naming_the_selected_command_passes(self) -> None:
        """Accept generated text that registers only the selected command."""

        for command, other in (("skillmount", "asm"), ("asm", "skillmount")):
            for shell in harness.COMPLETION_SHELLS:
                with self.subTest(command=command, shell=shell):
                    self.assertEqual(
                        harness.completion_text_findings(
                            shell,
                            self.completion_text(shell, command),
                            command=command,
                            other_command=other,
                        ),
                        (),
                    )

    def test_text_naming_the_wrong_command_fails(self) -> None:
        """Reject a file that registers the pair member instead."""

        for shell in harness.COMPLETION_SHELLS:
            with self.subTest(shell=shell):
                findings = harness.completion_text_findings(
                    shell,
                    self.completion_text(shell, "asm"),
                    command="skillmount",
                    other_command="asm",
                )
                joined = " ".join(findings)
                self.assertIn("lacks its registration", joined)
                self.assertIn("registers the pair member", joined)
                self.assertIn("mentions the pair member", joined)

    def test_shared_command_model_placeholder_fails(self) -> None:
        """Reject a file generated from a shared private command model."""

        text = self.completion_text("zsh", "skillmount").replace(
            "#compdef skillmount", "#compdef skillmount\n# usage: <asm|skillmount> completions"
        )
        findings = harness.completion_text_findings(
            "zsh", text, command="skillmount", other_command="asm"
        )
        self.assertIn(harness.SHARED_PLACEHOLDER, " ".join(findings))

    def test_empty_text_fails(self) -> None:
        """Reject an empty completion file."""

        findings = harness.completion_text_findings(
            "fish", "\n \n", command="asm", other_command="skillmount"
        )
        self.assertIn("is empty", " ".join(findings))

    def test_unknown_shell_is_rejected(self) -> None:
        """Refuse to classify a shell this project does not ship."""

        with self.assertRaises(harness.HomebrewAcceptanceError):
            harness.completion_text_findings(
                "powershell", "anything", command="asm", other_command="skillmount"
            )

    def test_linked_completions_accept_a_copy_or_a_symlink(self) -> None:
        """Accept prefix files Homebrew exposes as either a copy or a link."""

        keg = self.build_keg("skillmount", "skillmount")
        prefix = self.link_prefix(keg, "skillmount")
        self.assertEqual(
            harness.linked_completion_findings(prefix, keg, command="skillmount"), ()
        )
        zsh_relative = Path(harness.COMPLETION_LOCATIONS["zsh"][0][0]) / "_skillmount"
        (prefix / zsh_relative).unlink()
        (prefix / zsh_relative).symlink_to(keg / zsh_relative)
        self.assertEqual(
            harness.linked_completion_findings(prefix, keg, command="skillmount"), ()
        )

    def test_linked_completions_reject_drift_and_absence(self) -> None:
        """Reject a prefix completion file that differs or is missing."""

        keg = self.build_keg("skillmount-asm", "asm")
        prefix = self.link_prefix(keg, "asm")
        fish_relative = Path(harness.COMPLETION_LOCATIONS["fish"][0][0]) / "asm.fish"
        (prefix / fish_relative).write_text("complete -c asm -l other\n", encoding="utf-8")
        findings = harness.linked_completion_findings(prefix, keg, command="asm")
        self.assertEqual(len(findings), 1)
        self.assertIn("digest is", findings[0])
        (prefix / fish_relative).unlink()
        findings = harness.linked_completion_findings(prefix, keg, command="asm")
        self.assertEqual(len(findings), 1)
        self.assertIn("expected exactly 1", findings[0])


class UninstallTests(FixtureCase):
    """Post-uninstall absence classification."""

    def test_clean_uninstall_reports_nothing(self) -> None:
        """Accept an uninstall that removed the keg, link, and completions."""

        keg = self.build_keg("skillmount", "skillmount")
        prefix = self.link_prefix(keg, "skillmount")
        owned = {
            str(path): harness.digest_or_none(path)
            for shell in harness.COMPLETION_SHELLS
            for path in harness.completion_layout(prefix, command="skillmount")[shell]
        }
        self.assertEqual(len(owned), len(harness.COMPLETION_SHELLS))
        shutil.rmtree(keg)
        (prefix / "bin" / "skillmount").unlink()
        for name in owned:
            Path(name).unlink()
        self.assertEqual(
            harness.uninstall_findings(prefix, keg, command="skillmount", owned=owned), ()
        )

    def test_surviving_keg_link_and_completion_fail(self) -> None:
        """Reject an uninstall that left the keg, link, or completions behind."""

        keg = self.build_keg("skillmount", "skillmount")
        prefix = self.link_prefix(keg, "skillmount")
        owned = {
            str(path): harness.digest_or_none(path)
            for shell in harness.COMPLETION_SHELLS
            for path in harness.completion_layout(prefix, command="skillmount")[shell]
        }
        findings = harness.uninstall_findings(
            prefix, keg, command="skillmount", owned=owned
        )
        joined = " ".join(findings)
        self.assertIn("still exists after uninstall", joined)
        self.assertIn("survived the uninstall", joined)
        self.assertIn("survived uninstall with digest", joined)

    def test_dangling_link_fails(self) -> None:
        """Reject a completion link left pointing at a removed keg."""

        keg = self.build_keg("skillmount-asm", "asm")
        prefix = self.root / "prefix"
        relative = Path(harness.COMPLETION_LOCATIONS["zsh"][0][0]) / "_asm"
        (prefix / relative).parent.mkdir(parents=True)
        (prefix / relative).symlink_to(keg / relative)
        owned = {str(prefix / relative): harness.digest_or_none(prefix / relative)}
        shutil.rmtree(keg)
        findings = harness.uninstall_findings(prefix, keg, command="asm", owned=owned)
        self.assertIn("is a dangling link after uninstall", " ".join(findings))

    def test_replaced_completion_file_is_not_reported(self) -> None:
        """Accept an unrelated file that reused the completion path."""

        keg = self.build_keg("skillmount", "skillmount")
        prefix = self.link_prefix(keg, "skillmount")
        relative = Path(harness.COMPLETION_LOCATIONS["bash"][0][0]) / "skillmount"
        owned = {str(prefix / relative): harness.digest_or_none(prefix / relative)}
        shutil.rmtree(keg)
        (prefix / "bin" / "skillmount").unlink()
        (prefix / relative).write_text("# unrelated content\n", encoding="utf-8")
        self.assertEqual(
            harness.uninstall_findings(prefix, keg, command="skillmount", owned=owned), ()
        )


class SentinelTests(FixtureCase):
    """Unrelated-path comparison across the lifecycle."""

    def test_unchanged_paths_report_nothing(self) -> None:
        """Accept sentinel files whose bytes never changed."""

        first = self.root / "sentinel-one.txt"
        second = self.root / "missing-profile"
        first.write_bytes(harness.SENTINEL_CONTENT)
        before = harness.capture_digests((first, second))
        self.assertEqual(before[str(second)], None)
        self.assertEqual(
            harness.sentinel_findings(before, harness.capture_digests((first, second))), ()
        )

    def test_modified_removed_and_created_paths_fail(self) -> None:
        """Report a changed, removed, or newly created unrelated path."""

        changed = self.root / "changed.txt"
        removed = self.root / "removed.txt"
        created = self.root / "created.txt"
        changed.write_bytes(harness.SENTINEL_CONTENT)
        removed.write_bytes(harness.SENTINEL_CONTENT)
        before = harness.capture_digests((changed, removed, created))
        changed.write_bytes(b"tampered\n")
        removed.unlink()
        created.write_bytes(b"new\n")
        findings = harness.sentinel_findings(
            before, harness.capture_digests((changed, removed, created))
        )
        self.assertEqual(len(findings), 3)
        joined = " ".join(findings)
        self.assertIn("digest is", joined)
        self.assertIn("was removed", joined)
        self.assertIn("was created with digest", joined)

    def test_unobserved_path_fails(self) -> None:
        """Report a path that was not re-observed after the lifecycle."""

        findings = harness.sentinel_findings({"/tmp/one": None}, {})
        self.assertEqual(len(findings), 1)
        self.assertIn("was not re-observed", findings[0])
        findings = harness.sentinel_findings({}, {"/tmp/two": None})
        self.assertEqual(len(findings), 1)
        self.assertIn("only after", findings[0])


class FormulaTextTests(FixtureCase):
    """Rendered Formula text classification."""

    def test_platform_requirements_are_required(self) -> None:
        """Accept a Formula restricted to Apple Silicon macOS."""

        text = FORMULA_TEMPLATE.format(digest=DIGEST)
        self.assertEqual(harness.platform_findings(text, formula_class="skillmount"), ())

    def test_missing_or_forbidden_declarations_fail(self) -> None:
        """Reject Linux support, missing platform gates, and conflicts."""

        text = FORMULA_TEMPLATE.format(digest=DIGEST)
        without_arch = text.replace("  depends_on arch: :arm64\n", "")
        findings = harness.platform_findings(without_arch, formula_class="skillmount")
        self.assertIn("depends_on arch: :arm64", " ".join(findings))
        with_conflict = text.replace(
            "  depends_on :macos\n", '  depends_on :macos\n  conflicts_with "skillmount-asm"\n'
        )
        findings = harness.platform_findings(with_conflict, formula_class="skillmount")
        self.assertIn("conflicts_with", " ".join(findings))
        with_linux = text.replace("  depends_on :macos\n", "  on_linux do\n  end\n")
        findings = harness.platform_findings(with_linux, formula_class="skillmount")
        joined = " ".join(findings)
        self.assertIn("on_linux", joined)
        self.assertIn("depends_on :macos", joined)

    def test_audit_offences_are_waived_only_for_a_local_archive(self) -> None:
        """Waive HTTPS-URL offences only while the release archive is local."""

        output = (
            "skillmount:\n"
            "  * line 5: Please use a secure URL for the stable url\n"
            "  * line 12: `desc` should not start with an article\n"
        )
        findings = harness.audit_findings(output, local_archive=True)
        self.assertEqual(len(findings), 1)
        self.assertIn("should not start with an article", findings[0])
        findings = harness.audit_findings(output, local_archive=False)
        self.assertEqual(len(findings), 2)
        self.assertEqual(harness.audit_findings("skillmount:\n\n", local_archive=False), ())

    def test_version_output_must_match_exactly(self) -> None:
        """Require the packaged version banner both executables print."""

        self.assertEqual(
            harness.version_findings(
                f"{release.PRODUCT_NAME} {VERSION}\n", command="asm", version=VERSION
            ),
            (),
        )
        for observed in (
            f"{release.PRODUCT_NAME} 0.1.0\n",
            f"asm {VERSION}\n",
            f"{release.PRODUCT_NAME} {VERSION}\nextra\n",
            "",
        ):
            with self.subTest(observed=observed):
                findings = harness.version_findings(
                    observed, command="asm", version=VERSION
                )
                self.assertEqual(len(findings), 1)
                self.assertIn(repr(observed), findings[0])


class SelectionTests(FixtureCase):
    """Matrix selectors, manifest parsing, and upgrade eligibility."""

    def test_formula_selection_keeps_pair_order(self) -> None:
        """Return package ids in the immutable pair order."""

        self.assertEqual(harness.select_formulae(None), harness.FORMULA_IDS)
        self.assertEqual(harness.select_formulae([]), harness.FORMULA_IDS)
        self.assertEqual(
            harness.select_formulae(["skillmount-asm", "skillmount"]), harness.FORMULA_IDS
        )
        self.assertEqual(harness.select_formulae(["skillmount-asm"]), ("skillmount-asm",))
        with self.assertRaises(harness.HomebrewAcceptanceError) as caught:
            harness.select_formulae(["asm"])
        self.assertIn("asm", str(caught.exception))

    def test_phase_selection_adds_prerequisites(self) -> None:
        """Add every prerequisite phase in canonical order."""

        self.assertEqual(harness.expand_phases(None), harness.PHASE_ORDER)
        self.assertEqual(
            harness.expand_phases(["completions"]),
            ("trust", "install-skillmount-alone", "completions", "install-asm-alone"),
        )
        self.assertEqual(
            harness.expand_phases(["cross-uninstall"]),
            ("trust", "co-install", "cross-uninstall"),
        )
        self.assertEqual(
            harness.expand_phases(["uninstall", "selected-only"]),
            (
                "trust",
                "install-skillmount-alone",
                "selected-only",
                "uninstall",
                "install-asm-alone",
            ),
        )
        self.assertEqual(
            harness.expand_phases(["upgrade-from-prior"]), ("trust", "upgrade-from-prior")
        )
        self.assertEqual(harness.expand_phases(["style"]), ("style",))
        self.assertEqual(harness.expand_phases(["audit"]), ("audit",))
        with self.assertRaises(harness.HomebrewAcceptanceError) as caught:
            harness.expand_phases(["install"])
        self.assertIn("install", str(caught.exception))

    def test_cargo_manifest_version_is_read_from_the_package_table(self) -> None:
        """Read the version of the root package, not of a dependency."""

        manifest = (
            '[package]\nname = "skillmount"\nversion = "0.4.2"\nedition = "2024"\n\n'
            '[dependencies]\nclap = { version = "4.6.8" }\nversion = "9.9.9"\n'
        )
        self.assertEqual(harness.cargo_version_from_manifest(manifest), "0.4.2")
        repository = Path(__file__).resolve().parents[2]
        observed = harness.cargo_version_from_manifest(
            (repository / "Cargo.toml").read_text(encoding="utf-8")
        )
        self.assertEqual(observed, release.validate_stable_version(observed))
        for malformed in (
            '[dependencies]\nversion = "1.0.0"\n',
            '[package]\nname = "skillmount"\n',
            '[package]\nversion = "1.0.0"\nversion = "1.0.1"\n',
            '[package]\nversion = "1.0"\n',
        ):
            with self.subTest(malformed=malformed):
                with self.assertRaises(
                    (harness.HomebrewAcceptanceError, release.ReleaseError)
                ):
                    harness.cargo_version_from_manifest(malformed)

    def test_archive_override_requires_a_digest(self) -> None:
        """Require an explicit digest beside an overridden release-archive URL."""

        url = "https://example.invalid/skillmount-v0.2.0-aarch64-apple-darwin.tar.gz"
        self.assertIsNone(harness.archive_override(None, None))
        override = harness.archive_override(url, DIGEST)
        self.assertEqual(
            override, harness.ReleaseArchive(url=url, sha256=DIGEST, path=None)
        )
        self.assertFalse(override.local)
        self.assertTrue(
            harness.archive_override(
                "file:///tmp/skillmount-v0.2.0-aarch64-apple-darwin.tar.gz", DIGEST
            ).local
        )
        for malformed_url, digest in (
            (url, None),
            (None, DIGEST),
            ("ftp://example.invalid/archive.tar.gz", DIGEST),
            ("https://example.invalid/a b.tar.gz", DIGEST),
            (url, "C" * 64),
            (url, "abc"),
        ):
            with self.subTest(url=malformed_url, digest=digest):
                with self.assertRaises(harness.HomebrewAcceptanceError):
                    harness.archive_override(malformed_url, digest)

    def test_upgrade_rehearsal_skips_a_prior_release_without_completions(self) -> None:
        """Skip the rehearsal when the prior release predates `completions`."""

        decision = harness.upgrade_decision(
            prior_tag="v0.1.0",
            prior_version="0.1.0",
            candidate_version=VERSION,
            prior_cli_source=None,
            require_upgrade=False,
        )
        self.assertEqual(decision.status, "skipped")
        self.assertFalse(decision.eligible)
        self.assertIn("src/cli.rs", decision.reason)
        decision = harness.upgrade_decision(
            prior_tag="v0.1.0",
            prior_version="0.1.0",
            candidate_version=VERSION,
            prior_cli_source="enum CliCommand { Mount(MountArgs) }",
            require_upgrade=False,
        )
        self.assertEqual(decision.status, "skipped")
        self.assertIn(harness.COMPLETIONS_MARKER, decision.reason)

    def test_require_upgrade_turns_a_skip_into_a_failure(self) -> None:
        """Fail the rehearsal when the operator demanded it."""

        decision = harness.upgrade_decision(
            prior_tag="v0.1.0",
            prior_version="0.1.0",
            candidate_version=VERSION,
            prior_cli_source=None,
            require_upgrade=True,
        )
        self.assertEqual(decision.status, "failed")
        self.assertIn("--require-upgrade", decision.reason)

    def test_upgrade_rehearsal_needs_a_newer_candidate(self) -> None:
        """Skip a degenerate or backwards upgrade and accept a real one."""

        source = "Completions(CompletionArgs) // completions"
        equal = harness.upgrade_decision(
            prior_tag="v0.2.0",
            prior_version=VERSION,
            candidate_version=VERSION,
            prior_cli_source=source,
            require_upgrade=False,
        )
        self.assertEqual(equal.status, "skipped")
        self.assertIn("equals candidate version", equal.reason)
        backwards = harness.upgrade_decision(
            prior_tag="v0.3.0",
            prior_version="0.3.0",
            candidate_version=VERSION,
            prior_cli_source=source,
            require_upgrade=False,
        )
        self.assertEqual(backwards.status, "skipped")
        self.assertIn("is newer than", backwards.reason)
        eligible = harness.upgrade_decision(
            prior_tag="v0.2.0",
            prior_version=VERSION,
            candidate_version="0.3.0",
            prior_cli_source=source,
            require_upgrade=True,
        )
        self.assertEqual(eligible.status, "eligible")
        self.assertTrue(eligible.eligible)

    def test_version_order_validates_before_comparing(self) -> None:
        """Compare only validated stable versions."""

        self.assertEqual(harness.version_order("1.20.3"), (1, 20, 3))
        self.assertLess(harness.version_order("0.2.0"), harness.version_order("0.10.0"))
        with self.assertRaises(release.ReleaseError):
            harness.version_order("0.2.0-rc.1")


class OptionResolutionTests(FixtureCase):
    """Command-line resolution against a repository fixture."""

    def resolve(self, arguments: Sequence[str]) -> harness.HarnessOptions:
        """Resolve options against a minimal repository fixture."""

        return harness.resolve_options(harness.argument_parser().parse_args(list(arguments)))

    def setUp(self) -> None:
        """Create a repository fixture with a manifest and templates."""

        super().setUp()
        (self.root / "packaging" / "homebrew").mkdir(parents=True)
        (self.root / "Cargo.toml").write_text(
            '[package]\nname = "skillmount"\nversion = "0.2.0"\n', encoding="utf-8"
        )

    def test_defaults_come_from_the_checked_out_manifest(self) -> None:
        """Derive the version, tag, and template directory from the repository."""

        options = self.resolve(["--repository", str(self.root)])
        self.assertEqual(options.version, VERSION)
        self.assertEqual(options.tag, TAG)
        self.assertIsNone(options.commit)
        self.assertIsNone(options.archive)
        self.assertEqual(options.formula_ids, harness.FORMULA_IDS)
        self.assertEqual(options.phases, harness.PHASE_ORDER)
        self.assertEqual(options.repository, self.root.resolve())
        self.assertEqual(
            options.template_directory, self.root.resolve() / "packaging" / "homebrew"
        )
        self.assertFalse(options.require_upgrade)
        self.assertEqual(options.prior_tag, harness.PRIOR_TAG)

    def test_selectors_and_overrides_are_validated(self) -> None:
        """Resolve matrix selectors, a pinned commit, and a release-archive override."""

        archive_url = (
            "https://example.invalid/skillmount-v0.2.0-aarch64-apple-darwin.tar.gz"
        )
        options = self.resolve(
            [
                "--repository",
                str(self.root),
                "--formula",
                "skillmount-asm",
                "--phase",
                "brew-test",
                "--commit",
                COMMIT,
                "--archive-url-override",
                archive_url,
                "--archive-sha256",
                DIGEST,
                "--require-upgrade",
            ]
        )
        self.assertEqual(options.formula_ids, ("skillmount-asm",))
        self.assertEqual(
            options.phases,
            ("trust", "install-skillmount-alone", "brew-test", "install-asm-alone"),
        )
        self.assertEqual(options.commit, COMMIT)
        self.assertEqual(options.archive.url, archive_url)
        self.assertTrue(options.require_upgrade)

    def test_inconsistent_or_missing_inputs_fail(self) -> None:
        """Fail closed on a mismatched tag, missing manifest, or absent templates."""

        with self.assertRaises(harness.HomebrewAcceptanceError) as caught:
            self.resolve(["--repository", str(self.root), "--tag", "v9.9.9"])
        self.assertIn("v9.9.9", str(caught.exception))
        with self.assertRaises((harness.HomebrewAcceptanceError, release.ReleaseError)):
            self.resolve(["--repository", str(self.root), "--commit", "not-a-commit"])
        shutil.rmtree(self.root / "packaging")
        with self.assertRaises(harness.HomebrewAcceptanceError) as caught:
            self.resolve(["--repository", str(self.root)])
        self.assertIn("packaging", str(caught.exception))
        (self.root / "Cargo.toml").unlink()
        with self.assertRaises(harness.HomebrewAcceptanceError) as caught:
            self.resolve(["--repository", str(self.root)])
        self.assertIn("Cargo.toml", str(caught.exception))

    def test_preflight_inputs_reject_a_manual_identity(self) -> None:
        """Refuse to mix a preflight artifact with a hand-written identity."""

        artifact = self.root / "inputs.json"
        artifact.write_text("{}\n", encoding="utf-8")
        with self.assertRaises(harness.HomebrewAcceptanceError) as caught:
            self.resolve(
                [
                    "--repository",
                    str(self.root),
                    "--inputs",
                    str(artifact),
                    "--tag",
                    TAG,
                    "--archive-sha256",
                    DIGEST,
                ]
            )
        message = str(caught.exception)
        self.assertIn("--tag", message)
        self.assertIn("--archive-sha256", message)


class ReportTests(FixtureCase):
    """Report shaping, aggregation, and phase bookkeeping."""

    def phases(self) -> list[harness.Phase]:
        """Return one representative phase per status."""

        passed = harness.Phase(name="style")
        passed.record(
            harness.CommandEvidence(
                argv=("brew", "style", "--formula", "skillmount"),
                returncode=0,
                stdout="ok\n",
                stderr="",
            )
        )
        passed.note("skillmount resolved through the disposable tap")
        passed.settle()
        skipped = harness.Phase(name="upgrade-from-prior")
        skipped.skip("v0.1.0 predates the completions command")
        failed = harness.Phase(name="selected-only")
        failed.add(("keg holds both executables",))
        failed.settle()
        return [passed, skipped, failed]

    def test_phase_settlement_follows_findings(self) -> None:
        """Resolve a pending phase from its findings and keep a skip intact."""

        passed, skipped, failed = self.phases()
        self.assertEqual(passed.status, "passed")
        self.assertEqual(skipped.status, "skipped")
        self.assertEqual(failed.status, "failed")
        skipped.add(("late finding",))
        skipped.settle()
        self.assertEqual(skipped.status, "skipped")

    def test_status_aggregation(self) -> None:
        """Aggregate the report status from every phase and from cleanup."""

        passed, skipped, failed = self.phases()
        self.assertEqual(harness.aggregate_status([passed, skipped]), "passed")
        self.assertEqual(harness.aggregate_status([skipped]), "skipped")
        self.assertEqual(harness.aggregate_status([passed, failed]), "failed")
        self.assertEqual(
            harness.aggregate_status([passed, harness.Phase(name="audit")]), "incomplete"
        )
        self.assertEqual(harness.aggregate_status([passed], ("cleanup failed",)), "failed")

    def test_report_document_round_trips(self) -> None:
        """Shape a deterministic report document that reloads unchanged."""

        phases = self.phases()
        document = harness.build_report(
            options=options_for(self.root),
            commit=COMMIT,
            archive=harness.ReleaseArchive(
                url="file:///tmp/skillmount-v0.2.0-aarch64-apple-darwin.tar.gz",
                sha256=DIGEST,
                path=self.root / "a",
            ),
            prefix=Path(harness.SUPPORTED_PREFIX),
            environment={"brew": ["Homebrew 5.0.0"], "shells": {"bash": "bash 5.3"}},
            trust={
                "argv": [["brew", "trust", "--formula", acceptance_reference("skillmount")]],
                "brew": ["Homebrew 6.0.12"],
                "restore": harness.TRUST_RESTORE_MECHANISM,
                "restored": "brew untrust --formula",
            },
            phases=phases,
            cleanup=[
                harness.CommandEvidence(
                    argv=("brew", "untap", harness.ACCEPTANCE_TAP),
                    returncode=0,
                    stdout="",
                    stderr="",
                )
            ],
            cleanup_findings=(),
        )
        self.assertEqual(document["schema"], harness.REPORT_SCHEMA)
        self.assertEqual(document["status"], "failed")
        self.assertEqual(document["tap"], harness.ACCEPTANCE_TAP)
        self.assertEqual(document["version"], VERSION)
        self.assertEqual(document["commit"], COMMIT)
        self.assertEqual(document["prefix"], harness.SUPPORTED_PREFIX)
        self.assertEqual(document["coverage_gaps"], [])
        self.assertTrue(document["archive"]["local"])
        self.assertEqual(
            document["trust"]["argv"],
            [["brew", "trust", "--formula", acceptance_reference("skillmount")]],
        )
        self.assertEqual(document["trust"]["brew"], ["Homebrew 6.0.12"])
        self.assertIn("brew untrust --formula", str(document["trust"]["restore"]))
        text = harness.report_text(document)
        self.assertTrue(text.endswith("\n"))
        self.assertEqual(json.loads(text), document)
        names = [phase["name"] for phase in document["phases"]]
        self.assertEqual(names, ["style", "upgrade-from-prior", "selected-only"])
        self.assertEqual(
            document["phases"][1]["reason"], "v0.1.0 predates the completions command"
        )
        summary = harness.summary_text(document)
        for name in names:
            self.assertIn(name, summary)
        self.assertIn("keg holds both executables", summary)
        self.assertIn("Homebrew acceptance status: failed", summary)

    def test_summary_rejects_a_malformed_document(self) -> None:
        """Fail closed when a report document has no phase list."""

        with self.assertRaises(harness.HomebrewAcceptanceError):
            harness.summary_text({"status": "passed"})

    def test_evidence_is_bounded(self) -> None:
        """Keep only the diagnostic tail of very long command output."""

        self.assertEqual(harness.bounded_text("short"), "short")
        trimmed = harness.bounded_text("a" * 40 + "TAIL", limit=8)
        self.assertTrue(trimmed.endswith("aaaaTAIL"))
        self.assertIn("36 earlier characters elided", trimmed)
        with self.assertRaises(harness.HomebrewAcceptanceError):
            harness.bounded_text("value", limit=0)
        evidence = harness.CommandEvidence(
            argv=("brew", "install", "--formula", "skillmount", "--extra"),
            returncode=1,
            stdout="x" * (harness.EVIDENCE_LIMIT + 10),
            stderr="boom\n",
        )
        self.assertEqual(evidence.label, "brew install --formula skillmount")
        shaped = evidence.to_json_object()
        self.assertEqual(shaped["status"], 1)
        self.assertEqual(shaped["argv"][-1], "--extra")
        self.assertLess(len(shaped["stdout"]), len(evidence.stdout) + 40)
        self.assertEqual(shaped["stderr"], "boom\n")


class CoverageTests(FixtureCase):
    """Every spec scenario maps to a named phase, and every phase proves one."""

    def spec_path(self) -> Path:
        """Return the tracked `homebrew-distribution` spec, or skip."""

        repository = Path(__file__).resolve().parents[2]
        candidates = sorted(repository.glob("rasen/**/homebrew-distribution/spec.md"))
        if not candidates:
            raise unittest.SkipTest("homebrew-distribution/spec.md is not in this checkout")
        return candidates[0]

    def test_no_coverage_gaps(self) -> None:
        """Require every phase to prove at least one scenario."""

        self.assertEqual(harness.coverage_gaps(), ())
        self.assertEqual(len(harness.SCENARIO_COVERAGE), 23)

    def test_every_spec_scenario_is_mapped(self) -> None:
        """Require the mapping table to name exactly the spec's scenarios."""

        text = self.spec_path().read_text(encoding="utf-8")
        scenarios = [
            line.removeprefix("#### Scenario:").strip()
            for line in text.splitlines()
            if line.startswith("#### Scenario:")
        ]
        self.assertEqual(len(scenarios), len(set(scenarios)))
        self.assertEqual(
            sorted(scenarios),
            sorted(coverage.scenario for coverage in harness.SCENARIO_COVERAGE),
        )

    def test_every_spec_requirement_is_mapped(self) -> None:
        """Require the mapping table to name exactly the spec's requirements."""

        text = self.spec_path().read_text(encoding="utf-8")
        requirements = [
            line.removeprefix("### Requirement:").strip()
            for line in text.splitlines()
            if line.startswith("### Requirement:")
        ]
        self.assertEqual(
            sorted(set(requirements)),
            sorted({coverage.requirement for coverage in harness.SCENARIO_COVERAGE}),
        )

    def test_coverage_text_names_every_phase(self) -> None:
        """Print a mapping that names every phase and flags no gap."""

        rendered = harness.coverage_text()
        for phase in harness.PHASE_ORDER:
            self.assertIn(phase, rendered)
        self.assertIn("gaps: none", rendered)


class GatewayTests(FixtureCase):
    """The thin command boundary, exercised without Homebrew."""

    def test_output_and_status_are_captured(self) -> None:
        """Capture status, stdout, and stderr from one real process."""

        evidence = harness.run_command(
            [
                sys.executable,
                "-c",
                "import sys; print('out'); print('err', file=sys.stderr); sys.exit(3)",
            ]
        )
        self.assertEqual(evidence.returncode, 3)
        self.assertEqual(evidence.stdout, "out\n")
        self.assertEqual(evidence.stderr, "err\n")
        self.assertEqual(evidence.argv[0], sys.executable)

    def test_missing_executable_fails_closed(self) -> None:
        """Report a missing executable instead of treating it as success."""

        with self.assertRaises(harness.HomebrewAcceptanceError) as caught:
            harness.run_command([str(self.root / "definitely-absent-binary")])
        self.assertIn("is not installed", str(caught.exception))

    def test_timeout_fails_closed(self) -> None:
        """Report a hung command instead of waiting forever."""

        with self.assertRaises(harness.HomebrewAcceptanceError) as caught:
            harness.run_command(
                [sys.executable, "-c", "import time; time.sleep(5)"], timeout=1
            )
        self.assertIn("exceeded 1s", str(caught.exception))

    def test_homebrew_behaviour_is_pinned_off(self) -> None:
        """Pin Homebrew's implicit update and cleanup behaviour off."""

        gateway = harness.SubprocessGateway(self.root, environment={"PATH": os.environ["PATH"]})
        for name, value in harness.HOMEBREW_PINS.items():
            self.assertEqual(gateway.environment[name], value)
        self.assertEqual(gateway.repository, self.root.resolve())

    def test_missing_channel_model_fails_closed(self) -> None:
        """Report the missing shared channel model instead of guessing."""

        scripts = Path(harness.__file__).resolve().parent
        saved_path = list(sys.path)
        saved_module = sys.modules.pop("package_channels", None)
        sys.path[:] = [
            entry for entry in sys.path if Path(entry or ".").resolve() != scripts
        ]
        try:
            with self.assertRaises(harness.HomebrewAcceptanceError) as caught:
                harness.import_package_channels()
            self.assertIn("package_channels.py", str(caught.exception))
        finally:
            sys.path[:] = saved_path
            if saved_module is not None:
                sys.modules["package_channels"] = saved_module

    def channel_model(self) -> types.ModuleType:
        """Return the shared channel model, or skip when it is not in the checkout."""

        try:
            return harness.import_package_channels()
        except harness.HomebrewAcceptanceError as error:
            raise unittest.SkipTest(str(error)) from error

    def test_channel_model_selection_is_pinned(self) -> None:
        """Require the shared channel model to keep the immutable selection map."""

        module = self.channel_model()
        self.assertEqual(
            [identity.package_id for identity in module.PACKAGES], list(harness.FORMULA_IDS)
        )
        for package_id, (command, other) in harness.PACKAGE_TABLE_PINS.items():
            identity = module.package_for(package_id)
            self.assertEqual((identity.command, identity.other.command), (command, other))

    def preflight_artifact(self, module: types.ModuleType) -> Path:
        """Write one strictly valid preflight artifact for the release identity."""

        repository = module.DEFAULT_REPOSITORY
        archives = tuple(
            module.ArchiveIdentity(
                triple=target.triple,
                name=release.asset_name(TAG, target),
                url=module.asset_download_url(repository, TAG, release.asset_name(TAG, target)),
                sha256=DIGEST,
            )
            for target in sorted(release.TARGETS, key=lambda target: target.triple)
        )
        inputs = module.PackageInputs(
            repository=repository,
            version=VERSION,
            tag=TAG,
            commit=COMMIT,
            release_url=f"https://github.com/{repository}/releases/tag/{TAG}",
            archives=archives,
        )
        path = self.root / "inputs.json"
        path.write_text(inputs.to_json(), encoding="utf-8")
        return path

    def test_preflight_artifact_supplies_the_release_identity(self) -> None:
        """Take the version, tag, commit, and checked macOS archive from preflight."""

        module = self.channel_model()
        path = self.preflight_artifact(module)
        version, tag, commit, archive = harness.preflight_identity(path)
        self.assertEqual((version, tag, commit), (VERSION, TAG, COMMIT))
        self.assertEqual(archive.sha256, DIGEST)
        self.assertTrue(archive.url.startswith("https://github.com/"))
        self.assertIn(release.asset_name(TAG, module.MACOS_ARM64), archive.url)
        self.assertFalse(archive.local)
        self.assertIsNone(archive.path)

    def test_tampered_preflight_artifact_is_rejected(self) -> None:
        """Refuse a preflight artifact whose macOS release archive was replaced."""

        module = self.channel_model()
        path = self.preflight_artifact(module)
        document = json.loads(path.read_text(encoding="utf-8"))
        macos = next(
            archive
            for archive in document["archives"]
            if archive["triple"] == module.MACOS_ARM64.triple
        )
        macos["url"] = "https://example.invalid/foreign.tar.gz"
        path.write_text(json.dumps(document), encoding="utf-8")
        with self.assertRaises(harness.HomebrewAcceptanceError) as caught:
            harness.preflight_identity(path)
        self.assertIn(str(path), str(caught.exception))

    def test_reordered_channel_model_is_rejected(self) -> None:
        """Refuse a channel model whose pair order or selection drifted."""

        stub = types.ModuleType("package_channels")
        stub.PACKAGES = tuple(
            types.SimpleNamespace(package_id=package_id)
            for package_id in reversed(harness.FORMULA_IDS)
        )
        saved_module = sys.modules.get("package_channels")
        sys.modules["package_channels"] = stub
        try:
            with self.assertRaises(harness.HomebrewAcceptanceError) as caught:
                harness.import_package_channels()
            message = str(caught.exception)
            for package_id in harness.FORMULA_IDS:
                self.assertIn(package_id, message)
        finally:
            if saved_module is None:
                del sys.modules["package_channels"]
            else:
                sys.modules["package_channels"] = saved_module


class PhaseBookkeepingTests(FixtureCase):
    """Phase tables, aborts, and selection bookkeeping inside the harness."""

    def test_unselected_phases_start_skipped(self) -> None:
        """Mark every unselected phase as skipped before anything runs."""

        subject = harness.Harness(
            ForbiddenGateway(), options_for(self.root, phases=("style", "audit"))
        )
        self.assertEqual(subject.phases["style"].status, "pending")
        self.assertEqual(subject.phases["co-install"].status, "skipped")
        self.assertIn("--phase", subject.phases["co-install"].reason)
        self.assertTrue(subject.enabled("style"))
        self.assertFalse(subject.enabled("co-install"))
        with self.assertRaises(harness.HomebrewAcceptanceError):
            subject.phase("not-a-phase")

    def test_abort_is_attributed_to_the_active_phase(self) -> None:
        """Attribute an aborting failure to the phase that was running."""

        subject = harness.Harness(ForbiddenGateway(), options_for(self.root))
        subject.phase("audit")
        subject.record_failure(harness.HomebrewAcceptanceError("brew audit vanished"))
        self.assertEqual(subject.phases["audit"].status, "failed")
        self.assertIn("brew audit vanished", " ".join(subject.phases["audit"].findings))

    def test_abort_before_any_phase_lands_on_the_first_pending_phase(self) -> None:
        """Attribute an early failure to the first phase that had not run."""

        subject = harness.Harness(ForbiddenGateway(), options_for(self.root))
        subject.record_failure(harness.HomebrewAcceptanceError("tap already exists"))
        self.assertEqual(subject.phases["style"].status, "failed")
        self.assertIn("tap already exists", " ".join(subject.phases["style"].findings))

    def test_selection_requires_a_resolved_command_table(self) -> None:
        """Refuse to inspect a package whose command selection is unknown."""

        subject = harness.Harness(ForbiddenGateway(), options_for(self.root))
        with self.assertRaises(harness.HomebrewAcceptanceError):
            subject.selection("skillmount")
        subject.commands = dict(harness.PACKAGE_TABLE_PINS)
        self.assertEqual(subject.selection("skillmount"), ("skillmount", "asm"))
        self.assertEqual(subject.selection("skillmount-asm"), ("asm", "skillmount"))
        self.assertEqual(
            subject.formula_reference("skillmount-asm"),
            f"{harness.ACCEPTANCE_TAP}/skillmount-asm",
        )

    def test_uninstalled_package_has_no_keg(self) -> None:
        """Refuse to resolve a keg for a package that was never installed."""

        subject = harness.Harness(ForbiddenGateway(), options_for(self.root))
        with self.assertRaises(harness.HomebrewAcceptanceError):
            subject.keg_for("skillmount", version=VERSION)
        with self.assertRaises(harness.HomebrewAcceptanceError):
            subject.require_prefix()
        with self.assertRaises(harness.HomebrewAcceptanceError):
            subject.require_archive()


class BrewInvocationTests(FixtureCase):
    """The exact `brew` argv this harness is documented to run."""

    def harness_for(self, responses: dict[tuple[str, ...], tuple[int, str, str]]):
        """Return a harness bound to one scripted gateway with a trusted tap."""

        trusted = [trusted_name(package_id) for package_id in harness.FORMULA_IDS]
        gateway = ScriptedGateway(
            {
                ("brew", "trust", "--json"): (0, trust_json(formulae=trusted), ""),
                ("brew", "trust", "--formula"): (0, "", ""),
                **responses,
            }
        )
        subject = harness.Harness(gateway, options_for(self.root))
        subject.prefix = Path(harness.SUPPORTED_PREFIX)
        subject.commands = dict(harness.PACKAGE_TABLE_PINS)
        subject.trusted = [
            acceptance_reference(package_id) for package_id in harness.FORMULA_IDS
        ]
        subject.added_trust = list(subject.trusted)
        return gateway, subject

    def test_install_names_the_formula_and_resolves_its_cellar(self) -> None:
        """Install the checked archive through `--formula` and resolve its cellar."""

        cellar = self.root / "Cellar" / "skillmount"
        gateway, subject = self.harness_for(
            {
                ("brew", "install"): (0, "", ""),
                ("brew", "--cellar"): (0, f"{cellar}\n", ""),
            }
        )
        phase = subject.phase("install-skillmount-alone")
        self.assertTrue(subject.install(phase, "skillmount"))
        reference = f"{harness.ACCEPTANCE_TAP}/skillmount"
        self.assertEqual(
            gateway.calls,
            [
                ("brew", "trust", "--json", harness.TRUST_JSON_VERSION),
                ("brew", "trust", "--formula", reference),
                ("brew", "install", "--formula", reference),
                ("brew", "--cellar", reference),
            ],
        )
        self.assertEqual(subject.installed["skillmount"], cellar)
        phase.settle()
        self.assertEqual(phase.status, "passed")

    def test_failed_install_is_recorded_without_a_cellar_lookup(self) -> None:
        """Turn a failed archive install into a finding instead of a keg inspection."""

        gateway, subject = self.harness_for({("brew", "install"): (1, "", "install failed\n")})
        phase = subject.phase("install-asm-alone")
        self.assertFalse(subject.install(phase, "skillmount-asm"))
        self.assertEqual(
            [call[:2] for call in gateway.calls],
            [("brew", "trust"), ("brew", "trust"), ("brew", "install")],
        )
        self.assertNotIn("skillmount-asm", subject.installed)
        phase.settle()
        self.assertEqual(phase.status, "failed")
        self.assertIn("install failed", " ".join(phase.findings))

    def test_brew_test_takes_no_formula_flag(self) -> None:
        """Run `brew test installed_formula`, which accepts no `--formula`."""

        gateway, subject = self.harness_for({("brew", "test"): (0, "", "")})
        subject.observe_brew_test("skillmount-asm")
        self.assertEqual(
            gateway.calls, [("brew", "test", f"{harness.ACCEPTANCE_TAP}/skillmount-asm")]
        )
        subject.phases["brew-test"].settle()
        self.assertEqual(subject.phases["brew-test"].status, "passed")

    def test_uninstall_names_the_formula(self) -> None:
        """Remove exactly one Formula and forget it only when Homebrew agreed."""

        gateway, subject = self.harness_for({("brew", "uninstall"): (0, "", "")})
        subject.installed["skillmount"] = self.root / "Cellar" / "skillmount"
        subject.uninstall("skillmount")
        self.assertEqual(
            gateway.calls,
            [("brew", "uninstall", "--formula", f"{harness.ACCEPTANCE_TAP}/skillmount")],
        )
        self.assertEqual(subject.installed, {})
        self.assertEqual(len(subject.cleanup_commands), 1)

    def test_failed_uninstall_keeps_the_package_and_reports_it(self) -> None:
        """Keep a Formula recorded when its uninstall failed, and report cleanup."""

        gateway, subject = self.harness_for({("brew", "uninstall"): (1, "", "locked\n")})
        subject.installed["skillmount"] = self.root / "Cellar" / "skillmount"
        subject.cleanup()
        self.assertIn("skillmount", subject.installed)
        self.assertIn("locked", " ".join(subject.cleanup_findings))
        self.assertEqual(gateway.calls[0][:2], ("brew", "uninstall"))

    def test_tap_creation_refuses_leftover_state(self) -> None:
        """Refuse to reuse a disposable tap directory that already exists."""

        existing = self.root / "Taps" / "skillmount-acceptance" / "homebrew-homebrew-tap"
        existing.mkdir(parents=True)
        gateway, subject = self.harness_for(
            {("brew", "--repository"): (0, f"{existing}\n", "")}
        )
        with self.assertRaises(harness.HomebrewAcceptanceError) as caught:
            subject.create_tap()
        message = str(caught.exception)
        self.assertIn(str(existing), message)
        self.assertIn(f"brew untap {harness.ACCEPTANCE_TAP}", message)
        self.assertFalse(subject.tapped)
        self.assertEqual(gateway.calls, [("brew", "--repository", harness.ACCEPTANCE_TAP)])

    def test_tap_creation_places_both_formulae(self) -> None:
        """Create the tap without Git and copy both rendered Formulae into it."""

        located = self.root / "Taps" / "skillmount-acceptance" / "homebrew-homebrew-tap"
        gateway, subject = self.harness_for(
            {
                ("brew", "--repository"): (0, f"{located}\n", ""),
                ("brew", "tap-new"): (0, "", ""),
            }
        )
        self.assertEqual(subject.create_tap(), located)
        self.assertTrue(subject.tapped)
        self.assertTrue((located / "Formula").is_dir())
        self.assertEqual(
            gateway.calls,
            [
                ("brew", "--repository", harness.ACCEPTANCE_TAP),
                ("brew", "tap-new", "--no-git", harness.ACCEPTANCE_TAP),
            ],
        )
        rendered = {}
        for package_id in harness.FORMULA_IDS:
            path = self.root / "candidate" / "Formula" / f"{package_id}.rb"
            path.parent.mkdir(parents=True, exist_ok=True)
            path.write_text(f"# {package_id}\n", encoding="utf-8")
            rendered[package_id] = path
        placed = subject.place_formulae(rendered)
        self.assertEqual(sorted(placed), sorted(harness.FORMULA_IDS))
        for package_id, path in placed.items():
            self.assertEqual(path, located / "Formula" / f"{package_id}.rb")
            self.assertEqual(path.read_text(encoding="utf-8"), f"# {package_id}\n")
        del rendered["skillmount-asm"]
        with self.assertRaises(harness.HomebrewAcceptanceError) as caught:
            subject.place_formulae(rendered)
        self.assertIn("skillmount-asm", str(caught.exception))

    def test_style_requires_tap_ownership(self) -> None:
        """Reject a Formula reference that resolves outside the owned tap."""

        located = self.root / "Taps" / "skillmount-acceptance" / "homebrew-homebrew-tap"
        stray = self.root / "elsewhere" / "skillmount.rb"
        stray.parent.mkdir(parents=True)
        stray.write_text("# stray\n", encoding="utf-8")
        gateway, subject = self.harness_for(
            {
                ("brew", "formula"): (0, f"{stray}\n", ""),
                ("brew", "style"): (0, "", ""),
            }
        )
        subject.tap_root = located
        located.mkdir(parents=True)
        subject.phase_style()
        phase = subject.phases["style"]
        self.assertEqual(phase.status, "failed")
        self.assertIn(str(stray), " ".join(phase.findings))
        self.assertIn("disposable tap", " ".join(phase.findings))
        self.assertIn(
            ("brew", "style", "--formula", f"{harness.ACCEPTANCE_TAP}/skillmount"),
            gateway.calls,
        )

    def test_style_accepts_tap_owned_formulae(self) -> None:
        """Accept both Formulae when the owned tap resolves them."""

        located = self.root / "Taps" / "skillmount-acceptance" / "homebrew-homebrew-tap"
        (located / "Formula").mkdir(parents=True)
        owned = located / "Formula" / "skillmount.rb"
        owned.write_text("# owned\n", encoding="utf-8")
        gateway, subject = self.harness_for(
            {
                ("brew", "formula"): (0, f"{owned}\n", ""),
                ("brew", "style"): (0, "", ""),
            }
        )
        subject.tap_root = located
        subject.phase_style()
        phase = subject.phases["style"]
        self.assertEqual(phase.status, "passed")
        self.assertEqual(len(phase.observations), len(harness.FORMULA_IDS))
        self.assertEqual(len(gateway.calls), 2 * len(harness.FORMULA_IDS))

    def test_channel_errors_become_harness_errors(self) -> None:
        """Translate a rendering rejection into this harness's own error type."""

        class StubError(RuntimeError):
            """Stand-in for package_channels.ChannelError."""

        stub = types.ModuleType("package_channels")
        stub.ChannelError = StubError
        stub.DEFAULT_REPOSITORY = "pashifika/skillmount"
        stub.PACKAGES = tuple(
            types.SimpleNamespace(
                package_id=package_id,
                command=command,
                other=types.SimpleNamespace(command=other),
            )
            for package_id, (command, other) in harness.PACKAGE_TABLE_PINS.items()
        )
        stub.package_for = lambda package_id: next(
            identity for identity in stub.PACKAGES if identity.package_id == package_id
        )
        stub.MACOS_ARM64 = release.target_for("aarch64-apple-darwin")
        stub.ArchiveIdentity = lambda **values: values
        stub.PackageInputs = lambda **values: values

        def reject(inputs, *, template_directory, output_directory):
            raise StubError("template @UNKNOWN@ token is not a known value")

        stub.generate_formulae = reject
        saved_module = sys.modules.get("package_channels")
        sys.modules["package_channels"] = stub
        try:
            _, subject = self.harness_for({})
            archive = harness.ReleaseArchive(
                url="file:///tmp/skillmount-v0.2.0-aarch64-apple-darwin.tar.gz",
                sha256=DIGEST,
                path=None,
            )
            with self.assertRaises(harness.HomebrewAcceptanceError) as caught:
                subject.render_formulae(
                    self.root / "candidate",
                    archive=archive,
                    version=VERSION,
                    tag=TAG,
                    commit=COMMIT,
                )
            message = str(caught.exception)
            self.assertIn("@UNKNOWN@", message)
            self.assertIn("packaging", message)
        finally:
            if saved_module is None:
                del sys.modules["package_channels"]
            else:
                sys.modules["package_channels"] = saved_module


class TrustingGateway(ScriptedGateway):
    """Scripted gateway modelling Homebrew's name-keyed trust store on disk."""

    def __init__(
        self,
        responses: dict[tuple[str, ...], tuple[int, str, str]],
        *,
        environment: dict[str, str],
        formulae: Sequence[str] = (),
        trust_status: int = 0,
        untrust_status: int = 0,
        store_existed: bool = True,
    ) -> None:
        """Bind one fake trust store to the exact file Homebrew would read."""

        super().__init__(responses, environment=environment)
        self.store: dict[str, list[str]] = {
            **{section: list(names) for section, names in TRUST_STORE.items()},
            "formulae": sorted({*TRUST_STORE["formulae"], *formulae}),
        }
        self.trust_status = trust_status
        self.untrust_status = untrust_status
        self.path = harness.trust_store_path(environment)
        self.path.parent.mkdir(parents=True, exist_ok=True)
        if store_existed:
            self.write()

    def write(self) -> None:
        """Persist the fake trust store the way Homebrew persists its own."""

        self.path.write_text(json.dumps(self.store, indent=2) + "\n", encoding="utf-8")

    def answer(self, argv: tuple[str, ...]) -> harness.CommandEvidence:
        """Serve every trust subcommand from the fake store, else use the script."""

        if argv[:3] == ("brew", "trust", "--json"):
            self.calls.append(argv)
            return harness.CommandEvidence(
                argv=argv, returncode=0, stdout=json.dumps(self.store), stderr=""
            )
        if argv[:3] == ("brew", "trust", "--formula"):
            self.calls.append(argv)
            if self.trust_status != 0:
                return harness.CommandEvidence(
                    argv=argv,
                    returncode=self.trust_status,
                    stdout="",
                    stderr=f"Error: {harness.trust_refusal(argv[3])}\n",
                )
            name = harness.canonical_reference(argv[3])
            if name not in self.store["formulae"]:
                self.store["formulae"] = sorted([*self.store["formulae"], name])
                self.write()
            return harness.CommandEvidence(argv=argv, returncode=0, stdout="", stderr="")
        if argv[:3] == ("brew", "untrust", "--formula"):
            self.calls.append(argv)
            if self.untrust_status != 0:
                return harness.CommandEvidence(
                    argv=argv, returncode=self.untrust_status, stdout="", stderr="locked\n"
                )
            name = harness.canonical_reference(argv[3])
            self.store["formulae"] = [
                entry for entry in self.store["formulae"] if entry != name
            ]
            self.write()
            return harness.CommandEvidence(argv=argv, returncode=0, stdout="", stderr="")
        return super().answer(argv)


class TrustTests(FixtureCase):
    """Homebrew 6 refuses an untrusted third-party tap, so trust is proven and undone."""

    def trust_harness(
        self,
        responses: dict[tuple[str, ...], tuple[int, str, str]] | None = None,
        **keywords: object,
    ) -> tuple[TrustingGateway, harness.Harness]:
        """Return a harness whose trust store lives in this test's own HOME."""

        home = self.root / "home"
        home.mkdir(exist_ok=True)
        gateway = TrustingGateway(
            {} if responses is None else responses,
            environment={"HOME": str(home)},
            **keywords,
        )
        subject = harness.Harness(gateway, options_for(self.root))
        subject.prefix = Path(harness.SUPPORTED_PREFIX)
        subject.commands = dict(harness.PACKAGE_TABLE_PINS)
        subject.tap_root = self.root / "tap"
        subject.tap_root.mkdir(exist_ok=True)
        return gateway, subject

    def test_trust_store_path_follows_homebrews_own_rule(self) -> None:
        """Resolve the trust file from XDG_CONFIG_HOME, else from HOME."""

        self.assertEqual(
            harness.trust_store_path({"XDG_CONFIG_HOME": "/x/config", "HOME": "/u/me"}),
            Path("/x/config/homebrew/trust.json"),
        )
        self.assertEqual(
            harness.trust_store_path({"HOME": "/u/me"}), Path("/u/me/.homebrew/trust.json")
        )
        self.assertEqual(
            harness.trust_store_path({"XDG_CONFIG_HOME": "", "HOME": "/u/me"}),
            Path("/u/me/.homebrew/trust.json"),
        )
        for environment in ({}, {"HOME": ""}, {"HOME": "relative"}, {"XDG_CONFIG_HOME": "rel"}):
            with self.subTest(environment=environment):
                with self.assertRaises(harness.HomebrewAcceptanceError):
                    harness.trust_store_path(environment)

    def test_canonical_names_match_homebrews_own_spelling(self) -> None:
        """Strip the `homebrew-` repository prefix exactly as Homebrew reports it."""

        reference = acceptance_reference("skillmount")
        self.assertEqual(harness.canonical_reference(reference), trusted_name("skillmount"))
        self.assertEqual(trusted_name("skillmount"), "skillmount-acceptance/tap/skillmount")
        self.assertEqual(
            harness.canonical_tap_name(harness.ACCEPTANCE_TAP), "skillmount-acceptance/tap"
        )
        self.assertEqual(harness.canonical_tap_name("pashifika/tap"), "pashifika/tap")
        self.assertEqual(
            harness.trust_spellings(reference), (reference, trusted_name("skillmount"))
        )
        self.assertEqual(harness.trust_spellings("pashifika/tap/asm"), ("pashifika/tap/asm",))
        for malformed in ("", "skillmount", "a/b/c/d", "/skillmount", "a//c"):
            with self.subTest(malformed=malformed):
                with self.assertRaises(harness.HomebrewAcceptanceError):
                    harness.canonical_reference(malformed)

    def test_refusal_text_is_quoted_verbatim(self) -> None:
        """Quote Homebrew's own refusal, naming the canonical tap and remedy."""

        self.assertEqual(
            harness.trust_refusal(acceptance_reference("skillmount")),
            "Refusing to load formula skillmount-acceptance/tap/skillmount from untrusted tap "
            "skillmount-acceptance/tap. Run 'brew trust --formula "
            "skillmount-acceptance/tap/skillmount' or 'brew trust skillmount-acceptance/tap' to "
            "trust it.",
        )

    def test_trust_precedes_every_install_with_the_documented_argv(self) -> None:
        """Trust each Formula by name, then re-assert it immediately before installing."""

        cellar = self.root / "Cellar" / "skillmount"
        gateway, subject = self.trust_harness(
            {("brew", "install"): (0, "", ""), ("brew", "--cellar"): (0, f"{cellar}\n", "")}
        )
        subject.phase_trust()
        phase = subject.phases["trust"]
        self.assertEqual(phase.status, "passed")
        install_phase = subject.phase("install-skillmount-alone")
        self.assertTrue(subject.install(install_phase, "skillmount"))
        self.assertEqual(
            gateway.calls,
            [
                ("brew", "trust", "--json", harness.TRUST_JSON_VERSION),
                ("brew", "trust", "--formula", acceptance_reference("skillmount")),
                ("brew", "trust", "--formula", acceptance_reference("skillmount-asm")),
                ("brew", "trust", "--json", harness.TRUST_JSON_VERSION),
                ("brew", "trust", "--json", harness.TRUST_JSON_VERSION),
                ("brew", "trust", "--formula", acceptance_reference("skillmount")),
                ("brew", "install", "--formula", acceptance_reference("skillmount")),
                ("brew", "--cellar", acceptance_reference("skillmount")),
            ],
        )
        self.assertEqual(
            subject.added_trust,
            [acceptance_reference(package_id) for package_id in harness.FORMULA_IDS],
        )
        recorded = subject.trust_report()
        self.assertEqual(
            recorded["argv"],
            [list(call) for call in gateway.calls if call[:2] == ("brew", "trust")],
        )
        self.assertEqual(recorded["dropped"], [])
        self.assertEqual(recorded["store"]["path"], str(gateway.path))
        self.assertTrue(recorded["store"]["existed"])
        self.assertEqual(recorded["trusted"], [trusted_name(name) for name in harness.FORMULA_IDS])
        self.assertIn("brew untrust --formula", str(recorded["restore"]))
        self.assertIn("re-asserted", str(recorded["reassert"]))
        joined = " ".join(phase.observations)
        self.assertIn("keys trust by name", joined)
        self.assertIn("scoped to the install path", joined)
        self.assertIn("one `brew trust --formula` per run is not enough", joined)
        self.assertIn(
            "still lists skillmount-acceptance/tap/skillmount immediately before installing it",
            " ".join(install_phase.observations),
        )

    def test_every_install_reasserts_trust_even_after_an_uninstall(self) -> None:
        """Re-trust the reference immediately before every install, reinstalls included."""

        cellar = self.root / "Cellar" / "skillmount"
        gateway, subject = self.trust_harness(
            {
                ("brew", "install"): (0, "", ""),
                ("brew", "--cellar"): (0, f"{cellar}\n", ""),
                ("brew", "uninstall"): (0, "", ""),
            }
        )
        subject.phase_trust()
        reference = acceptance_reference("skillmount")
        self.assertTrue(subject.install(subject.phase("install-skillmount-alone"), "skillmount"))
        subject.uninstall("skillmount", phase=subject.phase("uninstall"))
        self.assertTrue(subject.install(subject.phase("co-install"), "skillmount"))
        installs = [
            index
            for index, call in enumerate(gateway.calls)
            if call[:2] == ("brew", "install")
        ]
        asserted = [
            index
            for index, call in enumerate(gateway.calls)
            if call == ("brew", "trust", "--formula", reference)
        ]
        self.assertEqual(len(installs), 2)
        self.assertEqual(len(asserted), 3)
        for index in installs:
            self.assertIn(index - 1, asserted)
            self.assertEqual(gateway.calls[index - 2][:3], ("brew", "trust", "--json"))
        self.assertEqual(
            subject.added_trust,
            [acceptance_reference(package_id) for package_id in harness.FORMULA_IDS],
        )

    def test_a_dropped_trust_entry_is_re_trusted_and_diagnosed(self) -> None:
        """Name the reference, the phase, and the last phase that ran when trust vanished."""

        cellar = self.root / "Cellar" / "skillmount"
        gateway, subject = self.trust_harness(
            {("brew", "install"): (0, "", ""), ("brew", "--cellar"): (0, f"{cellar}\n", "")}
        )
        subject.phase_trust()
        name = trusted_name("skillmount")
        gateway.store["formulae"] = [
            entry for entry in gateway.store["formulae"] if entry != name
        ]
        gateway.write()
        subject.phase("uninstall")
        phase = subject.phase("co-install")
        self.assertTrue(subject.install(phase, "skillmount"))
        self.assertIn(name, gateway.store["formulae"])
        diagnosis = phase.observations[0]
        self.assertIn(f"Homebrew dropped {name}", diagnosis)
        self.assertIn("in phase 'co-install'", diagnosis)
        self.assertIn("the last phase that ran was uninstall", diagnosis)
        self.assertIn(f'"{harness.trust_refusal(acceptance_reference("skillmount"))}"', diagnosis)
        phase.settle()
        self.assertEqual(phase.status, "passed")
        self.assertEqual(phase.findings, [])
        self.assertEqual(subject.trust_report()["dropped"], [diagnosis])
        self.assertEqual(
            subject.added_trust,
            [acceptance_reference(package_id) for package_id in harness.FORMULA_IDS],
        )

    def test_three_reasserts_still_untrust_each_reference_exactly_once(self) -> None:
        """Keep cleanup narrow however often trust was re-asserted."""

        cellar = self.root / "Cellar" / "skillmount"
        gateway, subject = self.trust_harness(
            {
                ("brew", "install"): (0, "", ""),
                ("brew", "--cellar"): (0, f"{cellar}\n", ""),
                ("brew", "uninstall"): (0, "", ""),
            }
        )
        before = gateway.path.read_bytes()
        subject.phase_trust()
        reference = acceptance_reference("skillmount")
        for name in ("install-skillmount-alone", "co-install", "upgrade-from-prior"):
            self.assertTrue(subject.install(subject.phase(name), "skillmount"))
            subject.uninstall("skillmount")
        self.assertEqual(
            [call for call in gateway.calls if call[:3] == ("brew", "trust", "--formula")].count(
                ("brew", "trust", "--formula", reference)
            ),
            4,
        )
        self.assertEqual(
            subject.added_trust,
            [acceptance_reference(package_id) for package_id in harness.FORMULA_IDS],
        )
        subject.cleanup()
        self.assertEqual(
            [call for call in gateway.calls if call[:2] == ("brew", "untrust")],
            [
                ("brew", "untrust", "--formula", acceptance_reference("skillmount-asm")),
                ("brew", "untrust", "--formula", reference),
            ],
        )
        self.assertEqual(gateway.path.read_bytes(), before)
        self.assertEqual(subject.cleanup_findings, [])

    def test_a_pre_existing_trust_entry_survives_every_reassert(self) -> None:
        """Leave an operator's own trust entry untouched however often it is re-asserted."""

        already = trusted_name("skillmount")
        cellar = self.root / "Cellar" / "skillmount"
        gateway, subject = self.trust_harness(
            {
                ("brew", "install"): (0, "", ""),
                ("brew", "--cellar"): (0, f"{cellar}\n", ""),
                ("brew", "uninstall"): (0, "", ""),
            },
            formulae=(already,),
        )
        before = gateway.path.read_bytes()
        subject.phase_trust()
        for name in ("install-skillmount-alone", "co-install"):
            self.assertTrue(subject.install(subject.phase(name), "skillmount"))
            subject.uninstall("skillmount")
        self.assertEqual(subject.added_trust, [acceptance_reference("skillmount-asm")])
        subject.cleanup()
        self.assertEqual(
            [call for call in gateway.calls if call[:2] == ("brew", "untrust")],
            [("brew", "untrust", "--formula", acceptance_reference("skillmount-asm"))],
        )
        self.assertIn(already, gateway.store["formulae"])
        self.assertEqual(gateway.path.read_bytes(), before)
        self.assertEqual(subject.cleanup_findings, [])

    def test_a_refused_reassert_fails_the_install_phase_with_the_refusal_quoted(self) -> None:
        """Fail the install phase by name when the pre-install `brew trust` is refused."""

        gateway, subject = self.trust_harness({("brew", "install"): (0, "", "")})
        subject.phase_trust()
        self.assertEqual(subject.phases["trust"].status, "passed")
        gateway.trust_status = 1
        reference = acceptance_reference("skillmount")
        phase = subject.phase("co-install")
        self.assertFalse(subject.install(phase, "skillmount"))
        self.assertNotIn(
            ("brew", "install", "--formula", reference), gateway.calls
        )
        phase.settle()
        self.assertEqual(phase.status, "failed")
        finding = " ".join(phase.findings)
        self.assertIn(f"brew trust --formula {reference} exited 1", finding)
        self.assertIn(
            "Refusing to load formula skillmount-acceptance/tap/skillmount from untrusted tap",
            finding,
        )
        self.assertIn("brew trust --help", finding)

    def test_an_install_refused_after_a_successful_reassert_is_its_own_failure(self) -> None:
        """Report a refusal that survived a re-assert as a distinct trust-model regression."""

        reference = acceptance_reference("skillmount")
        gateway, subject = self.trust_harness(
            {("brew", "install"): (1, "", f"Error: {harness.trust_refusal(reference)}\n")}
        )
        subject.phase_trust()
        phase = subject.phase("co-install")
        self.assertFalse(subject.install(phase, "skillmount"))
        self.assertEqual(
            gateway.calls[-1],
            ("brew", "install", "--formula", reference),
        )
        self.assertEqual(len(phase.findings), 2)
        self.assertNotIn("trust model changed", phase.findings[0])
        regression = phase.findings[1]
        self.assertIn(
            f"`brew trust --formula {reference}` succeeded immediately before", regression
        )
        self.assertIn(f'"{harness.trust_refusal(reference)}"', regression)
        self.assertIn("trust model changed again upstream", regression)

    def test_an_unreadable_pre_install_trust_state_refuses_the_install(self) -> None:
        """Refuse to install a reference whose current trust state cannot be read."""

        gateway, subject = self.trust_harness({("brew", "install"): (0, "", "")})
        subject.phase_trust()
        original = gateway.answer

        def answer(argv: tuple[str, ...]) -> harness.CommandEvidence:
            if argv[:3] == ("brew", "trust", "--json"):
                gateway.calls.append(argv)
                return harness.CommandEvidence(
                    argv=argv,
                    returncode=1,
                    stdout="",
                    stderr="Error: unknown command: trust\n",
                )
            return original(argv)

        gateway.answer = answer
        reference = acceptance_reference("skillmount")
        phase = subject.phase("install-skillmount-alone")
        self.assertFalse(subject.install(phase, "skillmount"))
        self.assertNotIn(
            ("brew", "install", "--formula", reference), gateway.calls
        )
        phase.settle()
        self.assertEqual(phase.status, "failed")
        finding = " ".join(phase.findings)
        self.assertIn(f"could not be read immediately before installing {reference}", finding)
        self.assertIn("in phase 'install-skillmount-alone'", finding)
        self.assertIn("unknown command: trust", finding)

    def test_a_refused_trust_quotes_homebrews_refusal_and_blocks_the_install(self) -> None:
        """Fail the trust phase by name and refuse to attempt any install."""

        gateway, subject = self.trust_harness(trust_status=1)
        subject.phase_trust()
        phase = subject.phases["trust"]
        self.assertEqual(phase.status, "failed")
        joined = " ".join(phase.findings)
        self.assertIn(
            "Refusing to load formula skillmount-acceptance/tap/skillmount from untrusted tap",
            joined,
        )
        self.assertIn("brew trust --help", joined)
        self.assertIn("--formula", joined)
        self.assertEqual(subject.trusted, [])
        self.assertEqual(subject.added_trust, [])
        install_phase = subject.phase("install-skillmount-alone")
        self.assertFalse(subject.install(install_phase, "skillmount"))
        self.assertNotIn(
            ("brew", "install", "--formula", acceptance_reference("skillmount")),
            gateway.calls,
        )
        self.assertIn("was never trusted", " ".join(install_phase.findings))

    def test_install_without_a_trust_phase_is_a_named_failure(self) -> None:
        """Refuse to install a reference no trust phase recorded."""

        gateway = ScriptedGateway({})
        subject = harness.Harness(gateway, options_for(self.root))
        subject.commands = dict(harness.PACKAGE_TABLE_PINS)
        phase = subject.phase("install-asm-alone")
        self.assertFalse(subject.install(phase, "skillmount-asm"))
        self.assertEqual(gateway.calls, [])
        finding = " ".join(phase.findings)
        self.assertIn("was never trusted", finding)
        self.assertIn("Refusing to load formula skillmount-acceptance/tap/skillmount-asm", finding)

    def test_an_unusable_trust_query_aborts_before_trusting_anything(self) -> None:
        """Refuse to trust anything without a readable prior trust state."""

        home = self.root / "home"
        home.mkdir()
        gateway = ScriptedGateway(
            {("brew", "trust", "--json"): (1, "", "Error: unknown command: trust\n")},
            environment={"HOME": str(home)},
        )
        subject = harness.Harness(gateway, options_for(self.root))
        subject.tap_root = self.root
        with self.assertRaises(harness.HomebrewAcceptanceError) as caught:
            subject.phase_trust()
        message = str(caught.exception)
        self.assertIn("brew trust --json v1", message)
        self.assertIn("unknown command: trust", message)
        self.assertEqual(len(gateway.calls), 1)
        self.assertIsNone(subject.trust_snapshot)
        subject.record_failure(caught.exception)
        self.assertEqual(subject.phases["trust"].status, "failed")

    def test_a_silently_unrecorded_trust_is_a_finding(self) -> None:
        """Fail when `brew trust` succeeds but records nothing Homebrew will load."""

        gateway, subject = self.trust_harness()

        def swallow(argv: tuple[str, ...]) -> harness.CommandEvidence:
            gateway.calls.append(argv)
            return harness.CommandEvidence(argv=argv, returncode=0, stdout="", stderr="")

        original = gateway.answer

        def answer(argv: tuple[str, ...]) -> harness.CommandEvidence:
            if argv[:3] == ("brew", "trust", "--formula"):
                return swallow(argv)
            return original(argv)

        gateway.answer = answer
        subject.phase_trust()
        phase = subject.phases["trust"]
        self.assertEqual(phase.status, "failed")
        joined = " ".join(phase.findings)
        self.assertIn("does not list skillmount-acceptance/tap/skillmount", joined)
        self.assertIn("Refusing to load formula", joined)

    def test_trust_store_bytes_are_restored_when_the_file_existed(self) -> None:
        """Leave an existing trust file byte-for-byte as it was found."""

        gateway, subject = self.trust_harness()
        before = gateway.path.read_bytes()
        subject.phase_trust()
        self.assertNotEqual(gateway.path.read_bytes(), before)
        subject.cleanup()
        self.assertEqual(gateway.path.read_bytes(), before)
        self.assertEqual(gateway.store["formulae"], list(TRUST_STORE["formulae"]))
        self.assertEqual(subject.cleanup_findings, [])
        self.assertEqual(subject.trust_evidence["restored"], "brew untrust --formula")
        self.assertEqual(
            [call for call in gateway.calls if call[:2] == ("brew", "untrust")],
            [
                ("brew", "untrust", "--formula", acceptance_reference("skillmount-asm")),
                ("brew", "untrust", "--formula", acceptance_reference("skillmount")),
            ],
        )

    def test_trust_store_is_removed_when_it_did_not_exist(self) -> None:
        """Remove a trust file this harness caused Homebrew to create."""

        gateway, subject = self.trust_harness(store_existed=False)
        self.assertFalse(gateway.path.exists())
        subject.phase_trust()
        self.assertTrue(gateway.path.exists())
        subject.cleanup()
        self.assertFalse(gateway.path.exists())
        self.assertEqual(subject.cleanup_findings, [])
        self.assertEqual(subject.trust_evidence["restored"], "trust file rewrite")

    def test_trust_is_restored_after_a_failed_lifecycle(self) -> None:
        """Restore the trust store even when the install that followed failed."""

        gateway, subject = self.trust_harness({("brew", "install"): (1, "", "install failed\n")})
        before = gateway.path.read_bytes()
        subject.phase_trust()
        phase = subject.phase("install-skillmount-alone")
        self.assertFalse(subject.install(phase, "skillmount"))
        self.assertNotIn("trust model changed", " ".join(phase.findings))
        subject.cleanup()
        self.assertEqual(gateway.path.read_bytes(), before)
        self.assertEqual(subject.cleanup_findings, [])

    def test_a_failed_untrust_falls_back_to_the_captured_bytes(self) -> None:
        """Rewrite the captured bytes when `brew untrust` could not remove an entry."""

        gateway, subject = self.trust_harness(untrust_status=1)
        before = gateway.path.read_bytes()
        subject.phase_trust()
        subject.cleanup()
        self.assertEqual(gateway.path.read_bytes(), before)
        self.assertEqual(subject.trust_evidence["restored"], "trust file rewrite")
        self.assertIn("could not untrust", " ".join(subject.cleanup_findings))

    def test_an_entry_the_harness_did_not_add_is_never_untrusted(self) -> None:
        """Leave a Formula the operator already trusted exactly as it was found."""

        already = trusted_name("skillmount")
        gateway, subject = self.trust_harness(formulae=(already,))
        before = gateway.path.read_bytes()
        subject.phase_trust()
        self.assertEqual(subject.phases["trust"].status, "passed")
        self.assertEqual(
            subject.trusted,
            [acceptance_reference(package_id) for package_id in harness.FORMULA_IDS],
        )
        self.assertEqual(subject.added_trust, [acceptance_reference("skillmount-asm")])
        self.assertIn(
            "was already trusted before this run", " ".join(subject.phases["trust"].observations)
        )
        subject.cleanup()
        self.assertEqual(
            [call for call in gateway.calls if call[:2] == ("brew", "untrust")],
            [("brew", "untrust", "--formula", acceptance_reference("skillmount-asm"))],
        )
        self.assertIn(already, gateway.store["formulae"])
        self.assertEqual(gateway.path.read_bytes(), before)

    def test_trust_drift_is_classified_in_both_directions(self) -> None:
        """Report a lost foreign entry and any addition this harness did not make."""

        reference = acceptance_reference("skillmount")
        before = {**TRUST_STORE, "formulae": ["hashicorp/tap/terraform"]}
        self.assertEqual(
            harness.trust_drift_findings(
                before,
                {**before, "formulae": ["hashicorp/tap/terraform", trusted_name("skillmount")]},
                added=(reference,),
            ),
            (),
        )
        lost = harness.trust_drift_findings(
            before, {**before, "formulae": [trusted_name("skillmount")]}, added=(reference,)
        )
        self.assertEqual(len(lost), 1)
        self.assertIn("hashicorp/tap/terraform", lost[0])
        self.assertIn("must never remove an entry it did not add", lost[0])
        gained = harness.trust_drift_findings(
            before,
            {**before, "formulae": ["hashicorp/tap/terraform", "stranger/tap/thing"]},
            added=(reference,),
        )
        self.assertEqual(len(gained), 1)
        self.assertIn("stranger/tap/thing", gained[0])
        widened = harness.trust_drift_findings(
            before, {**before, "taps": ["beeftornado/rmtree", "stranger/tap"]}, added=(reference,)
        )
        self.assertEqual(len(widened), 1)
        self.assertIn("'taps'", widened[0])

    def test_trust_json_is_parsed_strictly(self) -> None:
        """Fail closed on any `brew trust --json v1` shape this harness cannot restore."""

        parsed = harness.parse_trust_json(trust_json(formulae=["pashifika/tap/skillmount"]))
        self.assertEqual(parsed["formulae"], ("pashifika/tap/skillmount",))
        self.assertEqual(parsed["taps"], ("beeftornado/rmtree",))
        self.assertEqual(sorted(parsed), sorted(harness.TRUST_SECTIONS))
        for malformed in (
            "",
            "not json",
            "[]",
            '{"taps": [], "formulae": [], "casks": []}',
            '{"taps": [], "formulae": [], "casks": [], "commands": [], "extra": []}',
            '{"taps": [], "formulae": {}, "casks": [], "commands": []}',
            '{"taps": [], "formulae": [1], "casks": [], "commands": []}',
        ):
            with self.subTest(malformed=malformed):
                with self.assertRaises(harness.HomebrewAcceptanceError) as caught:
                    harness.parse_trust_json(malformed)
                self.assertIn("brew trust --json v1", str(caught.exception))

    def test_a_surviving_tap_is_a_cleanup_failure(self) -> None:
        """Treat a tap still registered after `brew untap` as a reported leak."""

        gateway, subject = self.trust_harness(
            {
                ("brew", "untap"): (0, "Untapped 2 formulae\n", ""),
                ("brew", "tap"): (0, "homebrew/core\nskillmount-acceptance/tap\n", ""),
            }
        )
        subject.tapped = True
        subject.cleanup()
        finding = " ".join(subject.cleanup_findings)
        self.assertIn("skillmount-acceptance/tap", finding)
        self.assertIn("does not deregister a tap", finding)
        self.assertIn(("brew", "untap", harness.ACCEPTANCE_TAP), gateway.calls)
        self.assertIn(("brew", "tap"), gateway.calls)

    def test_an_untapped_tap_leaves_no_cleanup_finding(self) -> None:
        """Accept cleanup when `brew tap` no longer lists the disposable tap."""

        gateway, subject = self.trust_harness(
            {
                ("brew", "untap"): (0, "Untapped 2 formulae\n", ""),
                ("brew", "tap"): (0, "homebrew/core\nhomebrew/cask\n", ""),
            }
        )
        subject.tapped = True
        subject.cleanup()
        self.assertEqual(subject.cleanup_findings, [])
        self.assertFalse(subject.tapped)
        self.assertEqual(harness.parse_tap_list("b/a\na/b\nb/a\n"), ("a/b", "b/a"))

    def test_rendered_templates_keep_the_binary_formula_shape(self) -> None:
        """Pin the tracked templates to the binary Formula shape accepted by Homebrew."""

        directory = Path(__file__).resolve().parents[2] / "packaging" / "homebrew"
        templates = sorted(directory.glob("*.rb.in"))
        if not templates:
            raise unittest.SkipTest(f"{directory} is not in this checkout")
        self.assertEqual(len(templates), len(harness.FORMULA_IDS))
        for template in templates:
            with self.subTest(template=template.name):
                text = template.read_text(encoding="utf-8")
                self.assertIn(
                    "  depends_on arch: :arm64\n  depends_on :macos\n",
                    text,
                )
                self.assertNotIn('depends_on "rust"', text)
                self.assertNotIn('system "cargo"', text)
                self.assertIn('    bin.install "@COMMAND@"\n', text)
                self.assertIn(
                    '    pkgshare.install "LICENSE-APACHE", "LICENSE-MIT", "VERSION"\n',
                    text,
                )
                self.assertIn(
                    '    generate_completions_from_executable(bin/"@COMMAND@", "completions", '
                    'base_name: "@COMMAND@")\n',
                    text,
                )
                self.assertEqual(text.count("generate_completions_from_executable"), 1)
                self.assertNotIn("shells:", text)
                self.assertIn('    refute_path_exists bin/"@OTHER_COMMAND@"\n', text)
                self.assertIn("    ].each do |completion, shell|\n", text)
                self.assertIn("      assert_path_exists completion", text)
                self.assertNotIn("assert !", text)
        fixture = FORMULA_TEMPLATE.format(digest=DIGEST)
        self.assertIn(
            "  depends_on arch: :arm64\n  depends_on :macos\n",
            fixture,
        )
        self.assertNotIn("cargo", fixture)
        self.assertIn('    bin.install "skillmount"\n', fixture)
        self.assertEqual(harness.platform_findings(fixture, formula_class="skillmount"), ())


if __name__ == "__main__":
    # Homebrew exists only on macOS and Linux, and these fixtures assert POSIX Homebrew paths:
    # `/opt/homebrew` is not absolute under Windows path semantics, and a Windows short name such
    # as `RUNNER~1` breaks keg-containment comparison. Refusing to run here is honest, because the
    # real coverage runs on the macOS and Linux jobs that can actually reach `brew`.
    if os.name != "posix":
        print(
            f"skipping: the Homebrew acceptance harness is POSIX-only; observed os.name="
            f"{os.name!r}. Run this suite on macOS or Linux.",
            file=sys.stderr,
        )
        raise SystemExit(0)
    unittest.main()
