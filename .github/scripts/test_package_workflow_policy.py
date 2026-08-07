#!/usr/bin/env python3
"""Parser and policy tests for the workflow governance the package channels depend on."""

from __future__ import annotations

import contextlib
import io
import os
import re
import subprocess
import tempfile
import unittest
from pathlib import Path

import package_workflow_policy as governance

WORKFLOW_DIRECTORY = Path(__file__).resolve().parent.parent / "workflows"
GOVERNED_WORKFLOWS = (
    "package.yml",
    "release.yml",
    "ci.yml",
    "live-agent-smoke.yml",
)
CHECKOUT_SHA = "a" * 40
UPLOAD_SHA = "b" * 40
CACHE_SHA = "c" * 40
CI_WORKFLOW = "ci.yml"
CI_CLASSIFY_JOB = "branch-policy"
CI_GATE_JOB = "gate"
CI_ACCEPTANCE_JOBS = ("package-homebrew-macos", "package-chocolatey-windows")
CI_ACCEPTANCE_OUTPUT = "package-acceptance"
CI_ACCEPTANCE_EXPRESSION = (
    "github.base_ref == 'main' || github.event_name == 'workflow_dispatch'"
)
CI_ACCEPTANCE_CONDITION = (
    f"needs.{CI_CLASSIFY_JOB}.outputs.{CI_ACCEPTANCE_OUTPUT} == 'true'"
)

# A minimal workflow that satisfies all twelve policies. Every policy test mutates exactly one
# construct here, so a mutation that trips the wrong policy or none at all is a test failure.
COMPLIANT = f"""\
name: Package channels

on:
  workflow_run:
    workflows: ["Release"]
    types: [completed]
  workflow_dispatch:
    inputs:
      tag:
        description: Published release tag
        required: true
        type: string
      channels:
        description: Channels to exercise
        required: true
        default: both
        type: choice
        options:
          - both
          - homebrew
          - chocolatey
      verification_only:
        description: Verify without publishing
        required: true
        default: true
        type: boolean

permissions:
  contents: read

jobs:
  preflight:
    name: preflight
    runs-on: ubuntu-24.04
    permissions:
      actions: read
      contents: read
    outputs:
      version: ${{{{ steps.preflight.outputs.version }}}}
      verification_only: ${{{{ steps.preflight.outputs.verification_only }}}}
    steps:
      - name: Check out the default branch
        uses: actions/checkout@{CHECKOUT_SHA} # v7.0.1
        with:
          fetch-depth: 1
          persist-credentials: false
      - name: Verify the published release
        id: preflight
        env:
          GH_TOKEN: ${{{{ secrets.GITHUB_TOKEN }}}}
          WORKFLOW_RUN_JSON: ${{{{ toJSON(github.event.workflow_run) }}}}
        run: python -B .github/scripts/package_channels.py preflight
      - name: Upload the validated package inputs
        uses: actions/upload-artifact@{UPLOAD_SHA} # v7.0.1
        with:
          name: package-inputs
          if-no-files-found: error

  generate:
    name: generate
    needs: preflight
    runs-on: ubuntu-24.04
    permissions:
      contents: read
    steps:
      - name: Check out for generation
        uses: actions/checkout@{CHECKOUT_SHA} # v7.0.1
        with:
          fetch-depth: 0
          persist-credentials: false
      - name: Render both channels
        run: python -B .github/scripts/package_channels.py generate-homebrew

  homebrew-acceptance:
    name: homebrew-acceptance
    needs: preflight
    runs-on: macos-15
    permissions:
      contents: read
    steps:
      - name: Exercise the native Homebrew lifecycle
        run: python3 -B .github/scripts/homebrew_acceptance.py

  chocolatey-acceptance:
    name: chocolatey-acceptance
    needs: preflight
    runs-on: windows-2025
    permissions:
      contents: read
    steps:
      - name: Exercise the native Chocolatey lifecycle
        run: python -B .github/scripts/chocolatey_acceptance.py

  homebrew-publish:
    name: homebrew-publish
    needs:
      - preflight
      - generate
      - homebrew-acceptance
    if: needs.preflight.outputs.verification_only != 'true'
    runs-on: ubuntu-24.04
    environment: homebrew
    permissions:
      contents: read
    concurrency:
      group: package-homebrew-${{{{ needs.preflight.outputs.version }}}}
      cancel-in-progress: false
    steps:
      - name: Reconcile the tap pull request
        env:
          CLIENT_ID: ${{{{ vars.HOMEBREW_TAP_APP_CLIENT_ID }}}}
        run: python -B .github/scripts/package_publish.py homebrew

  chocolatey-publish:
    name: chocolatey-publish
    needs:
      - preflight
      - generate
      - chocolatey-acceptance
    if: needs.preflight.outputs.verification_only != 'true'
    runs-on: windows-2025
    environment: chocolatey
    permissions:
      contents: read
    concurrency:
      group: package-chocolatey-${{{{ needs.preflight.outputs.version }}}}
      cancel-in-progress: false
    steps:
      - name: Reconcile the community repository
        env:
          API_KEY: ${{{{ secrets.CHOCOLATEY_API_KEY }}}}
        run: python -B .github/scripts/package_publish.py chocolatey

  summary:
    name: summary
    if: ${{{{ always() }}}}
    needs:
      - preflight
      - generate
      - homebrew-acceptance
      - chocolatey-acceptance
      - homebrew-publish
      - chocolatey-publish
    runs-on: ubuntu-24.04
    steps:
      - name: Report every entry
        env:
          PREFLIGHT_RESULT: ${{{{ needs.preflight.result }}}}
        run: echo "$PREFLIGHT_RESULT"
"""

