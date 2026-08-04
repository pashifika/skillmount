#!/usr/bin/env python3
"""Govern SkillMount's GitHub Actions workflows with no YAML dependency available."""

from __future__ import annotations

import argparse
import re
import sys
from dataclasses import dataclass
from pathlib import Path
from typing import Any, Iterator, Mapping, Sequence

PACKAGE_WORKFLOW = "package.yml"
PREFLIGHT_JOB = "preflight"
HOMEBREW_PUBLISH_JOB = "homebrew-publish"
CHOCOLATEY_PUBLISH_JOB = "chocolatey-publish"
PUBLISH_CHANNELS = {HOMEBREW_PUBLISH_JOB: "homebrew", CHOCOLATEY_PUBLISH_JOB: "chocolatey"}
PUBLISH_JOBS = tuple(PUBLISH_CHANNELS)
FOREIGN_SECRET_PREFIX = {HOMEBREW_PUBLISH_JOB: "CHOCOLATEY", CHOCOLATEY_PUBLISH_JOB: "HOMEBREW"}
ALLOWED_SHARED_SECRET = "GITHUB_TOKEN"
PREFLIGHT_PERMISSIONS = {"actions": "read", "contents": "read"}
FORBIDDEN_PERMISSIONS = {"contents": "write", "packages": "write", "id-token": "write"}
BLANKET_WRITE_PERMISSIONS = "write-all"
RELEASE_WORKFLOW_NAME = "Release"
REQUIRED_DISPATCH_INPUTS = ("tag", "channels", "verification_only")
CACHE_KEYS = ("cache", "save-always")
CACHE_ACTION_PREFIX = "actions/cache"
CHECKOUT_ACTION_PREFIX = "actions/checkout@"
VERSION_OUTPUT_REFERENCE = "needs.preflight.outputs.version"
VERIFICATION_OUTPUT_REFERENCE = "needs.preflight.outputs.verification_only"

PINNED_USES = re.compile(r"[A-Za-z0-9._/-]+@[0-9a-f]{40}")
USES_LINE = re.compile(r"^ *(?:- +)?uses: *(?P<value>[^ #]+) *(?P<comment>#.*)?$")
VERSION_COMMENT = re.compile(r"# *v?\d[\w.+-]* *$")
SECRET_REFERENCE = re.compile(r"secrets\.([A-Za-z_][A-Za-z0-9_]*)")
WORKFLOW_RUN_REFERENCE = re.compile(
    r"github\.event\.workflow_run\.(head_sha|head_branch|head_commit|head_repository)"
)
UNSAFE_PUBLISH_COMMANDS = (
    (re.compile(r"\btar\b"), "tar"),
    (re.compile(r"\bunzip\b"), "unzip"),
    (re.compile(r"\b7z\b"), "7z"),
    (re.compile(r"Expand-Archive"), "Expand-Archive"),
    (re.compile(r"Get-ChocolateyUnzip"), "Get-ChocolateyUnzip"),
    (re.compile(r"(?<![\w.])\./"), "./"),
    (re.compile(r"chmod +\+x"), "chmod +x"),
    (re.compile(r"\w*_acceptance\.py"), "an acceptance harness"),
)

POLICY_COUNT = 12
ALL_POLICIES = frozenset(range(1, POLICY_COUNT + 1))
# Policies 4-8 and 10-12 describe the package workflow's privileged publication shape and are
# meaningless for a workflow with no publish jobs, so only 1-3 and 9 guard the rest of the tree.
SHARED_POLICIES = frozenset({1, 2, 3, 9})
POLICY_MESSAGE = re.compile(r"^policy (?P<number>\d+): ")


class WorkflowPolicyError(RuntimeError):
    """A workflow document cannot be parsed under the supported YAML subset."""


@dataclass(frozen=True)
class WorkflowGovernance:
    """Which numbered policies guard one workflow file, and why the rest are skipped."""

    policies: frozenset[int]
    allowances: Mapping[int, str]

    def applies(self, number: int) -> bool:
        """Report whether policy *number* is enforced for this workflow."""

        return number in self.policies and number not in self.allowances


# The complete allowance list for this repository. Each entry names the concrete reason the
# policy cannot hold, so no future workflow silently inherits an exemption it did not earn.
GOVERNANCE: Mapping[str, WorkflowGovernance] = {
    PACKAGE_WORKFLOW: WorkflowGovernance(ALL_POLICIES, {}),
    "release.yml": WorkflowGovernance(
        SHARED_POLICIES,
        {
            3: (
                "the release workflow builds and publishes one validated commit, so every "
                "checkout pins ref: needs.preflight.outputs.commit by design"
            ),
            9: (
                "the release publish job creates the GitHub Release itself and therefore needs "
                "contents: write, which no package-channel job ever needs"
            ),
        },
    ),
    "ci.yml": WorkflowGovernance(SHARED_POLICIES, {}),
    "live-agent-smoke.yml": WorkflowGovernance(SHARED_POLICIES, {}),
}
DEFAULT_GOVERNANCE = WorkflowGovernance(SHARED_POLICIES, {})


