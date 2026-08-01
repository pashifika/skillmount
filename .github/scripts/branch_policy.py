#!/usr/bin/env python3
"""Validate the pull-request branch flow used by SkillMount."""

from __future__ import annotations

import re
import sys

DEVELOPMENT_BRANCH = re.compile(r"dev/[0-9]+\.[0-9]+\.x")
TOPIC_BRANCH = re.compile(
    r"(?:feat|fix|perf|refactor|docs|test|build|ci|chore|revert)/.+"
)


def validate_branch_flow(base: str, head: str) -> str | None:
    """Return an error message when *head* is not allowed to target *base*."""

    if base == "main":
        if DEVELOPMENT_BRANCH.fullmatch(head):
            return None
        return "pull requests into main must come from dev/<major>.<minor>.x"

    if DEVELOPMENT_BRANCH.fullmatch(base):
        if TOPIC_BRANCH.fullmatch(head):
            return None
        return (
            "pull requests into a development line must come from a supported "
            "topic branch with a non-empty slug"
        )

    return "the pull-request base must be main or dev/<major>.<minor>.x"


def main(arguments: list[str]) -> int:
    """Run the command-line branch-policy check."""

    if len(arguments) != 3:
        print("usage: branch_policy.py <base> <head>", file=sys.stderr)
        return 2

    base, head = arguments[1:]
    error = validate_branch_flow(base, head)
    if error is not None:
        print(f"branch policy rejected {head} -> {base}: {error}", file=sys.stderr)
        return 1

    print(f"branch policy accepted {head} -> {base}")
    return 0


if __name__ == "__main__":
    raise SystemExit(main(sys.argv))