# original text -> (replacement text, occurrences, policy numbers the mutation must trip)
MUTATIONS: dict[str, tuple[str, str, int, frozenset[int]]] = {
    "unpinned-action": (
        f"actions/upload-artifact@{UPLOAD_SHA} # v7.0.1",
        "actions/upload-artifact@v7",
        1,
        frozenset({1}),
    ),
    "action-without-version-comment": (
        f"actions/upload-artifact@{UPLOAD_SHA} # v7.0.1",
        f"actions/upload-artifact@{UPLOAD_SHA}",
        1,
        frozenset({1}),
    ),
    "action-with-non-version-comment": (
        f"actions/upload-artifact@{UPLOAD_SHA} # v7.0.1",
        f"actions/upload-artifact@{UPLOAD_SHA} # pinned",
        1,
        frozenset({1}),
    ),
    "cache-action": (
        f"actions/upload-artifact@{UPLOAD_SHA} # v7.0.1",
        f"actions/cache@{CACHE_SHA} # v4.2.0",
        1,
        frozenset({2}),
    ),
    "cache-key": (
        "          fetch-depth: 1\n",
        "          fetch-depth: 1\n          cache: pip\n",
        1,
        frozenset({2}),
    ),
    "save-always-key": (
        "          fetch-depth: 1\n",
        "          fetch-depth: 1\n          save-always: true\n",
        1,
        frozenset({2}),
    ),
    "checkout-ref": (
        "          fetch-depth: 1\n",
        "          fetch-depth: 1\n          ref: main\n",
        1,
        frozenset({3}),
    ),
    "checkout-persists-credentials": (
        "          fetch-depth: 1\n          persist-credentials: false\n",
        "          fetch-depth: 1\n",
        1,
        frozenset({3}),
    ),
    "foreign-secret-outside-publish": (
        "GH_TOKEN: ${{ secrets.GITHUB_TOKEN }}",
        "GH_TOKEN: ${{ secrets.HOMEBREW_TAP_APP_PRIVATE_KEY }}",
        1,
        frozenset({4}),
    ),
    "chocolatey-secret-in-homebrew-publish": (
        "CLIENT_ID: ${{ vars.HOMEBREW_TAP_APP_CLIENT_ID }}",
        "CLIENT_ID: ${{ secrets.CHOCOLATEY_API_KEY }}",
        1,
        frozenset({4}),
    ),
    "homebrew-variable-outside-publish": (
        "GH_TOKEN: ${{ secrets.GITHUB_TOKEN }}",
        "GH_TOKEN: ${{ vars.HOMEBREW_TAP_APP_CLIENT_ID }}",
        1,
        frozenset({4}),
    ),
    "wrong-homebrew-variable-in-homebrew-publish": (
        "CLIENT_ID: ${{ vars.HOMEBREW_TAP_APP_CLIENT_ID }}",
        "CLIENT_ID: ${{ vars.HOMEBREW_TAP_APP_ID }}",
        1,
        frozenset({4}),
    ),
    "homebrew-variable-in-chocolatey-publish": (
        "API_KEY: ${{ secrets.CHOCOLATEY_API_KEY }}",
        "CLIENT_ID: ${{ vars.HOMEBREW_TAP_APP_CLIENT_ID }}",
        1,
        frozenset({4}),
    ),
    "homebrew-secret-in-chocolatey-publish": (
        "API_KEY: ${{ secrets.CHOCOLATEY_API_KEY }}",
        "API_KEY: ${{ secrets.HOMEBREW_TAP_APP_PRIVATE_KEY }}",
        1,
        frozenset({4}),
    ),
    "wrong-environment-name": (
        "    environment: homebrew\n",
        "    environment: production\n",
        1,
        frozenset({5}),
    ),
    "environment-on-a-non-publish-job": (
        "  generate:\n    name: generate\n",
        "  generate:\n    name: generate\n    environment: staging\n",
        1,
        frozenset({5}),
    ),
    "publish-jobs-chained": (
        "      - homebrew-acceptance\n"
        "    if: needs.preflight.outputs.verification_only != 'true'\n"
        "    runs-on: ubuntu-24.04\n",
        "      - homebrew-acceptance\n"
        "      - chocolatey-publish\n"
        "    if: needs.preflight.outputs.verification_only != 'true'\n"
        "    runs-on: ubuntu-24.04\n",
        1,
        frozenset({6}),
    ),
    "publish-cancels-in-progress": (
        "      group: package-homebrew-${{ needs.preflight.outputs.version }}\n"
        "      cancel-in-progress: false\n",
        "      group: package-homebrew-${{ needs.preflight.outputs.version }}\n"
        "      cancel-in-progress: true\n",
        1,
        frozenset({7}),
    ),
    "publish-concurrency-ignores-version": (
        "      group: package-chocolatey-${{ needs.preflight.outputs.version }}\n",
        "      group: package-chocolatey\n",
        1,
        frozenset({7}),
    ),
    "publish-concurrency-missing": (
        "    concurrency:\n"
        "      group: package-chocolatey-${{ needs.preflight.outputs.version }}\n"
        "      cancel-in-progress: false\n",
        "",
        1,
        frozenset({7}),
    ),
    "publish-guard-ignores-verification-only": (
        "    if: needs.preflight.outputs.verification_only != 'true'\n"
        "    runs-on: windows-2025\n",
        "    if: github.event_name == 'push'\n    runs-on: windows-2025\n",
        1,
        frozenset({8}),
    ),
    "job-grants-contents-write": (
        "  generate:\n    name: generate\n    needs: preflight\n"
        "    runs-on: ubuntu-24.04\n    permissions:\n      contents: read\n",
        "  generate:\n    name: generate\n    needs: preflight\n"
        "    runs-on: ubuntu-24.04\n    permissions:\n      contents: write\n",
        1,
        frozenset({9}),
    ),
    "job-grants-id-token-write": (
        "  homebrew-acceptance:\n    name: homebrew-acceptance\n    needs: preflight\n"
        "    runs-on: macos-15\n    permissions:\n      contents: read\n",
        "  homebrew-acceptance:\n    name: homebrew-acceptance\n    needs: preflight\n"
        "    runs-on: macos-15\n    permissions:\n      contents: read\n"
        "      id-token: write\n",
        1,
        frozenset({9}),
    ),
    "workflow-grants-write-all": (
        "permissions:\n  contents: read\n\njobs:\n",
        "permissions: write-all\n\njobs:\n",
        1,
        frozenset({9}),
    ),
    "preflight-over-permissioned": (
        "    permissions:\n      actions: read\n      contents: read\n",
        "    permissions:\n      actions: read\n      contents: read\n      packages: read\n",
        1,
        frozenset({9}),
    ),
    "workflow-run-head-outside-preflight": (
        "        run: python -B .github/scripts/package_channels.py generate-homebrew\n",
        "        env:\n"
        "          HEAD: ${{ github.event.workflow_run.head_sha }}\n"
        "        run: python -B .github/scripts/package_channels.py generate-homebrew\n",
        1,
        frozenset({10}),
    ),
    "workflow-run-head-on-checkout": (
        "          fetch-depth: 1\n",
        "          fetch-depth: 1\n"
        "          ref: ${{ github.event.workflow_run.head_branch }}\n",
        1,
        frozenset({3, 10}),
    ),
    "publish-extracts-an-archive": (
        "        run: python -B .github/scripts/package_publish.py homebrew\n",
        "        run: tar -xf candidates.tar\n",
        1,
        frozenset({11}),
    ),
    "publish-runs-an-acceptance-harness": (
        "        run: python -B .github/scripts/package_publish.py chocolatey\n",
        "        run: python -B .github/scripts/chocolatey_acceptance.py\n",
        1,
        frozenset({11}),
    ),
    "publish-executes-a-local-path": (
        "        run: python -B .github/scripts/package_publish.py homebrew\n",
        "        run: ./publish.sh\n",
        1,
        frozenset({11}),
    ),
    "wrong-upstream-workflow": (
        '    workflows: ["Release"]\n',
        '    workflows: ["CI"]\n',
        1,
        frozenset({12}),
    ),
    "missing-workflow-run-trigger": (
        '  workflow_run:\n    workflows: ["Release"]\n    types: [completed]\n',
        "",
        1,
        frozenset({12}),
    ),
    "verification-defaults-to-publishing": (
        "        required: true\n        default: true\n        type: boolean\n",
        "        required: true\n        default: false\n        type: boolean\n",
        1,
        frozenset({12}),
    ),
    "verification-input-is-not-boolean": (
        "        required: true\n        default: true\n        type: boolean\n",
        "        required: true\n        default: true\n        type: string\n",
        1,
        frozenset({12}),
    ),
    "missing-channels-input": (
        "      channels:\n        description: Channels to exercise\n"
        "        required: true\n        default: both\n        type: choice\n"
        "        options:\n          - both\n          - homebrew\n          - chocolatey\n",
        "",
        1,
        frozenset({12}),
    ),
}