def governance_for(file_name: str) -> WorkflowGovernance:
    """Return the governance record for *file_name*, defaulting to the shared policies."""

    return GOVERNANCE.get(file_name, DEFAULT_GOVERNANCE)


# ---------------------------------------------------------------------------------------------
# Strict minimal YAML subset parser
# ---------------------------------------------------------------------------------------------

INDENT_STEP = 2
BLOCK_SCALAR_HEADERS = {
    "|": ("literal", "clip"),
    "|-": ("literal", "strip"),
    ">": ("folded", "clip"),
    ">-": ("folded", "strip"),
}
RESERVED_INDICATORS = {
    "&": "an anchor",
    "*": "an alias",
    "!": "a tag",
    "?": "an explicit key",
    "%": "a directive",
    "`": "a reserved indicator",
}
MERGE_KEY = "<<"
DOCUMENT_MARKERS = ("---", "...")
INTEGER_SCALAR = re.compile(r"-?(?:0|[1-9][0-9]*)")
NULL_SCALARS = ("null", "~")


@dataclass(frozen=True)
class _SourceLine:
    """One significant physical line: its number, its indentation, and its content."""

    number: int
    indent: int
    content: str


class _Reader:
    """Cursor over physical lines that skips blank and comment-only lines on demand."""

    def __init__(self, text: str) -> None:
        self._lines = text.split("\n")
        self._offset = 0

    def _significant(self, offset: int) -> _SourceLine | None:
        """Classify one physical line, returning None for a blank or comment-only line."""

        line = self._lines[offset]
        content = line.lstrip(" ")
        if content == "" or content.startswith("#"):
            return None
        return _SourceLine(offset + 1, len(line) - len(content), content)

    def peek(self) -> _SourceLine | None:
        """Advance past insignificant lines and return the next significant line."""

        while self._offset < len(self._lines):
            candidate = self._significant(self._offset)
            if candidate is not None:
                return candidate
            self._offset += 1
        return None

    def take(self) -> _SourceLine:
        """Consume and return the next significant line."""

        line = self.peek()
        if line is None:
            raise WorkflowPolicyError("workflow ended while a value was still expected")
        self._offset += 1
        return line

    def take_block(self, parent_indent: int) -> tuple[list[str], int]:
        """Consume a block scalar's raw lines and report their common indentation width."""

        collected: list[str] = []
        while self._offset < len(self._lines):
            line = self._lines[self._offset]
            content = line.lstrip(" ")
            if content == "":
                collected.append("")
                self._offset += 1
                continue
            if len(line) - len(content) <= parent_indent:
                break
            collected.append(line)
            self._offset += 1
        while collected and collected[-1] == "":
            collected.pop()
        widths = [len(line) - len(line.lstrip(" ")) for line in collected if line != ""]
        return collected, min(widths) if widths else parent_indent + INDENT_STEP


def _reject_reserved(text: str, line_number: int, label: str) -> None:
    """Reject a scalar that opens with a YAML indicator this subset refuses to interpret."""

    indicator = RESERVED_INDICATORS.get(text[:1])
    if indicator is not None:
        raise WorkflowPolicyError(
            f"line {line_number}: {label} opens with {indicator} ({text[:1]!r}), which the "
            f"supported YAML subset rejects rather than interpret: observed {text!r}"
        )


def _quoted_end(text: str, start: int, line_number: int) -> int:
    """Return the index of the closing quote of the quoted scalar opening at *start*."""

    quote = text[start]
    index = start + 1
    while index < len(text):
        character = text[index]
        if quote == '"' and character == "\\":
            index += 2
            continue
        if character == quote:
            if quote == "'" and text[index + 1 : index + 2] == "'":
                index += 2
                continue
            return index
        index += 1
    raise WorkflowPolicyError(
        f"line {line_number}: expected a closing {quote!r} on the same line, observed {text!r}"
    )


DOUBLE_QUOTE_ESCAPES = {"n": "\n", "t": "\t", "r": "\r", '"': '"', "\\": "\\", "/": "/"}


def _decode_quoted(text: str, line_number: int) -> str:
    """Decode one complete single- or double-quoted scalar."""

    body = text[1:-1]
    if text[0] == "'":
        return body.replace("''", "'")
    decoded: list[str] = []
    index = 0
    while index < len(body):
        character = body[index]
        if character != "\\":
            decoded.append(character)
            index += 1
            continue
        replacement = DOUBLE_QUOTE_ESCAPES.get(body[index + 1 : index + 2])
        if replacement is None:
            raise WorkflowPolicyError(
                f"line {line_number}: expected a supported escape after a backslash, observed "
                f"{body[index : index + 2]!r}"
            )
        decoded.append(replacement)
        index += 2
    return "".join(decoded)


