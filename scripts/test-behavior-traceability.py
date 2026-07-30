#!/usr/bin/env python3
"""Red/green unit tests for check-behavior-traceability.py (#1074)."""

from __future__ import annotations

import importlib.util
import sys
import tempfile
import unittest
from pathlib import Path


sys.dont_write_bytecode = True
SCRIPT = Path(__file__).with_name("check-behavior-traceability.py")
SPEC = importlib.util.spec_from_file_location("behavior_traceability", SCRIPT)
assert SPEC is not None and SPEC.loader is not None
CHECKER = importlib.util.module_from_spec(SPEC)
sys.modules[SPEC.name] = CHECKER
SPEC.loader.exec_module(CHECKER)


BUILT = """\
Feature: One governed behavior
  Rule: One semantic owner
    # nmp:id=ROUTING-CAP-001
    # nmp:status=built
    # nmp:evidence=rust:nmp-router::coverage_respects_whole_demand_cap
    # nmp:falsifier=changing CapExhausted to NoCandidates makes the assertion fail
    @ledger-4
    Scenario: An impossible objective reports shortfall
      Given demand exceeds the relay ceiling
      Then the excluded demand reports cap exhaustion
"""

SPECIFIED = """\
Feature: One governed gap
  # nmp:id=ROUTING-CAP-002
  # nmp:status=specified
  # nmp:gap=implementation
  # nmp:issue=#1071
  Scenario: A future distinction remains explicit
    Then the gap is not disguised as a passing proof
"""

KNOWN_VIOLATION = """\
Feature: One governed violation
  # nmp:id=ROUTING-CAP-003
  # nmp:status=known-violation
  # nmp:issue=https://github.com/pablof7z/nmp/issues/1071
  Scenario: A known violation stays visible
    Then its open issue owns the repair
"""