def tripped_policies(text: str) -> frozenset[int]:
    """Return the set of policy numbers a workflow source trips."""

    document = governance.parse_yaml(text)
    return frozenset(
        governance.policy_number(message)
        for message in governance.check_workflow(document, text)
    )


class YamlSubsetTests(unittest.TestCase):
    """Cover every construct the minimal YAML subset supports and every one it refuses."""

    def test_nested_block_mappings(self) -> None:
        """Parse two-space-indented nested block mappings."""

        document = governance.parse_yaml("a:\n  b:\n    c: d\ne: f\n")
        self.assertEqual(document, {"a": {"b": {"c": "d"}}, "e": "f"})

    def test_block_sequence_of_scalars(self) -> None:
        """Parse a block sequence of plain and quoted scalars."""

        document = governance.parse_yaml('branches:\n  - main\n  - "dev/**"\n')
        self.assertEqual(document, {"branches": ["main", "dev/**"]})

    def test_block_sequence_at_the_key_indentation(self) -> None:
        """Accept a block sequence that is not indented past its own key."""

        document = governance.parse_yaml("options:\n- both\n- homebrew\n")
        self.assertEqual(document, {"options": ["both", "homebrew"]})

    def test_block_sequence_of_mappings(self) -> None:
        """Parse sequence entries whose first key shares the entry line."""

        document = governance.parse_yaml(
            "steps:\n"
            "  - name: first\n"
            "    run: one\n"
            "  - name: second\n"
            "    with:\n"
            "      key: value\n"
        )
        self.assertEqual(
            document,
            {
                "steps": [
                    {"name": "first", "run": "one"},
                    {"name": "second", "with": {"key": "value"}},
                ]
            },
        )

    def test_comments(self) -> None:
        """Ignore whole-line, indented, and trailing comments."""

        document = governance.parse_yaml(
            "# leading\n"
            "a: one # trailing\n"
            "b:\n"
            "  # indented\n"
            "  c: two\n"
        )
        self.assertEqual(document, {"a": "one", "b": {"c": "two"}})

    def test_scalar_typing(self) -> None:
        """Type plain booleans, integers, nulls, and strings, and never type a quoted scalar."""

        document = governance.parse_yaml(
            "yes: true\n"
            "no: false\n"
            "count: 7\n"
            "negative: -3\n"
            "nothing: null\n"
            "tilde: ~\n"
            "empty:\n"
            "text: 0.2.0\n"
            'quoted: "1"\n'
        )
        self.assertEqual(
            document,
            {
                "yes": True,
                "no": False,
                "count": 7,
                "negative": -3,
                "nothing": None,
                "tilde": None,
                "empty": None,
                "text": "0.2.0",
                "quoted": "1",
            },
        )

    def test_quoted_scalars(self) -> None:
        """Decode single-quoted doubling and the supported double-quoted escapes."""

        document = governance.parse_yaml(
            "single: 'it''s here'\n"
            'double: "a\\tb\\nc\\"d\\\\e"\n'
            'hash: "value # not a comment"\n'
        )
        self.assertEqual(
            document,
            {
                "single": "it's here",
                "double": 'a\tb\nc"d\\e',
                "hash": "value # not a comment",
            },
        )

    def test_literal_block_scalars(self) -> None:
        """Keep line breaks in a literal block scalar and honour strip chomping."""

        clipped = governance.parse_yaml("run: |\n  one\n  two\n")
        self.assertEqual(clipped, {"run": "one\ntwo\n"})
        stripped = governance.parse_yaml("run: |-\n  one\n  two\n")
        self.assertEqual(stripped, {"run": "one\ntwo"})

    def test_literal_block_scalar_keeps_comment_lines_and_blank_lines(self) -> None:
        """Treat a '#' line inside a literal block scalar as content, not as a comment."""

        document = governance.parse_yaml("run: |\n  one\n\n  # two\n  three\n")
        self.assertEqual(document, {"run": "one\n\n# two\nthree\n"})

    def test_folded_block_scalars(self) -> None:
        """Fold equally indented lines to spaces and paragraph breaks to newlines."""

        self.assertEqual(governance.parse_yaml("run: >-\n  one\n  two\n"), {"run": "one two"})
        self.assertEqual(governance.parse_yaml("run: >\n  one\n  two\n"), {"run": "one two\n"})
        self.assertEqual(
            governance.parse_yaml("run: >-\n  one\n\n  two\n"), {"run": "one\ntwo"}
        )
        self.assertEqual(
            governance.parse_yaml("run: >-\n  one\n\n\n  two\n"), {"run": "one\n\ntwo"}
        )

    def test_folded_block_scalar_keeps_more_indented_lines_literal(self) -> None:
        """Never fold a break adjacent to a more-indented line, exactly as YAML requires."""

        document = governance.parse_yaml(
            "if: >-\n  ${{\n    a &&\n    b\n  }}\n"
        )
        self.assertEqual(document, {"if": "${{\n  a &&\n  b\n}}"})

    def test_empty_flow_collections(self) -> None:
        """Parse empty flow mappings and empty flow sequences."""

        document = governance.parse_yaml("mapping: {}\nsequence: []\n")
        self.assertEqual(document, {"mapping": {}, "sequence": []})

    def test_inline_flow_collections(self) -> None:
        """Parse flat flow mappings and flow sequences of scalars."""

        document = governance.parse_yaml(
            'permissions: {actions: read, contents: read}\n'
            'workflows: ["Release", \'CI\']\n'
            "concurrency: {group: a-b, cancel-in-progress: false}\n"
        )
        self.assertEqual(
            document,
            {
                "permissions": {"actions": "read", "contents": "read"},
                "workflows": ["Release", "CI"],
                "concurrency": {"group": "a-b", "cancel-in-progress": False},
            },
        )

    def test_expression_scalars_survive_verbatim(self) -> None:
        """Keep a GitHub expression intact even though it contains braces, pipes, and quotes."""

        document = governance.parse_yaml(
            "group: ${{ github.workflow }}-${{ github.event.number || github.ref }}\n"
            "if: needs.preflight.outputs.verification_only != 'true'\n"
        )
        self.assertEqual(
            document,
            {
                "group": "${{ github.workflow }}-${{ github.event.number || github.ref }}",
                "if": "needs.preflight.outputs.verification_only != 'true'",
            },
        )

    def test_rejects_tabs(self) -> None:
        """Refuse a tab rather than guess its indentation width."""

        with self.assertRaises(governance.WorkflowPolicyError) as caught:
            governance.parse_yaml("a:\n\tb: c\n")
        self.assertIn("tab", str(caught.exception))

    def test_rejects_anchors_aliases_and_tags(self) -> None:
        """Refuse anchors, aliases, and tags instead of resolving them."""

        for source in (
            "a: &anchor value\nb: c\n",
            "a: value\nb: *anchor\n",
            'a: !!str value\n',
            "&anchor: value\n",
        ):
            with self.subTest(source=source):
                with self.assertRaises(governance.WorkflowPolicyError):
                    governance.parse_yaml(source)

    def test_rejects_merge_keys(self) -> None:
        """Refuse a merge key rather than silently merging a mapping."""

        with self.assertRaises(governance.WorkflowPolicyError) as caught:
            governance.parse_yaml("a:\n  <<: value\n")
        self.assertIn("<<", str(caught.exception))

    def test_rejects_duplicate_keys(self) -> None:
        """Refuse duplicate keys in block mappings and in flow mappings."""

        for source in ("a: one\na: two\n", "a: {b: one, b: two}\n", "a:\n  b: 1\n  b: 2\n"):
            with self.subTest(source=source):
                with self.assertRaises(governance.WorkflowPolicyError) as caught:
                    governance.parse_yaml(source)
                self.assertIn("duplicate", str(caught.exception))

    def test_rejects_document_markers(self) -> None:
        """Refuse explicit document markers, because the subset parses one document."""

        for source in ("---\na: b\n", "a: b\n...\n", "--- !tag\na: b\n"):
            with self.subTest(source=source):
                with self.assertRaises(governance.WorkflowPolicyError):
                    governance.parse_yaml(source)

    def test_rejects_odd_indentation(self) -> None:
        """Refuse indentation that is not a multiple of two."""

        with self.assertRaises(governance.WorkflowPolicyError) as caught:
            governance.parse_yaml("a:\n   b: c\n")
        self.assertIn("multiples of 2", str(caught.exception))

    def test_rejects_unexpected_indentation_jump(self) -> None:
        """Refuse a nested block that skips an indentation level."""

        with self.assertRaises(governance.WorkflowPolicyError):
            governance.parse_yaml("a:\n    b: c\n")

    def test_rejects_nested_flow_collections(self) -> None:
        """Refuse a flow collection inside a flow collection."""

        for source in ("a: {b: [c]}\n", "a: [[b]]\n"):
            with self.subTest(source=source):
                with self.assertRaises(governance.WorkflowPolicyError) as caught:
                    governance.parse_yaml(source)
                self.assertIn("flat flow", str(caught.exception))

    def test_rejects_malformed_flow_collections(self) -> None:
        """Refuse unclosed flow collections, empty entries, and non-entries."""

        for source in ("a: {b: c\n", "a: [b,, c]\n", "a: {b}\n", "a: [b\n"):
            with self.subTest(source=source):
                with self.assertRaises(governance.WorkflowPolicyError):
                    governance.parse_yaml(source)

    def test_rejects_unterminated_and_trailing_quoted_scalars(self) -> None:
        """Refuse an unclosed quote and refuse content after a closing quote."""

        for source in ('a: "unterminated\n', "a: 'value' trailing\n"):
            with self.subTest(source=source):
                with self.assertRaises(governance.WorkflowPolicyError):
                    governance.parse_yaml(source)

    def test_rejects_unsupported_double_quoted_escape(self) -> None:
        """Refuse an escape the subset does not implement instead of dropping it."""

        with self.assertRaises(governance.WorkflowPolicyError):
            governance.parse_yaml('a: "b\\x41c"\n')

    def test_rejects_ambiguous_plain_scalar(self) -> None:
        """Refuse a plain scalar containing ': ', which YAML would read as a nested mapping."""

        with self.assertRaises(governance.WorkflowPolicyError) as caught:
            governance.parse_yaml("a: b: c\n")
        self.assertIn("': '", str(caught.exception))

    def test_rejects_a_missing_key_separator(self) -> None:
        """Refuse a mapping line with no 'key:' separator."""

        for source in ("a:\n  b\n", "key:value\n"):
            with self.subTest(source=source):
                with self.assertRaises(governance.WorkflowPolicyError):
                    governance.parse_yaml(source)

    def test_rejects_unsupported_block_scalar_headers(self) -> None:
        """Refuse chomping and indentation indicators the subset does not implement."""

        for header in ("|+", ">+", "|2", ">-2"):
            with self.subTest(header=header):
                with self.assertRaises(governance.WorkflowPolicyError):
                    governance.parse_yaml(f"run: {header}\n  body\n")

    def test_rejects_a_non_mapping_or_empty_document(self) -> None:
        """Refuse a document that is not a single top-level mapping."""

        for source in ("- a\n- b\n", "", "# only a comment\n"):
            with self.subTest(source=source):
                with self.assertRaises(governance.WorkflowPolicyError):
                    governance.parse_yaml(source)