def _key_separator(text: str) -> int | None:
    """Return the index of the ``key:`` separator, or None when *text* is not a mapping entry."""

    if text.startswith(("'", '"')):
        end = _quoted_end(text, 0, 0)
        return end + 1 if text[end + 1 : end + 2] == ":" else None
    index = 0
    while True:
        index = text.find(":", index)
        if index < 0:
            return None
        if text[index + 1 : index + 2] in ("", " "):
            return index
        index += 1


def _plain_scalar(text: str, line_number: int) -> Any:
    """Interpret a plain scalar, dropping any trailing comment and typing the result."""

    comment = text.find(" #")
    body = (text[:comment] if comment >= 0 else text).rstrip()
    _reject_reserved(body, line_number, "a plain scalar")
    if ": " in body:
        raise WorkflowPolicyError(
            f"line {line_number}: a plain scalar may not contain ': ', because the supported "
            f"YAML subset will not guess whether it opens a nested mapping: observed {body!r}"
        )
    if body == "" or body in NULL_SCALARS:
        return None
    if body == "true":
        return True
    if body == "false":
        return False
    if INTEGER_SCALAR.fullmatch(body):
        return int(body)
    return body


def _scalar(text: str, line_number: int) -> Any:
    """Interpret one inline scalar, quoted or plain."""

    if text.startswith(("'", '"')):
        end = _quoted_end(text, 0, line_number)
        trailer = text[end + 1 :].strip()
        if trailer and not trailer.startswith("#"):
            raise WorkflowPolicyError(
                f"line {line_number}: expected nothing after a quoted scalar except a comment, "
                f"observed {trailer!r}"
            )
        return _decode_quoted(text[: end + 1], line_number)
    return _plain_scalar(text, line_number)


def _decode_key(text: str, line_number: int) -> str:
    """Decode and validate one mapping key."""

    body = text.rstrip()
    if body.startswith(("'", '"')):
        key = _decode_quoted(body, line_number)
    else:
        key = body
    if key == MERGE_KEY:
        raise WorkflowPolicyError(
            f"line {line_number}: the merge key '<<' is rejected rather than resolved by the "
            "supported YAML subset"
        )
    _reject_reserved(key, line_number, "a mapping key")
    if key == "":
        raise WorkflowPolicyError(f"line {line_number}: expected a non-empty mapping key")
    return key


def _split_flow(body: str, line_number: int) -> list[str]:
    """Split a flow collection body on its top-level commas, rejecting nested collections."""

    if body.strip() == "":
        return []
    items: list[str] = []
    current: list[str] = []
    index = 0
    while index < len(body):
        character = body[index]
        if character in ("'", '"'):
            end = _quoted_end(body, index, line_number)
            current.append(body[index : end + 1])
            index = end + 1
            continue
        if character in "{}[]":
            raise WorkflowPolicyError(
                f"line {line_number}: the supported YAML subset allows only flat flow "
                f"collections, observed a nested {character!r} in {body!r}"
            )
        if character == ",":
            items.append("".join(current).strip())
            current = []
            index += 1
            continue
        current.append(character)
        index += 1
    items.append("".join(current).strip())
    if any(item == "" for item in items):
        raise WorkflowPolicyError(
            f"line {line_number}: expected a scalar between flow commas, observed {body!r}"
        )
    return items


def _parse_flow(text: str, line_number: int) -> Any:
    """Parse an inline flow mapping or flow sequence of scalars."""

    closing = {"{": "}", "[": "]"}[text[0]]
    if not text.endswith(closing):
        raise WorkflowPolicyError(
            f"line {line_number}: expected a flow collection closed by {closing!r} on the same "
            f"line, observed {text!r}"
        )
    items = _split_flow(text[1:-1], line_number)
    if text[0] == "[":
        return [_scalar(item, line_number) for item in items]
    mapping: dict[str, Any] = {}
    for item in items:
        separator = _key_separator(item)
        if separator is None:
            raise WorkflowPolicyError(
                f"line {line_number}: expected 'key: value' inside a flow mapping, observed "
                f"{item!r}"
            )
        key = _decode_key(item[:separator], line_number)
        if key in mapping:
            raise WorkflowPolicyError(
                f"line {line_number}: duplicate key {key!r} in one flow mapping"
            )
        mapping[key] = _scalar(item[separator + 1 :].strip(), line_number)
    return mapping


def _fold(lines: Sequence[str]) -> str:
    """Fold a folded block scalar the way YAML does, keeping more-indented lines literal.

    A single break between two equally indented content lines becomes one space. A break
    adjacent to a more-indented line is never folded, and a run of *n* breaks between content
    lines contributes *n - 1* newlines.
    """

    pieces: list[str] = []
    pending_breaks = 0
    previous_more_indented = False
    for line in lines:
        if line == "":
            pending_breaks += 1
            continue
        more_indented = line.startswith(" ")
        if pieces:
            unfoldable = more_indented or previous_more_indented
            if pending_breaks == 0:
                pieces.append("\n" if unfoldable else " ")
            else:
                pieces.append("\n" * (pending_breaks + (1 if unfoldable else 0)))
        pending_breaks = 0
        pieces.append(line)
        previous_more_indented = more_indented
    return "".join(pieces)


