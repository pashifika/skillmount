#!/usr/bin/env python3
"""Table tests for the SkillMount pull-request branch policy."""

from __future__ import annotations

import unittest

from branch_policy import main, validate_branch_flow

REPOSITORY = "pashifika/skillmount"


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
                self.assertIsNone(
                    validate_branch_flow(base, head, REPOSITORY, REPOSITORY)
                )

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
                self.assertIsNotNone(
                    validate_branch_flow(base, head, REPOSITORY, REPOSITORY)
                )

    def test_main_promotion_rejects_same_named_branch_from_fork(self) -> None:
        """Require development-line promotions to originate in this repository."""

        self.assertIsNotNone(
            validate_branch_flow(
                "main", "dev/0.1.x", REPOSITORY, "contributor/skillmount"
            )
        )

    def test_development_line_accepts_topic_branch_from_fork(self) -> None:
        """Keep external topic contributions available on development lines."""

        self.assertIsNone(
            validate_branch_flow(
                "dev/0.1.x",
                "feat/external-contribution",
                REPOSITORY,
                "contributor/skillmount",
            )
        )

    def test_command_line_entry_point_statuses(self) -> None:
        """Cover usage, accepted, and rejected command-line outcomes."""

        self.assertEqual(main(["branch_policy.py"]), 2)
        self.assertEqual(
            main(
                [
                    "branch_policy.py",
                    "main",
                    "dev/0.1.x",
                    REPOSITORY,
                    REPOSITORY,
                ]
            ),
            0,
        )
        self.assertEqual(
            main(
                [
                    "branch_policy.py",
                    "main",
                    "dev/0.1.x",
                    REPOSITORY,
                    "contributor/skillmount",
                ]
            ),
            1,
        )


if __name__ == "__main__":
    unittest.main()