class GovernanceTests(unittest.TestCase):
    """Cover the per-file policy scoping and the allowance list."""

    def test_package_workflow_is_fully_governed(self) -> None:
        """Every numbered policy applies to the package workflow with no allowance."""

        record = governance.governance_for(governance.PACKAGE_WORKFLOW)
        self.assertEqual(record.policies, governance.ALL_POLICIES)
        self.assertEqual(dict(record.allowances), {})
        for number in range(1, governance.POLICY_COUNT + 1):
            self.assertTrue(record.applies(number))

    def test_release_allowances_are_narrow_and_explained(self) -> None:
        """The release workflow keeps policies 1 and 2 and documents the two it cannot hold."""

        record = governance.governance_for("release.yml")
        self.assertEqual(sorted(record.allowances), [3, 9])
        self.assertTrue(record.applies(1))
        self.assertTrue(record.applies(2))
        self.assertFalse(record.applies(3))
        self.assertFalse(record.applies(9))
        for reason in record.allowances.values():
            self.assertGreater(len(reason), 40)

    def test_other_workflows_carry_the_shared_policies_only(self) -> None:
        """A workflow with no publish jobs is governed by policies 1-3 and 9."""

        for name in ("ci.yml", "live-agent-smoke.yml", "some-future-workflow.yml"):
            with self.subTest(name=name):
                record = governance.governance_for(name)
                self.assertEqual(record.policies, governance.SHARED_POLICIES)
                self.assertEqual(dict(record.allowances), {})
                self.assertFalse(record.applies(12))

    def test_policy_number_extraction(self) -> None:
        """Read the policy number back out of a violation message, and reject a stray one."""

        self.assertEqual(governance.policy_number("policy 11: something"), 11)
        with self.assertRaises(governance.WorkflowPolicyError):
            governance.policy_number("something")

    def test_governed_violations_filters_by_file(self) -> None:
        """Drop a violation of a policy that does not govern the named file."""

        messages = ["policy 1: pin", "policy 3: ref", "policy 12: trigger"]
        self.assertEqual(
            governance.governed_violations("release.yml", messages), ["policy 1: pin"]
        )
        self.assertEqual(governance.governed_violations("package.yml", messages), messages)