class _Parser:
    """Recursive-descent parser for the block-structured YAML subset the workflows use."""

    def __init__(self, text: str) -> None:
        self._reader = _Reader(text)

    def parse_document(self) -> dict[str, Any]:
        """Parse the single implicit document and require a top-level mapping."""

        line = self._reader.peek()
        if line is None:
            raise WorkflowPolicyError("expected a workflow mapping, observed an empty document")
        if line.indent != 0:
            raise WorkflowPolicyError(
                f"line {line.number}: expected the document to start at indentation 0, observed "
                f"indentation {line.indent}"
            )
        document = self._parse_block(0)
        trailing = self._reader.peek()
        if trailing is not None:
            raise WorkflowPolicyError(
                f"line {trailing.number}: expected the document to end, observed "
                f"{trailing.content!r}"
            )
        if not isinstance(document, dict):
            raise WorkflowPolicyError(
                f"expected a top-level mapping, observed {type(document).__name__}"
            )
        return document

    def _parse_block(self, indent: int) -> Any:
        """Parse the block mapping or block sequence whose entries sit at *indent*."""

        line = self._reader.peek()
        if line is None or line.indent != indent:
            observed = "end of document" if line is None else f"indentation {line.indent}"
            raise WorkflowPolicyError(
                f"expected a block at indentation {indent}, observed {observed}"
            )
        if _is_sequence_entry(line.content):
            return self._parse_sequence(indent)
        return self._parse_mapping(indent, {})

    def _parse_mapping(self, indent: int, mapping: dict[str, Any]) -> dict[str, Any]:
        """Parse consecutive ``key: value`` entries at *indent* into *mapping*."""

        while True:
            line = self._reader.peek()
            if line is None or line.indent < indent:
                return mapping
            if line.indent > indent:
                raise WorkflowPolicyError(
                    f"line {line.number}: expected indentation {indent} to continue the mapping, "
                    f"observed indentation {line.indent}"
                )
            if _is_sequence_entry(line.content):
                return mapping
            separator = _key_separator(line.content)
            if separator is None:
                raise WorkflowPolicyError(
                    f"line {line.number}: expected 'key:' or 'key: value', observed "
                    f"{line.content!r}"
                )
            key = _decode_key(line.content[:separator], line.number)
            if key in mapping:
                raise WorkflowPolicyError(
                    f"line {line.number}: duplicate key {key!r} in one mapping"
                )
            self._reader.take()
            mapping[key] = self._parse_value(line.content[separator + 1 :], indent, line.number)

    def _parse_sequence(self, indent: int) -> list[Any]:
        """Parse consecutive ``- item`` entries at *indent*."""

        items: list[Any] = []
        while True:
            line = self._reader.peek()
            if line is None or line.indent < indent:
                return items
            if line.indent > indent:
                raise WorkflowPolicyError(
                    f"line {line.number}: expected indentation {indent} to continue the "
                    f"sequence, observed indentation {line.indent}"
                )
            if not _is_sequence_entry(line.content):
                return items
            self._reader.take()
            inner = line.content[1:].lstrip(" ")
            entry_indent = line.indent + (len(line.content) - len(inner))
            if inner == "" or inner.startswith("#"):
                items.append(self._parse_nested(indent))
                continue
            separator = _key_separator(inner)
            if separator is None:
                items.append(self._inline_value(inner, line.number))
                continue
            key = _decode_key(inner[:separator], line.number)
            entry = {key: self._parse_value(inner[separator + 1 :], entry_indent, line.number)}
            items.append(self._parse_mapping(entry_indent, entry))

    def _parse_nested(self, indent: int) -> Any:
        """Parse the block belonging to a key or sequence entry that has no inline value."""

        line = self._reader.peek()
        if line is None or line.indent < indent:
            return None
        if line.indent == indent + INDENT_STEP:
            return self._parse_block(line.indent)
        if line.indent == indent and _is_sequence_entry(line.content):
            return self._parse_sequence(indent)
        if line.indent <= indent:
            return None
        raise WorkflowPolicyError(
            f"line {line.number}: expected indentation {indent + INDENT_STEP} for a nested "
            f"block, observed indentation {line.indent}"
        )

    def _inline_value(self, text: str, line_number: int) -> Any:
        """Interpret an inline flow collection or an inline scalar."""

        if text[:1] in ("{", "["):
            return _parse_flow(text, line_number)
        return _scalar(text, line_number)

    def _parse_value(self, text: str, indent: int, line_number: int) -> Any:
        """Interpret the text after ``key:`` and consume any block that belongs to it.

        The caller has already proven the separator was ``:`` followed by a space or a line
        end, so *text* is either empty or begins with a space.
        """

        inline = text.strip()
        if inline == "" or inline.startswith("#"):
            return self._parse_nested(indent)
        if inline in BLOCK_SCALAR_HEADERS:
            return self._parse_block_scalar(inline, indent, line_number)
        if inline[0] in ("|", ">"):
            raise WorkflowPolicyError(
                f"line {line_number}: expected a '|', '|-', '>' or '>-' block scalar header, "
                f"observed {inline!r}"
            )
        return self._inline_value(inline, line_number)

    def _parse_block_scalar(self, header: str, indent: int, line_number: int) -> str:
        """Read, dedent, fold, and chomp one block scalar."""

        style, chomping = BLOCK_SCALAR_HEADERS[header]
        lines, width = self._reader.take_block(indent)
        dedented = [line[width:] if line != "" else "" for line in lines]
        body = "\n".join(dedented) if style == "literal" else _fold(dedented)
        return body if chomping == "strip" else f"{body}\n"