class TraceabilityTests(unittest.TestCase):
    def validate(self, *contents: str):
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            paths = []
            for index, content in enumerate(contents):
                path = root / f"case-{index}.feature"
                path.write_text(content, encoding="utf-8")
                paths.append(path)
            return CHECKER.validate(paths, root, resolve=False)

    def messages(self, *contents: str) -> list[str]:
        _records, problems = self.validate(*contents)
        return [problem.message for problem in problems]

    def assert_problem(self, expected: str, *contents: str) -> None:
        messages = self.messages(*contents)
        self.assertTrue(
            any(expected in message for message in messages),
            f"expected {expected!r} in {messages!r}",
        )

    def test_valid_built_specified_and_known_violation(self):
        records, problems = self.validate(BUILT, SPECIFIED, KNOWN_VIOLATION)
        self.assertEqual(3, len(records))
        self.assertEqual([], problems)

    def test_missing_metadata_is_red(self):
        self.assert_problem(
            "missing contiguous nmp metadata",
            "Feature: Missing\n  Scenario: No identity\n    Then this is red\n",
        )

    def test_duplicate_id_is_red(self):
        self.assert_problem("duplicate nmp:id", BUILT, BUILT)

    def test_invalid_id_is_red(self):
        self.assert_problem(
            "invalid nmp:id",
            BUILT.replace("ROUTING-CAP-001", "routing-cap-1"),
        )

    def test_invalid_status_is_red(self):
        self.assert_problem(
            "invalid nmp:status",
            BUILT.replace("nmp:status=built", "nmp:status=wip"),
        )

    def test_built_requires_evidence_and_falsifier(self):
        without_evidence = BUILT.replace(
            "    # nmp:evidence=rust:nmp-router::coverage_respects_whole_demand_cap\n",
            "",
        )
        without_falsifier = BUILT.replace(
            "    # nmp:falsifier=changing CapExhausted to NoCandidates makes the assertion fail\n",
            "",
        )
        self.assert_problem("built scenario requires nmp:evidence", without_evidence)
        self.assert_problem("built scenario requires nmp:falsifier", without_falsifier)

    def test_specified_requires_typed_gap_and_issue(self):
        self.assert_problem(
            "specified scenario requires nmp:gap",
            SPECIFIED.replace("  # nmp:gap=implementation\n", ""),
        )
        self.assert_problem(
            "specified scenario requires nmp:issue",
            SPECIFIED.replace("  # nmp:issue=#1071\n", ""),
        )
        self.assert_problem(
            "invalid nmp:gap",
            SPECIFIED.replace("nmp:gap=implementation", "nmp:gap=unknown"),
        )

    def test_known_violation_requires_issue(self):
        self.assert_problem(
            "known-violation scenario requires nmp:issue",
            KNOWN_VIOLATION.replace(
                "  # nmp:issue=https://github.com/pablof7z/nmp/issues/1071\n", ""
            ),
        )

    def test_lifecycle_and_capability_tags_are_red(self):
        self.assert_problem(
            "@wip is forbidden",
            BUILT.replace("@ledger-4", "@ledger-4 @wip"),
        )
        self.assert_problem(
            "@requires-nip17 is forbidden",
            BUILT.replace("@ledger-4", "@requires-nip17"),
        )
        self.assert_problem(
            "@live is forbidden",
            BUILT.replace("@ledger-4", "@live"),
        )

    def test_acceptance_requires_built(self):
        self.assert_problem(
            "@acceptance is allowed only",
            SPECIFIED.replace("  Scenario:", "  @acceptance\n  Scenario:"),
        )

    def test_acceptance_must_live_at_and_resolve_to_the_public_facade_owner(self):
        misplaced = BUILT.replace("@ledger-4", "@acceptance")
        self.assert_problem("@acceptance scenario must live under", misplaced)

        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            behavior = root / "crates/nmp/tests/acceptance/cap.feature"
            behavior.parent.mkdir(parents=True)
            behavior.write_text(misplaced, encoding="utf-8")
            _records, problems = CHECKER.validate([behavior], root, resolve=False)
            self.assertTrue(
                any(
                    "@acceptance evidence must be owned by the nmp" in problem.message
                    for problem in problems
                ),
                problems,
            )

    def test_invalid_evidence_locator_is_red(self):
        self.assert_problem(
            "invalid nmp:evidence locator",
            BUILT.replace(
                "rust:nmp-router::coverage_respects_whole_demand_cap",
                "nmp-router/coverage test",
            ),
        )

    def test_unknown_and_duplicate_metadata_are_red(self):
        self.assert_problem(
            "unknown nmp metadata key",
            BUILT.replace(
                "    # nmp:status=built\n",
                "    # nmp:status=built\n    # nmp:owner=router\n",
            ),
        )
        self.assert_problem(
            "duplicate nmp:status",
            BUILT.replace(
                "    # nmp:status=built\n",
                "    # nmp:status=built\n    # nmp:status=built\n",
            ),
        )

    def test_metadata_must_be_contiguous(self):
        self.assert_problem(
            "missing contiguous nmp metadata",
            BUILT.replace(
                "    @ledger-4\n",
                "\n    @ledger-4 @acceptance\n",
            ),
        )

    def test_rust_evidence_resolves_inside_its_owner_crate(self):
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            behavior = root / "crates/nmp-router/tests/behavior/cap.feature"
            behavior.parent.mkdir(parents=True)
            behavior.write_text(BUILT, encoding="utf-8")
            evidence = root / "crates/nmp-router/tests/contract.rs"
            evidence.write_text(
                "#[test]\nfn coverage_respects_whole_demand_cap() {}\n",
                encoding="utf-8",
            )

            _records, problems = CHECKER.validate([behavior], root, resolve=True)
            self.assertEqual([], problems)

            evidence.write_text(
                "#[test]\nfn a_different_test() {}\n", encoding="utf-8"
            )
            _records, problems = CHECKER.validate([behavior], root, resolve=True)
            self.assertTrue(
                any("evidence test" in problem.message for problem in problems),
                problems,
            )


if __name__ == "__main__":
    unittest.main()