class WorkflowPolicyTests(unittest.TestCase):
    """Cover each policy in both directions against one synthetic compliant workflow."""

    def test_compliant_workflow_has_no_violations(self) -> None:
        """The synthetic compliant workflow trips no policy at all."""

        document = governance.parse_yaml(COMPLIANT)
        self.assertEqual(governance.check_workflow(document, COMPLIANT), [])

    def test_compliant_workflow_declares_the_contracted_shape(self) -> None:
        """Guard the fixture itself, so a mutation test can never pass against a stub."""

        document = governance.parse_yaml(COMPLIANT)
        self.assertEqual(
            sorted(document["jobs"]),
            [
                "chocolatey-acceptance",
                "chocolatey-publish",
                "generate",
                "homebrew-acceptance",
                "homebrew-publish",
                "preflight",
                "summary",
            ],
        )

    def test_each_mutation_trips_exactly_its_policy(self) -> None:
        """Every targeted mutation trips the expected policies and nothing else."""

        for name, (original, replacement, count, expected) in MUTATIONS.items():
            with self.subTest(mutation=name):
                self.assertEqual(
                    COMPLIANT.count(original),
                    count,
                    f"mutation {name!r} expected {count} occurrence(s) of its anchor",
                )
                mutated = COMPLIANT.replace(original, replacement)
                self.assertNotEqual(mutated, COMPLIANT)
                self.assertEqual(tripped_policies(mutated), expected)

    def test_mutations_cover_every_policy(self) -> None:
        """Every numbered policy is exercised by at least one mutation."""

        covered: set[int] = set()
        for _, _, _, expected in MUTATIONS.values():
            covered |= expected
        self.assertEqual(covered, set(governance.ALL_POLICIES))

    def test_unpinned_message_names_expected_and_observed(self) -> None:
        """A violation message names both the expected form and the observed value."""

        original, replacement, _, _ = MUTATIONS["unpinned-action"]
        mutated = COMPLIANT.replace(original, replacement)
        messages = governance.check_workflow(governance.parse_yaml(mutated), mutated)
        self.assertEqual(len(messages), 1)
        self.assertIn("expected uses '<action>@<40-hex sha>'", messages[0])
        self.assertIn("actions/upload-artifact@v7", messages[0])


class RealWorkflowTests(unittest.TestCase):
    """Run the repository's own workflows through the checker."""

    def test_every_governed_workflow_is_clean(self) -> None:
        """Each real workflow parses and satisfies every policy that governs it."""

        for name in GOVERNED_WORKFLOWS:
            with self.subTest(workflow=name):
                path = WORKFLOW_DIRECTORY / name
                self.assertTrue(path.is_file(), f"expected {path} to exist")
                text = path.read_text(encoding="utf-8")
                violations = governance.check_workflow(governance.parse_yaml(text), text)
                self.assertEqual(governance.governed_violations(name, violations), [])

    def test_check_paths_reports_nothing_for_the_real_tree(self) -> None:
        """The path-level entry point reports no violation for the governed workflow set."""

        paths = [WORKFLOW_DIRECTORY / name for name in GOVERNED_WORKFLOWS]
        self.assertEqual(governance.check_paths(paths), [])

    def test_package_workflow_declares_the_contracted_jobs(self) -> None:
        """The real package workflow carries exactly the seven contracted jobs."""

        text = (WORKFLOW_DIRECTORY / "package.yml").read_text(encoding="utf-8")
        document = governance.parse_yaml(text)
        self.assertEqual(
            sorted(document["jobs"]),
            [
                "chocolatey-acceptance",
                "chocolatey-publish",
                "generate",
                "homebrew-acceptance",
                "homebrew-publish",
                "preflight",
                "summary",
            ],
        )

    def test_no_package_workflow_run_script_interpolates_an_expression(self) -> None:
        """Every value reaches a shell through env:, so no expansion can inject shell syntax."""

        text = (WORKFLOW_DIRECTORY / "package.yml").read_text(encoding="utf-8")
        document = governance.parse_yaml(text)
        offenders = [
            (name, step.get("name"))
            for name, job in document["jobs"].items()
            for step in job.get("steps") or []
            if isinstance(step.get("run"), str) and "${{" in step["run"]
        ]
        self.assertEqual(offenders, [])


def _job_needs(job: dict) -> list[str]:
    """Return a job's dependencies from either the scalar or the sequence form."""

    needs = job.get("needs")
    if needs is None:
        return []
    return list(needs) if isinstance(needs, list) else [needs]