def _is_sequence_entry(content: str) -> bool:
    """Report whether a line's content opens a block sequence entry."""

    return content == "-" or content.startswith("- ")


def parse_yaml(text: str) -> dict[str, Any]:
    """Parse a workflow written in the supported YAML subset into plain Python data."""

    if "\t" in text:
        line_number = text[: text.index("\t")].count("\n") + 1
        raise WorkflowPolicyError(
            f"line {line_number}: expected space indentation, observed a tab character"
        )
    for number, line in enumerate(text.split("\n"), start=1):
        stripped = line.strip()
        if stripped == "":
            continue
        if stripped in DOCUMENT_MARKERS or stripped.startswith("--- "):
            raise WorkflowPolicyError(
                f"line {number}: expected a single implicit document, observed the document "
                f"marker {stripped!r}"
            )
        indent = len(line) - len(line.lstrip(" "))
        if indent % INDENT_STEP != 0:
            raise WorkflowPolicyError(
                f"line {number}: expected indentation in multiples of {INDENT_STEP}, observed "
                f"indentation {indent}"
            )
    return _Parser(text).parse_document()


# ---------------------------------------------------------------------------------------------
# Document navigation
# ---------------------------------------------------------------------------------------------


def _walk(node: Any, path: tuple[Any, ...] = ()) -> Iterator[tuple[tuple[Any, ...], Any]]:
    """Yield every ``(path, node)`` pair in a parsed document, depth first."""

    yield path, node
    if isinstance(node, Mapping):
        for key, value in node.items():
            yield from _walk(value, path + (key,))
    elif isinstance(node, list):
        for index, value in enumerate(node):
            yield from _walk(value, path + (index,))


def _describe(path: Sequence[Any]) -> str:
    """Render a document path as a compact locator for a violation message."""

    if not path:
        return "<document>"
    rendered = str(path[0])
    for element in path[1:]:
        rendered += f"[{element}]" if isinstance(element, int) else f".{element}"
    return rendered


def _jobs(document: Mapping[str, Any]) -> dict[str, Mapping[str, Any]]:
    """Return the workflow's jobs, ignoring anything that is not a mapping."""

    jobs = document.get("jobs")
    if not isinstance(jobs, Mapping):
        return {}
    return {name: job for name, job in jobs.items() if isinstance(job, Mapping)}


def _steps(job: Mapping[str, Any]) -> list[Mapping[str, Any]]:
    """Return one job's steps, ignoring anything that is not a mapping."""

    steps = job.get("steps")
    if not isinstance(steps, list):
        return []
    return [step for step in steps if isinstance(step, Mapping)]


def _job_name(path: Sequence[Any]) -> str | None:
    """Return the job a document path belongs to, or None for workflow-level content."""

    if len(path) >= 2 and path[0] == "jobs" and isinstance(path[1], str):
        return path[1]
    return None


def _as_list(value: Any) -> list[Any]:
    """Normalise a scalar-or-sequence workflow field to a list."""

    if value is None:
        return []
    return list(value) if isinstance(value, list) else [value]


# ---------------------------------------------------------------------------------------------
# Policies
# ---------------------------------------------------------------------------------------------


def _uses_comments(text: str) -> dict[str, list[tuple[int, str | None]]]:
    """Index every ``uses:`` source line by its value, recording the trailing comment."""

    indexed: dict[str, list[tuple[int, str | None]]] = {}
    for number, line in enumerate(text.split("\n"), start=1):
        match = USES_LINE.match(line)
        if match is not None:
            indexed.setdefault(match.group("value"), []).append((number, match.group("comment")))
    return indexed


def _policy_pinned_actions(document: Mapping[str, Any], text: str) -> list[str]:
    """1. Every ``uses:`` pins a 40-hex commit SHA and carries a version comment."""

    indexed = _uses_comments(text)
    violations: list[str] = []
    for path, node in _walk(document):
        if not isinstance(node, Mapping) or not isinstance(node.get("uses"), str):
            continue
        value = node["uses"]
        where = _describe(path)
        if not PINNED_USES.fullmatch(value):
            violations.append(
                f"policy 1: {where} expected uses '<action>@<40-hex sha>', observed {value!r}"
            )
            continue
        records = indexed.get(value, [])
        unlabelled = [
            number
            for number, comment in records
            if comment is None or VERSION_COMMENT.search(comment) is None
        ]
        if not records or unlabelled:
            observed = f"lines {unlabelled}" if unlabelled else "no matching source line"
            violations.append(
                f"policy 1: {where} expected uses {value!r} to carry a trailing '# <version>' "
                f"comment, observed {observed}"
            )
    return violations


