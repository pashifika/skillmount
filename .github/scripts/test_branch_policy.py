#!/usr/bin/env python3
"""Table tests for the SkillMount pull-request branch policy."""

from __future__ import annotations

import unittest

from branch_policy import validate_branch_flow


class BranchPolicyTests(unittest.TestCase):
    """Cover valid and invalid main/development/topic branch relationships."""

    def test_valid_branch_flows(self) -> None:
        """Accept development promotions and every supported topic prefix."""

        cases = [("main", "dev/0.1.x"), ("main", "dev/12.34.x")]
        cases.extend(
            ("dev/0.1.x", f"{prefix}/catalog-overlay")
            for prefix in (
                "feat",
                "fix",
                "perf",
                "refactor",
                "docs",
                "test",
                "build",
                "ci",
                "chore",
                "revert",
            )
        )

        for base, head in cases:
            with self.subTest(base=base, head=head):
                self.assertIsNone(validate_branch_flow(base, head))

    def test_invalid_branch_flows(self) -> None:
        """Reject skipped development lines, malformed names, and empty slugs."""

        cases = (
            ("main", "feat/catalog-overlay"),
            ("main", "dev/0.x"),
            ("main", "dev/0.1.0"),
            ("dev/0.1.x", "dev/0.2.x"),
            ("dev/0.1.x", "feature/catalog-overlay"),
            ("dev/0.1.x", "feat/"),
            ("release/0.1", "fix/catalog-overlay"),
        )

        for base, head in cases:
            with self.subTest(base=base, head=head):
                self.assertIsNotNone(validate_branch_flow(base, head))


if __name__ == "__main__":
    unittest.main()