class CiWorkflowContractTests(unittest.TestCase):
    """Pin the CI workflow's event topology and its package-acceptance wiring."""

    def setUp(self) -> None:
        self.text = (WORKFLOW_DIRECTORY / CI_WORKFLOW).read_text(encoding="utf-8")
        self.document = governance.parse_yaml(self.text)
        self.jobs = self.document["jobs"]

    def classifying_step(self) -> dict:
        """Return the branch-policy step the package-acceptance output is published from."""

        job = self.jobs[CI_CLASSIFY_JOB]
        reference = (job.get("outputs") or {}).get(CI_ACCEPTANCE_OUTPUT)
        pattern = (
            r"\$\{\{ steps\.([A-Za-z0-9_-]+)\.outputs\."
            + re.escape(CI_ACCEPTANCE_OUTPUT)
            + r" \}\}"
        )
        match = re.fullmatch(pattern, reference or "")
        self.assertIsNotNone(
            match,
            f"expected jobs.{CI_CLASSIFY_JOB}.outputs.{CI_ACCEPTANCE_OUTPUT} to name a step "
            f"output, observed {reference!r}",
        )
        assert match is not None
        steps = [step for step in job["steps"] if step.get("id") == match.group(1)]
        self.assertEqual(
            len(steps), 1, f"expected exactly one step with id {match.group(1)!r}"
        )
        return steps[0]

    def test_ci_runs_for_pull_requests_development_pushes_and_dispatch(self) -> None:
        """CI covers both pull-request bases, only development-line pushes, and dispatch."""

        triggers = self.document["on"]
        self.assertEqual(triggers["pull_request"]["branches"], ["main", "dev/**"])
        self.assertEqual(triggers["push"]["branches"], ["dev/**"])
        self.assertIn("workflow_dispatch", triggers)

    def test_branch_policy_publishes_one_package_acceptance_classification(self) -> None:
        """One step derives the classification through env: and publishes it as a job output."""

        step = self.classifying_step()
        script = step.get("run") or ""
        self.assertIn('>> "$GITHUB_OUTPUT"', script)
        binding = re.search(
            re.escape(CI_ACCEPTANCE_OUTPUT) + r"=\$([A-Za-z_][A-Za-z0-9_]*)", script
        )
        self.assertIsNotNone(
            binding, f"expected the classification to be published from env, observed {script!r}"
        )
        assert binding is not None
        environment = step.get("env") or {}
        self.assertEqual(
            environment.get(binding.group(1)),
            "${{ " + CI_ACCEPTANCE_EXPRESSION + " }}",
        )
        self.assertEqual(
            self.text.count(CI_ACCEPTANCE_EXPRESSION),
            1,
            "the classification expression must be written exactly once",
        )

    def test_both_package_acceptance_jobs_consume_the_classification(self) -> None:
        """Neither acceptance job runs unless the published classification includes it."""

        for name in CI_ACCEPTANCE_JOBS:
            with self.subTest(job=name):
                job = self.jobs[name]
                self.assertEqual(_job_needs(job), [CI_CLASSIFY_JOB])
                self.assertEqual(job.get("if"), CI_ACCEPTANCE_CONDITION)

    def test_gate_represents_every_predecessor_and_binds_the_classification(self) -> None:
        """The gate depends on every job, reads each result, and reads the classification."""

        gate = self.jobs[CI_GATE_JOB]
        needs = _job_needs(gate)
        self.assertEqual(
            sorted(needs),
            sorted(
                [
                    CI_CLASSIFY_JOB,
                    "quality",
                    "test",
                    "shell-completions-macos",
                    "shell-completions-windows",
                    *CI_ACCEPTANCE_JOBS,
                ]
            ),
        )
        steps = gate["steps"]
        self.assertEqual(len(steps), 1, "expected the gate to carry one aggregate step")
        environment = steps[0].get("env") or {}
        for name in needs:
            variable = name.upper().replace("-", "_") + "_RESULT"
            self.assertEqual(
                environment.get(variable), "${{ needs." + name + ".result }}"
            )
        self.assertEqual(
            environment.get("PACKAGE_ACCEPTANCE"),
            "${{ needs."
            + CI_CLASSIFY_JOB
            + ".outputs."
            + CI_ACCEPTANCE_OUTPUT
            + " }}",
        )

    def test_no_ci_run_script_interpolates_a_github_expression(self) -> None:
        """Event-controlled values reach a CI shell through env:, never through expansion."""

        offenders = [
            (name, step.get("name"))
            for name, job in self.jobs.items()
            for step in job.get("steps") or []
            if isinstance(step.get("run"), str) and "${{ github." in step["run"]
        ]
        self.assertEqual(offenders, [])


class CommandLineTests(unittest.TestCase):
    """Cover the check subcommand's exit status and reported output."""

    def invoke(self, *arguments: str) -> tuple[int, str, str]:
        """Run the command line, capturing its status, standard output, and diagnostics."""

        out = io.StringIO()
        err = io.StringIO()
        with contextlib.redirect_stdout(out), contextlib.redirect_stderr(err):
            status = governance.main(list(arguments))
        return status, out.getvalue(), err.getvalue()

    def check(self, text: str, *, file_name: str = "package.yml") -> tuple[int, str, str]:
        """Write *text* to a temporary workflow file and run the check subcommand on it."""

        with tempfile.TemporaryDirectory() as directory:
            path = Path(directory) / file_name
            path.write_text(text, encoding="utf-8")
            return self.invoke("check", str(path))

    def test_compliant_workflow_exits_zero(self) -> None:
        """A compliant workflow reports success and prints no violation."""

        status, out, err = self.check(COMPLIANT)
        self.assertEqual(status, 0)
        self.assertIn("1 workflow(s) satisfy every governing policy", out)
        self.assertNotIn("policy", err)

    def test_violating_workflow_exits_one_and_names_the_policy(self) -> None:
        """A single violation fails the check and is printed with its path and policy."""

        original, replacement, _, _ = MUTATIONS["publish-extracts-an-archive"]
        status, out, err = self.check(COMPLIANT.replace(original, replacement))
        self.assertEqual(status, 1)
        self.assertIn("policy 11: jobs.homebrew-publish", out)
        self.assertIn("1 workflow policy violation(s)", err)

    def test_allowance_applies_by_file_name(self) -> None:
        """The same violating text passes under a file name whose policy is allowed."""

        original, replacement, _, _ = MUTATIONS["checkout-ref"]
        mutated = COMPLIANT.replace(original, replacement)
        self.assertEqual(self.check(mutated, file_name="package.yml")[0], 1)
        self.assertEqual(self.check(mutated, file_name="release.yml")[0], 0)

    def test_unparsable_workflow_exits_one(self) -> None:
        """A workflow outside the supported subset fails instead of being half-checked."""

        status, _, err = self.check("jobs:\n\tbad: true\n")
        self.assertEqual(status, 1)
        self.assertIn("observed a tab character", err)

    def test_missing_workflow_exits_one(self) -> None:
        """A missing path fails with a stable status rather than a traceback."""

        with tempfile.TemporaryDirectory() as directory:
            status, _, err = self.invoke("check", str(Path(directory) / "absent.yml"))
        self.assertEqual(status, 1)
        self.assertIn("workflow policy check failed", err)

    def test_check_requires_at_least_one_path(self) -> None:
        """The parser refuses a check invocation with no workflow path."""

        with contextlib.redirect_stderr(io.StringIO()):
            with self.assertRaises(SystemExit):
                governance.argument_parser().parse_args(["check"])

    def test_real_tree_passes_through_the_command_line(self) -> None:
        """The documented acceptance invocation exits zero against the real workflows."""

        status, out, _ = self.invoke(
            "check", *(str(WORKFLOW_DIRECTORY / name) for name in GOVERNED_WORKFLOWS)
        )
        self.assertEqual(status, 0)
        self.assertIn("4 workflow(s) satisfy every governing policy", out)