def _policy_no_cache(document: Mapping[str, Any], text: str) -> list[str]:
    """2. No ``actions/cache`` and no ``cache:`` or ``save-always:`` key anywhere."""

    violations: list[str] = []
    for path, node in _walk(document):
        if not isinstance(node, Mapping):
            continue
        for key in node:
            if key in CACHE_KEYS:
                violations.append(
                    f"policy 2: {_describe(path)} expected no caching key, observed {key!r}"
                )
        uses = node.get("uses")
        if isinstance(uses, str) and uses.startswith(CACHE_ACTION_PREFIX):
            violations.append(
                f"policy 2: {_describe(path)} expected no caching action, observed uses {uses!r}"
            )
    return violations


def _policy_checkout_hygiene(document: Mapping[str, Any], text: str) -> list[str]:
    """3. Every checkout sets ``persist-credentials: false`` and declares no ``ref:``."""

    violations: list[str] = []
    for job_name, job in _jobs(document).items():
        for index, step in enumerate(_steps(job)):
            uses = step.get("uses")
            if not (isinstance(uses, str) and uses.startswith(CHECKOUT_ACTION_PREFIX)):
                continue
            where = f"jobs.{job_name}.steps[{index}]"
            inputs = step.get("with")
            inputs = inputs if isinstance(inputs, Mapping) else {}
            if inputs.get("persist-credentials") is not False:
                violations.append(
                    f"policy 3: {where} expected 'persist-credentials: false', observed "
                    f"{inputs.get('persist-credentials')!r}"
                )
            if "ref" in inputs:
                violations.append(
                    f"policy 3: {where} expected no 'ref:' input on a checkout, observed "
                    f"{inputs['ref']!r}"
                )
    return violations


def _policy_secret_scoping(document: Mapping[str, Any], text: str) -> list[str]:
    """4. Secrets stay inside their own publish job, and neither lane reads the other's."""

    violations: list[str] = []
    for path, node in _walk(document):
        if not isinstance(node, str):
            continue
        job_name = _job_name(path)
        for name in SECRET_REFERENCE.findall(node):
            where = _describe(path)
            if job_name not in PUBLISH_JOBS:
                if name != ALLOWED_SHARED_SECRET:
                    violations.append(
                        f"policy 4: {where} expected only secrets.{ALLOWED_SHARED_SECRET} "
                        f"outside {list(PUBLISH_JOBS)}, observed secrets.{name}"
                    )
                continue
            prefix = FOREIGN_SECRET_PREFIX[job_name]
            if name.startswith(prefix):
                violations.append(
                    f"policy 4: {where} expected no secrets.{prefix}* in {job_name}, observed "
                    f"secrets.{name}"
                )
    return violations


def _policy_environments(document: Mapping[str, Any], text: str) -> list[str]:
    """5. Exactly the two publish jobs declare their own deployment environment."""

    jobs = _jobs(document)
    declared = sorted(name for name, job in jobs.items() if "environment" in job)
    violations: list[str] = []
    if declared != sorted(PUBLISH_JOBS):
        violations.append(
            f"policy 5: expected exactly {sorted(PUBLISH_JOBS)} to declare 'environment:', "
            f"observed {declared}"
        )
    for job_name, channel in PUBLISH_CHANNELS.items():
        job = jobs.get(job_name)
        if job is None or "environment" not in job:
            continue
        if job["environment"] != channel:
            violations.append(
                f"policy 5: jobs.{job_name} expected environment {channel!r}, observed "
                f"{job['environment']!r}"
            )
    return violations


def _policy_independent_publishes(document: Mapping[str, Any], text: str) -> list[str]:
    """6. Neither publish job waits on the other, so one channel cannot block the other."""

    jobs = _jobs(document)
    violations: list[str] = []
    for job_name in PUBLISH_JOBS:
        job = jobs.get(job_name)
        if job is None:
            continue
        other = next(name for name in PUBLISH_JOBS if name != job_name)
        needs = _as_list(job.get("needs"))
        if other in needs:
            violations.append(
                f"policy 6: jobs.{job_name}.needs expected no {other!r}, observed {needs}"
            )
    return violations


def _policy_publish_concurrency(document: Mapping[str, Any], text: str) -> list[str]:
    """7. Each publish job serialises per version without cancelling a run already in flight."""

    jobs = _jobs(document)
    violations: list[str] = []
    for job_name, channel in PUBLISH_CHANNELS.items():
        job = jobs.get(job_name)
        if job is None:
            continue
        concurrency = job.get("concurrency")
        if not isinstance(concurrency, Mapping):
            violations.append(
                f"policy 7: jobs.{job_name} expected a 'concurrency:' mapping, observed "
                f"{concurrency!r}"
            )
            continue
        if concurrency.get("cancel-in-progress") is not False:
            violations.append(
                f"policy 7: jobs.{job_name}.concurrency expected 'cancel-in-progress: false', "
                f"observed {concurrency.get('cancel-in-progress')!r}"
            )
        group = concurrency.get("group")
        if not isinstance(group, str) or channel not in group:
            violations.append(
                f"policy 7: jobs.{job_name}.concurrency expected a group naming {channel!r}, "
                f"observed {group!r}"
            )
        elif VERSION_OUTPUT_REFERENCE not in group:
            violations.append(
                f"policy 7: jobs.{job_name}.concurrency expected a group naming "
                f"{VERSION_OUTPUT_REFERENCE}, observed {group!r}"
            )
    return violations


def _policy_verification_guard(document: Mapping[str, Any], text: str) -> list[str]:
    """8. Each publish job's condition consults the preflight verification-only decision."""

    jobs = _jobs(document)
    violations: list[str] = []
    for job_name in PUBLISH_JOBS:
        job = jobs.get(job_name)
        if job is None:
            continue
        condition = job.get("if")
        if not isinstance(condition, str) or VERIFICATION_OUTPUT_REFERENCE not in condition:
            violations.append(
                f"policy 8: jobs.{job_name}.if expected a reference to "
                f"{VERIFICATION_OUTPUT_REFERENCE}, observed {condition!r}"
            )
    return violations


def _permission_violations(where: str, permissions: Any) -> list[str]:
    """Report every write grant in one ``permissions:`` block."""

    violations: list[str] = []
    if permissions == BLANKET_WRITE_PERMISSIONS:
        violations.append(
            f"policy 9: {where} expected no blanket write grant, observed "
            f"{BLANKET_WRITE_PERMISSIONS!r}"
        )
    if isinstance(permissions, Mapping):
        for scope, grant in permissions.items():
            if FORBIDDEN_PERMISSIONS.get(scope) == grant:
                violations.append(
                    f"policy 9: {where} expected no '{scope}: {grant}' grant, observed it"
                )
    return violations


def _policy_permissions(document: Mapping[str, Any], text: str) -> list[str]:
    """9. No job grants a write permission, and preflight grants exactly two read scopes."""

    violations = _permission_violations("<document>.permissions", document.get("permissions"))
    jobs = _jobs(document)
    for job_name, job in jobs.items():
        violations.extend(
            _permission_violations(f"jobs.{job_name}.permissions", job.get("permissions"))
        )
    preflight = jobs.get(PREFLIGHT_JOB)
    if preflight is not None:
        granted = preflight.get("permissions")
        if not isinstance(granted, Mapping) or dict(granted) != PREFLIGHT_PERMISSIONS:
            violations.append(
                f"policy 9: jobs.{PREFLIGHT_JOB}.permissions expected exactly "
                f"{PREFLIGHT_PERMISSIONS}, observed {granted!r}"
            )
    return violations


def _policy_untrusted_workflow_run(document: Mapping[str, Any], text: str) -> list[str]:
    """10. Attacker-controlled ``workflow_run`` fields stay in preflight and off every checkout."""

    violations: list[str] = []
    for path, node in _walk(document):
        if not isinstance(node, str):
            continue
        job_name = _job_name(path)
        for field in WORKFLOW_RUN_REFERENCE.findall(node):
            if job_name != PREFLIGHT_JOB:
                violations.append(
                    f"policy 10: {_describe(path)} expected no reference to "
                    f"github.event.workflow_run.{field} outside {PREFLIGHT_JOB}, observed one"
                )
    for job_name, job in _jobs(document).items():
        for index, step in enumerate(_steps(job)):
            uses = step.get("uses")
            if not (isinstance(uses, str) and uses.startswith(CHECKOUT_ACTION_PREFIX)):
                continue
            inputs = step.get("with")
            for key, value in (inputs if isinstance(inputs, Mapping) else {}).items():
                if isinstance(value, str) and WORKFLOW_RUN_REFERENCE.search(value):
                    violations.append(
                        f"policy 10: jobs.{job_name}.steps[{index}].with.{key} expected no "
                        f"github.event.workflow_run head value on a checkout, observed {value!r}"
                    )
    return violations


def _policy_publish_never_extracts(document: Mapping[str, Any], text: str) -> list[str]:
    """11. A publish job never extracts, executes, or re-runs acceptance content."""

    jobs = _jobs(document)
    violations: list[str] = []
    for job_name in PUBLISH_JOBS:
        job = jobs.get(job_name)
        if job is None:
            continue
        for index, step in enumerate(_steps(job)):
            script = step.get("run")
            if not isinstance(script, str):
                continue
            for pattern, label in UNSAFE_PUBLISH_COMMANDS:
                if pattern.search(script):
                    violations.append(
                        f"policy 11: jobs.{job_name}.steps[{index}].run expected no {label!r} "
                        "in a publish job, observed it"
                    )
    return violations