class TemplateTokenTests(unittest.TestCase):
    """Guard the packaging templates this workflow renders against token drift."""

    PACKAGING = Path(__file__).resolve().parent.parent.parent / "packaging"
    TOKEN = re.compile(r"@([A-Z][A-Z0-9_]*)@")
    EXPECTED = {
        "homebrew/skillmount.rb.in": {
            "FORMULA_CLASS", "PACKAGE_ID", "DESCRIPTION", "HOMEPAGE", "ARCHIVE_URL",
            "ARCHIVE_SHA256", "VERSION", "LICENSE", "COMMAND", "OTHER_COMMAND", "TAG",
            "COMMIT",
        },
        "homebrew/skillmount-asm.rb.in": {
            "FORMULA_CLASS", "PACKAGE_ID", "DESCRIPTION", "HOMEPAGE", "ARCHIVE_URL",
            "ARCHIVE_SHA256", "VERSION", "LICENSE", "COMMAND", "OTHER_COMMAND", "TAG",
            "COMMIT",
        },
        "chocolatey/skillmount/skillmount.nuspec.in": {
            "PACKAGE_ID", "VERSION", "TITLE", "SUMMARY", "DESCRIPTION", "PROJECT_URL",
            "PROJECT_SOURCE_URL", "LICENSE_URL", "RELEASE_NOTES_URL", "COMMAND", "TAG",
        },
        "chocolatey/skillmount-asm/skillmount-asm.nuspec.in": {
            "PACKAGE_ID", "VERSION", "TITLE", "SUMMARY", "DESCRIPTION", "PROJECT_URL",
            "PROJECT_SOURCE_URL", "LICENSE_URL", "RELEASE_NOTES_URL", "COMMAND", "TAG",
        },
        "chocolatey/skillmount/tools/chocolateyinstall.ps1.in": {
            "PACKAGE_ID", "VERSION", "TAG", "COMMAND", "SELECTED_EXECUTABLE",
            "OTHER_EXECUTABLE", "URL_X86", "SHA256_X86", "URL_X64", "SHA256_X64",
            "ARCHIVE_ROOT_X86", "ARCHIVE_ROOT_X64",
        },
        "chocolatey/skillmount-asm/tools/chocolateyinstall.ps1.in": {
            "PACKAGE_ID", "VERSION", "TAG", "COMMAND", "SELECTED_EXECUTABLE",
            "OTHER_EXECUTABLE", "URL_X86", "SHA256_X86", "URL_X64", "SHA256_X64",
            "ARCHIVE_ROOT_X86", "ARCHIVE_ROOT_X64",
        },
    }

    def test_every_template_uses_exactly_its_contracted_tokens(self) -> None:
        """A template may use no other token and must use every token it is given."""

        for relative, expected in self.EXPECTED.items():
            with self.subTest(template=relative):
                path = self.PACKAGING / relative
                self.assertTrue(path.is_file(), f"expected {path} to exist")
                observed = set(self.TOKEN.findall(path.read_text(encoding="utf-8")))
                self.assertEqual(observed, expected)

    def test_paired_templates_are_identical(self) -> None:
        """Both members of a pair share one template, so only the tokens can differ."""

        pairs = (
            ("homebrew/skillmount.rb.in", "homebrew/skillmount-asm.rb.in"),
            (
                "chocolatey/skillmount/tools/chocolateyinstall.ps1.in",
                "chocolatey/skillmount-asm/tools/chocolateyinstall.ps1.in",
            ),
            (
                "chocolatey/skillmount/skillmount.nuspec.in",
                "chocolatey/skillmount-asm/skillmount-asm.nuspec.in",
            ),
        )
        for first, second in pairs:
            with self.subTest(pair=(first, second)):
                self.assertEqual(
                    (self.PACKAGING / first).read_bytes(),
                    (self.PACKAGING / second).read_bytes(),
                )

    def test_formula_templates_confine_the_paired_command_to_the_test_block(self) -> None:
        """The pair member's command may appear only inside 'test do', never in install."""

        text = (self.PACKAGING / "homebrew/skillmount.rb.in").read_text(encoding="utf-8")
        self.assertNotIn("conflicts_with", text)
        self.assertNotIn('depends_on "rust"', text)
        self.assertNotIn('system "cargo"', text)
        self.assertEqual(
            sorted(line.strip() for line in text.splitlines() if "depends_on" in line),
            ["depends_on :macos", "depends_on arch: :arm64"],
        )
        self.assertIn('bin.install "@COMMAND@"', text)
        before, _, test_block = text.partition("  test do")
        self.assertNotIn("@OTHER_COMMAND@", before)
        self.assertIn("@OTHER_COMMAND@", test_block)

    def test_tap_ci_exercises_binary_formulae_without_a_build_toolchain(self) -> None:
        """Keep tap validation on the same release-archive install path operators use."""

        text = (self.PACKAGING / "homebrew/tap-ci.yml").read_text(encoding="utf-8")
        self.assertNotIn("--build-from-source", text)
        self.assertNotIn("brew install --formula rust", text)
        self.assertNotIn("brew test --formula", text)
        self.assertIn('brew install --formula "$TAP/skillmount"', text)
        self.assertIn('brew install --formula "$TAP/skillmount-asm"', text)

    def test_install_templates_never_reference_the_unselected_executable_after_selection(
        self,
    ) -> None:
        """The unselected executable is validated and then never touched again."""

        text = (
            self.PACKAGING / "chocolatey/skillmount/tools/chocolateyinstall.ps1.in"
        ).read_text(encoding="utf-8")
        self.assertIn("$ErrorActionPreference = 'Stop'", text)
        self.assertIn("Set-StrictMode -Version 2", text)
        self.assertNotIn("Install-ChocolateyZipPackage", text)
        self.assertNotIn("Install-ChocolateyPath", text)
        self.assertNotIn("$PROFILE", text)
        self.assertNotIn(".ignore", text)
        selection = text.index("foreach ($retained in")
        self.assertLess(text.rindex("@OTHER_EXECUTABLE@"), selection)
        self.assertLess(text.rindex("$unselectedExecutable"), selection)