def _policy_triggers(document: Mapping[str, Any], text: str) -> list[str]:
    """12. The workflow follows a completed Release run and accepts a guarded dispatch."""

    triggers = document.get("on")
    if not isinstance(triggers, Mapping):
        return [f"policy 12: expected an 'on:' mapping of triggers, observed {triggers!r}"]
    violations: list[str] = []
    workflow_run = triggers.get("workflow_run")
    if not isinstance(workflow_run, Mapping):
        violations.append(
            f"policy 12: expected an 'on.workflow_run' trigger, observed {workflow_run!r}"
        )
    else:
        workflows = _as_list(workflow_run.get("workflows"))
        if workflows != [RELEASE_WORKFLOW_NAME]:
            violations.append(
                f"policy 12: expected on.workflow_run.workflows [{RELEASE_WORKFLOW_NAME!r}], "
                f"observed {workflows}"
            )
        types = _as_list(workflow_run.get("types"))
        if "completed" not in types:
            violations.append(
                f"policy 12: expected on.workflow_run.types to contain 'completed', observed "
                f"{types}"
            )
    dispatch = triggers.get("workflow_dispatch")
    if not isinstance(dispatch, Mapping):
        violations.append(
            f"policy 12: expected an 'on.workflow_dispatch' trigger with inputs, observed "
            f"{dispatch!r}"
        )
        return violations
    inputs = dispatch.get("inputs")
    inputs = inputs if isinstance(inputs, Mapping) else {}
    missing = [name for name in REQUIRED_DISPATCH_INPUTS if name not in inputs]
    if missing:
        violations.append(
            f"policy 12: expected on.workflow_dispatch.inputs {list(REQUIRED_DISPATCH_INPUTS)}, "
            f"observed missing {missing}"
        )
    verification = inputs.get("verification_only")
    if isinstance(verification, Mapping):
        if verification.get("type") != "boolean":
            violations.append(
                "policy 12: expected on.workflow_dispatch.inputs.verification_only.type "
                f"'boolean', observed {verification.get('type')!r}"
            )
        if verification.get("default") is not True:
            violations.append(
                "policy 12: expected on.workflow_dispatch.inputs.verification_only.default "
                f"true, observed {verification.get('default')!r}"
            )
    return violations


POLICIES = (
    _policy_pinned_actions,
    _policy_no_cache,
    _policy_checkout_hygiene,
    _policy_secret_scoping,
    _policy_environments,
    _policy_independent_publishes,
    _policy_publish_concurrency,
    _policy_verification_guard,
    _policy_permissions,
    _policy_untrusted_workflow_run,
    _policy_publish_never_extracts,
    _policy_triggers,
)


def check_workflow(document: Mapping[str, Any], text: str) -> list[str]:
    """Return every numbered policy violation in one parsed workflow and its source text."""

    violations: list[str] = []
    for policy in POLICIES:
        violations.extend(policy(document, text))
    return violations


def policy_number(message: str) -> int:
    """Return the policy number one violation message reports."""

    match = POLICY_MESSAGE.match(message)
    if match is None:
        raise WorkflowPolicyError(
            f"expected a violation message prefixed 'policy <number>: ', observed {message!r}"
        )
    return int(match.group("number"))


def governed_violations(file_name: str, violations: Sequence[str]) -> list[str]:
    """Keep only the violations of policies that govern *file_name*."""

    governance = governance_for(file_name)
    return [message for message in violations if governance.applies(policy_number(message))]


def check_paths(paths: Sequence[Path]) -> list[str]:
    """Check every workflow path and return path-prefixed violations."""

    reported: list[str] = []
    for path in paths:
        text = path.read_text(encoding="utf-8")
        document = parse_yaml(text)
        for message in governed_violations(path.name, check_workflow(document, text)):
            reported.append(f"{path}: {message}")
    return reported


def argument_parser() -> argparse.ArgumentParser:
    """Build the workflow-policy command-line parser."""

    parser = argparse.ArgumentParser(description=__doc__)
    subparsers = parser.add_subparsers(dest="command", required=True)
    check = subparsers.add_parser("check", help="check one or more workflow files")
    check.add_argument("paths", nargs="+", type=Path)
    return parser


def run(arguments: Sequence[str]) -> int:
    """Check the requested workflows and report every governed violation."""

    options = argument_parser().parse_args(arguments)
    violations = check_paths(options.paths)
    for message in violations:
        print(message)
    if violations:
        print(f"{len(violations)} workflow policy violation(s)", file=sys.stderr)
        return 1
    print(f"{len(options.paths)} workflow(s) satisfy every governing policy")
    return 0


def main(arguments: Sequence[str] | None = None) -> int:
    """Convert an unparsable or ungoverned workflow into a stable nonzero status."""

    try:
        return run(sys.argv[1:] if arguments is None else arguments)
    except (OSError, UnicodeError, WorkflowPolicyError) as error:
        print(f"workflow policy check failed: {error}", file=sys.stderr)
        return 1


if __name__ == "__main__":
    raise SystemExit(main())