class TapWorkflowTests(unittest.TestCase):
    """Exercise the self-contained tap workflow's publication-state boundary."""

    WORKFLOW = (
        Path(__file__).resolve().parent.parent.parent / "packaging/homebrew/tap-ci.yml"
    )
    FORMULAE = ("skillmount", "skillmount-asm")
    BOOTSTRAP_FILES = ("README.md", "CONTRIBUTING.md", "SECURITY.md")

    @classmethod
    def classifier_script(cls) -> str:
        """Extract the exact shell program GitHub Actions runs to classify the tap."""

        document = governance.parse_yaml(cls.WORKFLOW.read_text(encoding="utf-8"))
        steps = document["jobs"]["formulae"]["steps"]
        return next(
            step["run"]
            for step in steps
            if step.get("name") == "Classify the tap publication state"
        )

    @staticmethod
    def git(root: Path, *arguments: str) -> None:
        """Run one deterministic local Git operation for a disposable tap."""

        subprocess.run(
            ("git", "-C", str(root), *arguments),
            check=True,
            text=True,
            capture_output=True,
        )

    def commit(self, root: Path, message: str) -> None:
        """Commit the disposable tap without depending on operator Git identity."""

        self.git(root, "add", "-A")
        self.git(
            root,
            "-c",
            "user.name=SkillMount Tests",
            "-c",
            "user.email=tests@skillmount.invalid",
            "commit",
            "--quiet",
            "-m",
            message,
        )

    def initialize_tap(
        self,
        root: Path,
        *,
        formulae: tuple[str, ...] = (),
        omit: str | None = None,
    ) -> None:
        """Create one committed tap state with the requested Formula names."""

        self.git(root, "init", "--quiet", "-b", "main")
        for relative in self.BOOTSTRAP_FILES:
            if relative != omit:
                (root / relative).write_text(f"# {relative}\n", encoding="utf-8")
        workflow = root / ".github/workflows/tap.yml"
        workflow.parent.mkdir(parents=True)
        workflow.write_text(self.WORKFLOW.read_text(encoding="utf-8"), encoding="utf-8")
        formula_directory = root / "Formula"
        for formula in formulae:
            formula_directory.mkdir(exist_ok=True)
            (formula_directory / f"{formula}.rb").write_text(
                f"class {formula.replace('-', '_').title()} < Formula\nend\n",
                encoding="utf-8",
            )
        self.commit(root, "initialize tap")

    def classify(self, root: Path) -> tuple[subprocess.CompletedProcess[str], str]:
        """Run the workflow classifier and return its process and declared output."""

        output = root.parent / "github-output.txt"
        output.touch()
        environment = os.environ.copy()
        environment.update({"GITHUB_OUTPUT": str(output), "TAP_SOURCE": str(root)})
        result = subprocess.run(
            ("bash", "-c", self.classifier_script()),
            cwd=root,
            env=environment,
            text=True,
            capture_output=True,
            check=False,
        )
        return result, output.read_text(encoding="utf-8")

    def test_classifier_accepts_bootstrap_and_complete_pair(self) -> None:
        """Accept only the intended states before and after first publication."""

        for formulae, expected in (((), "published=false\n"), (self.FORMULAE, "published=true\n")):
            with self.subTest(formulae=formulae):
                with tempfile.TemporaryDirectory() as directory:
                    root = Path(directory) / "tap"
                    root.mkdir()
                    self.initialize_tap(root, formulae=formulae)
                    result, output = self.classify(root)
                    self.assertEqual(result.returncode, 0, result.stdout + result.stderr)
                    self.assertEqual(output, expected)

    def test_classifier_rejects_partial_or_extra_formulae(self) -> None:
        """Reject an incomplete pair and any third Ruby Formula."""

        for formulae in (("skillmount",), (*self.FORMULAE, "unrelated")):
            with self.subTest(formulae=formulae):
                with tempfile.TemporaryDirectory() as directory:
                    root = Path(directory) / "tap"
                    root.mkdir()
                    self.initialize_tap(root, formulae=formulae)
                    result, _ = self.classify(root)
                    self.assertNotEqual(result.returncode, 0)
                    self.assertIn("expected exactly the skillmount Formula pair", result.stdout)

    def test_classifier_rejects_a_symlinked_expected_formula(self) -> None:
        """Require both expected Formulae to be regular tap-owned files."""

        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory) / "tap"
            root.mkdir()
            self.initialize_tap(root, formulae=("skillmount-asm",))
            (root / "skillmount.rb").write_text(
                "class Redirected < Formula\nend\n", encoding="utf-8"
            )
            (root / "Formula/skillmount.rb").symlink_to("../skillmount.rb")
            self.commit(root, "add symlinked formula")
            result, _ = self.classify(root)
            self.assertNotEqual(result.returncode, 0)
            self.assertIn("expected exactly the skillmount Formula pair", result.stdout)

    def test_classifier_never_reopens_bootstrap_after_publication(self) -> None:
        """Reject deletion of both Formulae even when the current tree looks unpublished."""

        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory) / "tap"
            root.mkdir()
            self.initialize_tap(root, formulae=self.FORMULAE)
            for formula in self.FORMULAE:
                (root / "Formula" / f"{formula}.rb").unlink()
            self.commit(root, "remove formulae")
            result, output = self.classify(root)
            self.assertNotEqual(result.returncode, 0)
            self.assertEqual(output, "")
            self.assertIn("after a Formula existed in tap history", result.stdout)

    def test_classifier_requires_every_bootstrap_file(self) -> None:
        """Reject an unpublished tap that omits a maintainer-owned baseline file."""

        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory) / "tap"
            root.mkdir()
            self.initialize_tap(root, omit="SECURITY.md")
            result, _ = self.classify(root)
            self.assertNotEqual(result.returncode, 0)
            self.assertIn("missing required bootstrap files: SECURITY.md", result.stdout)


if __name__ == "__main__":
    unittest.main()
