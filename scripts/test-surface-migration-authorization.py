#!/usr/bin/env python3
"""Adversarial tests for reusable protected-governance authorization."""

from __future__ import annotations

import contextlib
import dataclasses
import errno
import importlib.util
import io
import json
import os
import shutil
import subprocess
import sys
import tempfile
import time
import unittest
from pathlib import Path
from typing import Any
from unittest import mock


MODULE_PATH = Path(__file__).with_name("check-surface-migration-authorization.py")
sys.dont_write_bytecode = True
SPEC = importlib.util.spec_from_file_location(
    "surface_migration_authorization",
    MODULE_PATH,
)
assert SPEC is not None and SPEC.loader is not None
authorization = importlib.util.module_from_spec(SPEC)
sys.modules[SPEC.name] = authorization
SPEC.loader.exec_module(authorization)


# The fixtures are real Git repositories on purpose: the program under test
# makes promises about exact 40-character object ids and canonical raw diffs, so
# a mocked repository would prove nothing. Real repositories also mean real
# background work. `git commit` spawns a detached `git maintenance run --auto`
# that keeps writing under `.git/objects` after the foreground command has
# already returned, so a scratch tree can refill between `os.scandir` and
# `os.rmdir` and removal fails with ENOTEMPTY. Two independent mechanisms keep
# that off this suite's exit status: FIXTURE_GIT_ENVIRONMENT stops the
# background work from being started, and remove_scratch_tree refuses to turn a
# removal it cannot complete into a test result.
FIXTURE_GIT_ENVIRONMENT = {
    # Ambient configuration is not an input to a falsifier. Whatever the host,
    # the developer or the CI image has configured, the fixtures see exactly the
    # settings below and nothing else.
    "GIT_CONFIG_GLOBAL": os.devnull,
    "GIT_CONFIG_SYSTEM": os.devnull,
    "GIT_CONFIG_COUNT": "3",
    "GIT_CONFIG_KEY_0": "maintenance.auto",
    "GIT_CONFIG_VALUE_0": "false",
    "GIT_CONFIG_KEY_1": "gc.auto",
    "GIT_CONFIG_VALUE_1": "0",
    "GIT_CONFIG_KEY_2": "init.defaultBranch",
    "GIT_CONFIG_VALUE_2": "master",
}

SCRATCH_REMOVAL_ATTEMPTS = 5
SCRATCH_REMOVAL_BACKOFF_SECONDS = 0.05


# The identity that authorizes a protected governance migration, declared here
# a second time so that changing the program's copy alone turns this suite red.
#
# Every other property the program asserts is downstream of these four. A wrong
# owner id or login means somebody else's approval satisfies the gate; a renamed
# context means a status published under a different name is read as the owner's
# signature; a wrong repository means a pull request, issue and status in some
# other repository are accepted as this one's. None of them are repository
# taste — each is an external fact:
#
#   repository   the repository whose protected paths this program guards
#   owner_login  the account that owns it
#   owner_id     that account's immutable numeric GitHub user id; a login can be
#                renamed and the freed name re-registered, an id cannot
#   context      the exact commit-status context the owner publishes under, and
#                the only name a status is ever looked up by
#
# A second declaration is the only pin available in-repo, so it has to be a
# declaration the program cannot reach: nothing below imports these values from
# AuthorizationPolicy, and the fixtures built from them are handed to the real
# PRODUCTION_POLICY.
PINNED_AUTHORIZING_IDENTITY: dict[str, Any] = {
    "repository": "pablof7z/nmp",
    "context": "nmp/surface-governance-migration",
    "owner_login": "pablof7z",
    "owner_id": 779813,
}

# The remaining policy fields say *which files* are guarded, never *who* may
# authorize a change to them, so they are pinned by inventory tests rather than
# by identity falsifiers.
PINNED_INVENTORY_FIELDS = frozenset({"protected_paths", "protected_prefixes"})

# Fixed synthetic inputs for the byte-stable description below. Real object ids
# would move every run and pin nothing.
GOLDEN_BASE = "1" * 40
GOLDEN_HEAD = "2" * 40
GOLDEN_PATH = ".github/workflows/ci.yml"
GOLDEN_DESCRIPTION = (
    "nmp-governance-v2:"
    "0dcf32c32ca977631dec091110c69205507c530435074c72a477d8630f34d64d"
)


def remove_scratch_tree(root: Path) -> None:
    """Remove a fixture scratch tree without ever raising.

    A scratch tree this suite cannot delete says nothing about the program under
    test. Every assertion has already run by the time this is called, so the
    only honest outcomes are "removed" and "reported on stderr" — never a test
    error that reads like a governance verdict.
    """
    failure: OSError | None = None
    for attempt in range(SCRATCH_REMOVAL_ATTEMPTS):
        try:
            shutil.rmtree(root)
            return
        except FileNotFoundError:
            return
        except OSError as error:
            failure = error
            time.sleep(SCRATCH_REMOVAL_BACKOFF_SECONDS * (attempt + 1))
    print(
        "test-surface-migration-authorization: leaving scratch tree behind, "
        f"{root} could not be removed: {failure}",
        file=sys.stderr,
    )


class ScratchRemovalTests(unittest.TestCase):
    def test_removal_retries_a_tree_that_refills_and_stays_silent(self) -> None:
        attempts: list[Path] = []

        def refill_twice(root: Path) -> None:
            attempts.append(root)
            if len(attempts) < 3:
                raise OSError(errno.ENOTEMPTY, "Directory not empty", str(root))

        stderr = io.StringIO()
        with mock.patch.object(shutil, "rmtree", refill_twice), mock.patch.object(
            time, "sleep", lambda _seconds: None
        ), contextlib.redirect_stderr(stderr):
            remove_scratch_tree(Path("scratch"))

        self.assertEqual(len(attempts), 3)
        self.assertEqual(stderr.getvalue(), "")

    def test_removal_reports_a_stuck_tree_instead_of_raising(self) -> None:
        attempts: list[Path] = []

        def always_refill(root: Path) -> None:
            attempts.append(root)
            raise OSError(errno.ENOTEMPTY, "Directory not empty", str(root))

        stderr = io.StringIO()
        with mock.patch.object(shutil, "rmtree", always_refill), mock.patch.object(
            time, "sleep", lambda _seconds: None
        ), contextlib.redirect_stderr(stderr):
            remove_scratch_tree(Path("scratch"))

        self.assertEqual(len(attempts), SCRATCH_REMOVAL_ATTEMPTS)
        self.assertIn("leaving scratch tree behind", stderr.getvalue())
        self.assertIn("Directory not empty", stderr.getvalue())


class MigrationAuthorizationTests(unittest.TestCase):
    def setUp(self) -> None:
        self.root = Path(tempfile.mkdtemp(prefix="surface-migration-authorization-"))
        self.addCleanup(remove_scratch_tree, self.root)
        self.git("init", "-q")
        # Repo-local settings so every git process that touches this fixture is
        # bound by them, including the ones the program under test spawns.
        self.git("config", "maintenance.auto", "false")
        self.git("config", "gc.auto", "0")
        self.git("config", "user.email", "surface@example.invalid")
        self.git("config", "user.name", "SurfaceTest")

        self.workflow = ".github/workflows/architecture-gates.yml"
        self.verifier = "scripts/check-surface-migration-authorization.py"
        self.outside_old_shell_pattern = "governance/future-check.py"
        self.tool_prefix = "tools/behavior-traceability/"
        self.tool_manifest = f"{self.tool_prefix}Cargo.toml"
        self.tool_source = f"{self.tool_prefix}src/lib.rs"
        self.write(self.workflow, "name: Base architecture gates\n")
        self.write(self.verifier, "def verify():\n    return False\n")
        self.write(self.outside_old_shell_pattern, "def check():\n    return False\n")
        self.write(self.tool_manifest, "[package]\nname = \"behavior-traceability\"\n")
        self.write(self.tool_source, "pub fn validate() -> bool { false }\n")
        self.write("ordinary.txt", "base\n")
        self.git("add", "-A")
        self.git("commit", "-qm", "base")
        self.base = self.git("rev-parse", "HEAD")

        self.write(self.workflow, "name: Migrated architecture gates\n")
        self.write(self.tool_source, "pub fn validate() -> bool { true }\n")
        self.write("docs/migration.md", "Exact migration documentation.\n")
        self.git("add", "-A")
        self.git("commit", "-qm", "first migration")
        self.head = self.git("rev-parse", "HEAD")

        self.policy = authorization.AuthorizationPolicy(
            **PINNED_AUTHORIZING_IDENTITY,
            protected_paths=(
                self.workflow,
                self.verifier,
                self.outside_old_shell_pattern,
            ),
            protected_prefixes=(self.tool_prefix,),
        )
        self.pr_number = 1200
        self.issue_number = 1074
        self.pr_url = (
            f"https://github.com/{self.policy.repository}/pull/{self.pr_number}"
        )
        self.snapshot = authorization.canonical_diff(
            self.root,
            base=self.base,
            head=self.head,
        )
        self.description = authorization.authorization_description(
            self.policy,
            root=self.root,
            base=self.base,
            head=self.head,
            pr_number=self.pr_number,
            issue_number=self.issue_number,
            snapshot=self.snapshot,
        )
        self.pull_request = self.make_pull_request()
        self.issue = self.make_issue()
        self.statuses = [self.make_status()]

    def git(self, *args: str) -> str:
        return subprocess.check_output(
            ["git", "-C", str(self.root), *args],
            text=True,
            env={**os.environ, **FIXTURE_GIT_ENVIRONMENT},
        ).strip()

    def write(self, relative: str, content: str) -> None:
        path = self.root / relative
        path.parent.mkdir(parents=True, exist_ok=True)
        path.write_text(content, encoding="utf-8")

    def commit(self, message: str) -> str:
        self.git("add", "-A")
        self.git("commit", "-qm", message)
        return self.git("rev-parse", "HEAD")

    def make_pull_request(
        self,
        *,
        base: str | None = None,
        head: str | None = None,
        pr_number: int | None = None,
        state: str = "open",
        merged: bool = False,
        repository: str | None = None,
    ) -> dict[str, Any]:
        actual_pr = pr_number if pr_number is not None else self.pr_number
        actual_repository = repository or self.policy.repository
        return {
            "number": actual_pr,
            "state": state,
            "merged": merged,
            "html_url": f"https://github.com/{actual_repository}/pull/{actual_pr}",
            "base": {
                "sha": base or self.base,
                "repo": {"full_name": actual_repository},
            },
            "head": {
                "sha": head or self.head,
                "repo": {"full_name": actual_repository},
            },
        }

    def make_issue(
        self,
        *,
        issue_number: int | None = None,
        state: str = "open",
        repository: str | None = None,
        pull_request: bool = False,
    ) -> dict[str, Any]:
        actual_issue = (
            issue_number if issue_number is not None else self.issue_number
        )
        actual_repository = repository or self.policy.repository
        record: dict[str, Any] = {
            "number": actual_issue,
            "state": state,
            "html_url": (
                f"https://github.com/{actual_repository}/issues/{actual_issue}"
            ),
            "repository_url": f"https://api.github.com/repos/{actual_repository}",
        }
        if pull_request:
            record["pull_request"] = {
                "url": (
                    f"https://api.github.com/repos/{actual_repository}"
                    f"/pulls/{actual_issue}"
                )
            }
        return record

    def make_status(
        self,
        *,
        description: str | None = None,
        head: str | None = None,
        repository: str | None = None,
        issue_number: int | None = None,
        context: str | None = None,
        state: str = "success",
        login: str = PINNED_AUTHORIZING_IDENTITY["owner_login"],
        owner_id: int = PINNED_AUTHORIZING_IDENTITY["owner_id"],
        identifier: int = 10,
        created_at: str = "2026-07-30T23:59:00Z",
        target_url: str | None = None,
        status_url: str | None = None,
    ) -> dict[str, Any]:
        actual_issue = (
            issue_number if issue_number is not None else self.issue_number
        )
        actual_repository = repository or self.policy.repository
        actual_head = head or self.head
        return {
            "id": identifier,
            "created_at": created_at,
            "url": status_url
            or f"https://api.github.com/repos/{actual_repository}/statuses/{actual_head}",
            "state": state,
            "context": context or self.policy.context,
            "description": description or self.description,
            "target_url": target_url
            or authorization.issue_target_url(self.policy, actual_issue),
            "creator": {"login": login, "id": owner_id, "type": "User"},
        }

    def verify(
        self,
        *,
        base: str | None = None,
        head: str | None = None,
        pr_number: int | None = None,
        pr_url: str | None = None,
        pull_request: Any | None = None,
        issue: Any | None = None,
        statuses: Any | None = None,
        snapshot: Any | None = None,
    ) -> tuple[str, str]:
        return authorization.verify_authorization(
            self.policy,
            root=self.root,
            base=base or self.base,
            head=head or self.head,
            pr_number=pr_number or self.pr_number,
            pr_url=pr_url or self.pr_url,
            pull_request_record=(
                self.pull_request if pull_request is None else pull_request
            ),
            issue_record=self.issue if issue is None else issue,
            status_records=self.statuses if statuses is None else statuses,
            snapshot=self.snapshot if snapshot is None else snapshot,
        )

    def authorization_for(
        self,
        *,
        base: str,
        head: str,
        pr_number: int,
        issue_number: int,
    ) -> tuple[Any, str, dict[str, Any], dict[str, Any], list[dict[str, Any]]]:
        snapshot = authorization.canonical_diff(self.root, base=base, head=head)
        description = authorization.authorization_description(
            self.policy,
            root=self.root,
            base=base,
            head=head,
            pr_number=pr_number,
            issue_number=issue_number,
            snapshot=snapshot,
        )
        pull_request = self.make_pull_request(
            base=base,
            head=head,
            pr_number=pr_number,
        )
        issue = self.make_issue(issue_number=issue_number)
        status = self.make_status(
            description=description,
            head=head,
            issue_number=issue_number,
        )
        return snapshot, description, pull_request, issue, [status]

    def pinned_identity_records(
        self,
        policy: Any,
    ) -> tuple[str, str, dict[str, Any], dict[str, Any], list[dict[str, Any]]]:
        """A complete, correct migration authorization by the pinned identity.

        Every record is addressed to PINNED_AUTHORIZING_IDENTITY and never to
        `policy`, so it describes the identity that is supposed to authorize
        rather than the one the program currently believes in. The description is
        the one `policy` itself computes, so the tuple binding is always
        satisfied and the identity comparison is the only thing left to decide
        the verdict.
        """
        repository = PINNED_AUTHORIZING_IDENTITY["repository"]
        pr_url = f"https://github.com/{repository}/pull/{self.pr_number}"
        issue_url = f"https://github.com/{repository}/issues/{self.issue_number}"
        description = authorization.authorization_description(
            policy,
            root=self.root,
            base=self.base,
            head=self.head,
            pr_number=self.pr_number,
            issue_number=self.issue_number,
            snapshot=self.snapshot,
        )
        pull_request = {
            "number": self.pr_number,
            "state": "open",
            "merged": False,
            "html_url": pr_url,
            "base": {"sha": self.base, "repo": {"full_name": repository}},
            "head": {"sha": self.head, "repo": {"full_name": repository}},
        }
        issue = {
            "number": self.issue_number,
            "state": "open",
            "html_url": issue_url,
            "repository_url": f"https://api.github.com/repos/{repository}",
        }
        status = {
            "id": 10,
            "created_at": "2026-07-30T23:59:00Z",
            "url": f"https://api.github.com/repos/{repository}/statuses/{self.head}",
            "state": "success",
            "context": PINNED_AUTHORIZING_IDENTITY["context"],
            "description": description,
            "target_url": issue_url,
            "creator": {
                "login": PINNED_AUTHORIZING_IDENTITY["owner_login"],
                "id": PINNED_AUTHORIZING_IDENTITY["owner_id"],
                "type": "User",
            },
        }
        return pr_url, description, pull_request, issue, [status]

    def test_fixture_repository_is_sealed_against_ambient_git_configuration(
        self,
    ) -> None:
        hostile_home = Path(tempfile.mkdtemp(prefix="surface-hostile-gitconfig-"))
        self.addCleanup(remove_scratch_tree, hostile_home)
        hostile = hostile_home / "gitconfig"
        hostile.write_text(
            "[maintenance]\n\tauto = true\n"
            "[gc]\n\tauto = 1\n\tautoDetach = true\n"
            "[init]\n\tdefaultBranch = ambient\n",
            encoding="utf-8",
        )
        ambient = {
            key: value
            for key, value in os.environ.items()
            if not key.startswith("GIT_CONFIG")
        }
        ambient["GIT_CONFIG_GLOBAL"] = str(hostile)
        ambient["GIT_CONFIG_SYSTEM"] = str(hostile)

        # This suite's own git calls carry the pin, so ambient settings lose.
        with mock.patch.dict(os.environ, ambient, clear=True):
            self.assertEqual(
                self.git("config", "--get", "init.defaultBranch"),
                "master",
            )
            self.assertEqual(
                self.git("config", "--get", "maintenance.auto"),
                "false",
            )

        # A git process the program under test spawns carries no pin, so the
        # repository's own configuration has to be what disables background work.
        for key, expected in (("maintenance.auto", "false"), ("gc.auto", "0")):
            self.assertEqual(
                subprocess.check_output(
                    ["git", "-C", str(self.root), "config", "--get", key],
                    text=True,
                    env=ambient,
                ).strip(),
                expected,
            )

    def test_production_policy_protects_the_complete_program_and_itself(self) -> None:
        policy = authorization.PRODUCTION_POLICY
        for path in (
            ".github/workflows/architecture-gates.yml",
            ".github/workflows/ci.yml",
            ".github/workflows/surface-governance.yml",
            "scripts/check-surface-migration-authorization.py",
            "scripts/check-surface-governance.sh",
            "scripts/report-surface-governance-verdict.sh",
            "scripts/run-surface-regeneration-governance.sh",
            "scripts/test-surface-migration-authorization.py",
            "scripts/test-surface-governance.sh",
        ):
            self.assertIn(path, policy.protected_paths)
        self.assertIn("tools/behavior-traceability/", policy.protected_prefixes)
        authorization.require_well_formed_policy(policy)

    def test_every_policy_field_is_classified_as_identity_or_inventory(self) -> None:
        """The rule that keeps the falsifiers below complete.

        A field added to AuthorizationPolicy is invisible to a suite that only
        knows the fields it was written against, so the classification is
        asserted rather than assumed. An unclassified field fails here. Pinning
        it as identity puts it straight into
        test_every_pinned_identity_field_decides_the_verdict, which iterates this
        mapping; the one manual step left is to address it in the fixture that
        pinned_identity_records builds.
        """
        self.assertEqual(
            {
                field.name
                for field in dataclasses.fields(authorization.AuthorizationPolicy)
            },
            set(PINNED_AUTHORIZING_IDENTITY) | PINNED_INVENTORY_FIELDS,
            "AuthorizationPolicy gained or lost a field. Decide which it is: "
            "an identity field goes in PINNED_AUTHORIZING_IDENTITY with its "
            "external value, an inventory field goes in PINNED_INVENTORY_FIELDS "
            "and is pinned by the protected-path tests instead.",
        )

    def test_production_policy_authorizes_only_the_pinned_identity(self) -> None:
        """Falsifier for every identity constant in PRODUCTION_POLICY.

        The fixture is a real migration authorized by the pinned identity, run
        through the real verification path against the real PRODUCTION_POLICY. If
        the program's owner id, owner login, status context or repository is
        changed, this authorization stops being accepted and this test fails.
        """
        self.assertTrue(
            authorization.protected_paths_changed(
                authorization.PRODUCTION_POLICY,
                self.snapshot.entries,
            ),
            "fixture no longer changes a path PRODUCTION_POLICY protects",
        )
        pr_url, description, pull_request, issue, statuses = (
            self.pinned_identity_records(authorization.PRODUCTION_POLICY)
        )
        self.assertEqual(
            authorization.verify_authorization(
                authorization.PRODUCTION_POLICY,
                root=self.root,
                base=self.base,
                head=self.head,
                pr_number=self.pr_number,
                pr_url=pr_url,
                pull_request_record=pull_request,
                issue_record=issue,
                status_records=statuses,
                snapshot=self.snapshot,
            ),
            (description, issue["html_url"]),
        )

    def test_every_pinned_identity_field_decides_the_verdict(self) -> None:
        """The companion that stops the test above from passing vacuously.

        Accepting the pinned identity only proves something if each field of it
        is actually compared. One field at a time is moved to a neighbouring
        value that is still a well-formed policy, and the same authorization
        must then be refused.
        """
        for field, pinned in sorted(PINNED_AUTHORIZING_IDENTITY.items()):
            with self.subTest(field=field):
                neighbour = (
                    pinned + 1 if isinstance(pinned, int) else f"{pinned}-neighbour"
                )
                self.assertNotEqual(neighbour, pinned)
                policy = dataclasses.replace(
                    authorization.PRODUCTION_POLICY,
                    **{field: neighbour},
                )
                authorization.require_well_formed_policy(policy)
                pr_url, _, pull_request, issue, statuses = (
                    self.pinned_identity_records(policy)
                )
                with self.assertRaises(authorization.AuthorizationError):
                    authorization.verify_authorization(
                        policy,
                        root=self.root,
                        base=self.base,
                        head=self.head,
                        pr_number=self.pr_number,
                        pr_url=pr_url,
                        pull_request_record=pull_request,
                        issue_record=issue,
                        status_records=statuses,
                        snapshot=self.snapshot,
                    )

    def test_authorization_description_is_byte_stable(self) -> None:
        """Pins the two constants that are not identity but are equally silent.

        The domain separator and the digest prefix decide what a published status
        has to say, and changing either invalidates every status the owner has
        already created. That direction is fail-closed rather than fail-open, so
        it is not an identity hole — but nothing else notices it, and a
        migration of the wire format should be a deliberate edit here rather
        than a silent one over there. The inputs are synthetic and fixed so the
        digest is a constant; recompute it only when the format is meant to
        change.
        """
        snapshot = authorization.DiffSnapshot(
            merge_base=GOLDEN_BASE,
            entries=(
                authorization.DiffEntry(
                    status="M",
                    path=GOLDEN_PATH,
                    old_mode="100644",
                    new_mode="100644",
                    old_oid="3" * 40,
                    new_oid="4" * 40,
                ),
            ),
        )
        self.assertIn(GOLDEN_PATH, authorization.PRODUCTION_POLICY.protected_paths)
        self.assertEqual(
            authorization.authorization_description(
                authorization.PRODUCTION_POLICY,
                root=self.root,
                base=GOLDEN_BASE,
                head=GOLDEN_HEAD,
                pr_number=1200,
                issue_number=1074,
                snapshot=snapshot,
            ),
            GOLDEN_DESCRIPTION,
        )

    def test_exact_owner_authorization_passes_on_repeat_reruns(self) -> None:
        expected = (
            self.description,
            authorization.issue_target_url(self.policy, self.issue_number),
        )
        self.assertEqual(self.verify(), expected)
        self.assertEqual(self.verify(), expected)

    def test_real_api_shaped_status_binds_the_exact_head_by_canonical_url(self) -> None:
        status = self.make_status()
        self.assertNotIn("sha", status)
        self.assertEqual(
            status["url"],
            (
                "https://api.github.com/repos/"
                f"{self.policy.repository}/statuses/{self.head}"
            ),
        )
        self.verify(statuses=[status])

    def test_status_url_must_bind_the_exact_repository_and_head(self) -> None:
        wrong_urls = (
            None,
            "",
            f"https://api.github.com/repos/{self.policy.repository}/statuses/{'0' * 40}",
            f"https://api.github.com/repos/other/repository/statuses/{self.head}",
            f"https://github.com/{self.policy.repository}/statuses/{self.head}",
        )
        for status_url in wrong_urls:
            with self.subTest(status_url=status_url):
                status = self.make_status()
                if status_url is None:
                    status.pop("url")
                else:
                    status["url"] = status_url
                with self.assertRaises(authorization.AuthorizationError):
                    self.verify(statuses=[status])

        answer_injected = self.make_status()
        answer_injected.pop("url")
        answer_injected["sha"] = self.head
        with self.assertRaises(authorization.AuthorizationError):
            self.verify(statuses=[answer_injected])

    def test_raw_diff_binds_complete_diff_and_merge_base(self) -> None:
        self.assertEqual(self.snapshot.merge_base, self.base)
        entries = {entry.path: entry for entry in self.snapshot.entries}
        self.assertEqual(
            set(entries),
            {self.workflow, self.tool_source, "docs/migration.md"},
        )
        self.assertEqual(entries["docs/migration.md"].status, "A")
        self.assertEqual(entries["docs/migration.md"].old_oid, "absent")
        self.assertNotEqual(entries["docs/migration.md"].new_oid, "absent")
        self.assertTrue(
            authorization.protected_paths_changed(
                self.policy,
                self.snapshot.entries,
            )
        )

    def test_cli_prints_and_verifies_exact_status_tuple(self) -> None:
        pull_record = self.root / "pull-request.json"
        issue_record = self.root / "issue.json"
        status_records = self.root / "statuses.json"
        pull_record.write_text(json.dumps(self.pull_request), encoding="utf-8")
        issue_record.write_text(json.dumps(self.issue), encoding="utf-8")
        status_records.write_text(json.dumps(self.statuses), encoding="utf-8")
        common = [
            "--root",
            str(self.root),
            "--base",
            self.base,
            "--head",
            self.head,
            "--pr-number",
            str(self.pr_number),
        ]
        with mock.patch.object(authorization, "PRODUCTION_POLICY", self.policy):
            output = io.StringIO()
            with contextlib.redirect_stdout(output):
                self.assertEqual(
                    authorization.main(
                        common
                        + [
                            "print-status",
                            "--issue-number",
                            str(self.issue_number),
                        ]
                    ),
                    0,
                )
            self.assertEqual(
                output.getvalue(),
                (
                    f"context={self.policy.context}\n"
                    f"description={self.description}\n"
                    "target_url="
                    f"{authorization.issue_target_url(self.policy, self.issue_number)}\n"
                ),
            )
            output = io.StringIO()
            with contextlib.redirect_stdout(output):
                self.assertEqual(
                    authorization.main(
                        common
                        + [
                            "verify",
                            "--pr-url",
                            self.pr_url,
                            "--pull-request-record",
                            str(pull_record),
                            "--issue-record",
                            str(issue_record),
                            "--status-records",
                            str(status_records),
                        ]
                    ),
                    0,
                )

    def test_cli_exit_code_separates_verdict_staleness_and_malfunction(self) -> None:
        """#1264: a red must say which of three things happened.

        The verifier is the innermost place where "your change is not
        authorized" and "I could not tell" shared one exit code, so the split is
        asserted on the exit code itself and never on message text. Every case
        here still exits nonzero: none of it makes the gate fail open.
        """
        pull_record = self.root / "pull-request.json"
        issue_record = self.root / "issue.json"
        status_records = self.root / "statuses.json"
        pull_record.write_text(json.dumps(self.pull_request), encoding="utf-8")
        issue_record.write_text(json.dumps(self.issue), encoding="utf-8")

        def run(*, base: str | None = None, root: Path | None = None) -> int:
            with mock.patch.object(authorization, "PRODUCTION_POLICY", self.policy):
                with contextlib.redirect_stdout(io.StringIO()):
                    with contextlib.redirect_stderr(io.StringIO()):
                        return authorization.main(
                            [
                                "--root",
                                str(self.root if root is None else root),
                                "--base",
                                base or self.base,
                                "--head",
                                self.head,
                                "--pr-number",
                                str(self.pr_number),
                                "verify",
                                "--pr-url",
                                self.pr_url,
                                "--pull-request-record",
                                str(pull_record),
                                "--issue-record",
                                str(issue_record),
                                "--status-records",
                                str(status_records),
                            ]
                        )

        # A verdict: the head is protected and no owner status authorizes it.
        status_records.write_text("[]", encoding="utf-8")
        self.assertEqual(run(), authorization.VERDICT_EXIT)

        # Authorized: the same call with the exact owner status.
        status_records.write_text(json.dumps(self.statuses), encoding="utf-8")
        self.assertEqual(run(), 0)

        # Malfunction: the fetched record is unreadable, so nothing was judged.
        status_records.unlink()
        self.assertEqual(run(), authorization.MALFUNCTION_EXIT)
        status_records.write_text("{not json", encoding="utf-8")
        self.assertEqual(run(), authorization.MALFUNCTION_EXIT)
        status_records.write_text(json.dumps(self.statuses), encoding="utf-8")

        # Malfunction: Git itself fails, so no diff exists to judge.
        outside = Path(tempfile.mkdtemp(prefix="surface-not-a-repository-"))
        self.addCleanup(remove_scratch_tree, outside)
        self.assertEqual(run(root=outside), authorization.MALFUNCTION_EXIT)

        # Malfunction: an unhandled defect in the verifier is never a rejection.
        with mock.patch.object(
            authorization,
            "verify_authorization",
            side_effect=ZeroDivisionError("defect"),
        ):
            self.assertEqual(run(), authorization.MALFUNCTION_EXIT)

        # Staleness: the head is not on the current base, so again nothing about
        # it was judged. Committed last because it moves the fixture's branch.
        self.git("checkout", "-q", "-b", "advanced-base", self.base)
        self.write("base-only.txt", "base advanced\n")
        advanced_base = self.commit("advance base")
        self.assertEqual(run(base=advanced_base), authorization.STALE_BASE_EXIT)

        self.assertEqual(
            len(
                {
                    0,
                    authorization.VERDICT_EXIT,
                    authorization.UNPROTECTED_EXIT,
                    authorization.STALE_BASE_EXIT,
                    authorization.MALFUNCTION_EXIT,
                }
            ),
            5,
            "the five outcomes must not share an exit code",
        )

    def test_unrelated_ordinary_diff_does_not_activate(self) -> None:
        self.git("checkout", "-q", "-b", "ordinary", self.base)
        self.write("ordinary.txt", "ordinary edit\n")
        ordinary_head = self.commit("ordinary")
        snapshot = authorization.canonical_diff(
            self.root,
            base=self.base,
            head=ordinary_head,
        )
        self.assertFalse(
            authorization.protected_paths_changed(self.policy, snapshot.entries)
        )

    def test_spec_protected_path_outside_old_shell_patterns_activates(self) -> None:
        self.git("checkout", "-q", self.base)
        self.write(self.outside_old_shell_pattern, "def check():\n    return True\n")
        head = self.commit("outside old shell pattern")
        snapshot = authorization.canonical_diff(self.root, base=self.base, head=head)
        self.assertTrue(
            authorization.protected_paths_changed(self.policy, snapshot.entries)
        )
        with self.assertRaises(authorization.AuthorizationError):
            authorization.verify_authorization(
                self.policy,
                root=self.root,
                base=self.base,
                head=head,
                pr_number=self.pr_number,
                pr_url=self.pr_url,
                pull_request_record=self.make_pull_request(head=head),
                issue_record=self.issue,
                status_records=[],
                snapshot=snapshot,
            )

    def test_extra_ordinary_path_cannot_hitchhike_on_old_status(self) -> None:
        self.write("extra.txt", "hitchhiker\n")
        new_head = self.commit("extra ordinary path")
        snapshot = authorization.canonical_diff(
            self.root,
            base=self.base,
            head=new_head,
        )
        forged = self.make_status(description=self.description, head=new_head)
        with self.assertRaises(authorization.AuthorizationError):
            self.verify(
                head=new_head,
                pull_request=self.make_pull_request(head=new_head),
                statuses=[forged],
                snapshot=snapshot,
            )

    def test_stale_non_ancestor_head_cannot_be_authorized(self) -> None:
        self.git("checkout", "-q", "-b", "advanced-base", self.base)
        self.write("base-only.txt", "base advanced\n")
        advanced_base = self.commit("advance base")
        snapshot = authorization.canonical_diff(
            self.root,
            base=advanced_base,
            head=self.head,
        )
        self.assertEqual(snapshot.merge_base, self.base)
        self.assertNotEqual(snapshot.merge_base, advanced_base)
        with self.assertRaisesRegex(
            authorization.AuthorizationError,
            "not descended from the current PR base",
        ):
            authorization.authorization_description(
                self.policy,
                root=self.root,
                base=advanced_base,
                head=self.head,
                pr_number=self.pr_number,
                issue_number=self.issue_number,
                snapshot=snapshot,
            )

    def test_changed_blob_cannot_reuse_old_status(self) -> None:
        self.write(self.tool_source, "pub fn validate() -> bool { false }\n")
        new_head = self.commit("change protected blob")
        snapshot = authorization.canonical_diff(
            self.root,
            base=self.base,
            head=new_head,
        )
        forged = self.make_status(description=self.description, head=new_head)
        with self.assertRaises(authorization.AuthorizationError):
            self.verify(
                head=new_head,
                pull_request=self.make_pull_request(head=new_head),
                statuses=[forged],
                snapshot=snapshot,
            )

    def test_mode_change_cannot_reuse_old_status(self) -> None:
        self.git("update-index", "--chmod=+x", self.verifier)
        self.git("commit", "-qm", "change protected mode")
        new_head = self.git("rev-parse", "HEAD")
        snapshot = authorization.canonical_diff(
            self.root,
            base=self.base,
            head=new_head,
        )
        entry = next(item for item in snapshot.entries if item.path == self.verifier)
        self.assertNotEqual(entry.old_mode, entry.new_mode)
        forged = self.make_status(description=self.description, head=new_head)
        with self.assertRaises(authorization.AuthorizationError):
            self.verify(
                head=new_head,
                pull_request=self.make_pull_request(head=new_head),
                statuses=[forged],
                snapshot=snapshot,
            )

    def test_exact_owner_can_authorize_full_protected_prefix_deletion(self) -> None:
        self.git("rm", "-qr", self.tool_prefix.removesuffix("/"))
        new_head = self.commit("delete protected tool")
        snapshot, description, pull_request, issue, statuses = self.authorization_for(
            base=self.base,
            head=new_head,
            pr_number=self.pr_number,
            issue_number=self.issue_number,
        )
        prefix_states = authorization.affected_prefix_states(
            self.policy,
            root=self.root,
            head=new_head,
            entries=snapshot.entries,
        )
        self.assertEqual(prefix_states, ((self.tool_prefix, authorization.ABSENT),))
        self.assertEqual(
            authorization.verify_authorization(
                self.policy,
                root=self.root,
                base=self.base,
                head=new_head,
                pr_number=self.pr_number,
                pr_url=self.pr_url,
                pull_request_record=pull_request,
                issue_record=issue,
                status_records=statuses,
                snapshot=snapshot,
            ),
            (
                description,
                authorization.issue_target_url(self.policy, self.issue_number),
            ),
        )

    def test_deleted_prefix_root_blob_and_symlink_activate_and_bind_state(self) -> None:
        self.git("rm", "-qr", self.tool_prefix.removesuffix("/"))
        deleted_head = self.commit("delete protected prefix")
        (
            _,
            deletion_description,
            _,
            _,
            _,
        ) = self.authorization_for(
            base=self.base,
            head=deleted_head,
            pr_number=self.pr_number,
            issue_number=self.issue_number,
        )
        root_path = self.root / self.tool_prefix.removesuffix("/")

        for label, mode in (("blob", "100644"), ("symlink", "120000")):
            with self.subTest(label=label):
                self.git("reset", "--hard", "-q", deleted_head)
                root_path.parent.mkdir(parents=True, exist_ok=True)
                if label == "blob":
                    root_path.write_text(
                        "proposed governance payload\n",
                        encoding="utf-8",
                    )
                else:
                    root_path.symlink_to("proposed-governance-payload")
                replacement_head = self.commit(f"replace deleted prefix with {label}")
                snapshot = authorization.canonical_diff(
                    self.root,
                    base=deleted_head,
                    head=replacement_head,
                )
                self.assertEqual(
                    [(entry.status, entry.path) for entry in snapshot.entries],
                    [("A", self.tool_prefix.removesuffix("/"))],
                )
                root_entry = snapshot.entries[0]
                self.assertTrue(
                    authorization.protected_paths_changed(
                        self.policy,
                        snapshot.entries,
                    )
                )
                prefix_states = authorization.affected_prefix_states(
                    self.policy,
                    root=self.root,
                    head=replacement_head,
                    entries=snapshot.entries,
                )
                self.assertEqual(len(prefix_states), 1)
                prefix, state = prefix_states[0]
                self.assertEqual(prefix, self.tool_prefix)
                self.assertEqual(state.mode, mode)
                self.assertEqual(state.object_type, "blob")
                self.assertEqual(state.oid, root_entry.new_oid)

                stale_status = self.make_status(
                    description=deletion_description,
                    head=replacement_head,
                )
                with self.assertRaises(authorization.AuthorizationError):
                    authorization.verify_authorization(
                        self.policy,
                        root=self.root,
                        base=deleted_head,
                        head=replacement_head,
                        pr_number=self.pr_number,
                        pr_url=self.pr_url,
                        pull_request_record=self.make_pull_request(
                            base=deleted_head,
                            head=replacement_head,
                        ),
                        issue_record=self.issue,
                        status_records=[stale_status],
                        snapshot=snapshot,
                    )

    def test_protected_prefix_tree_to_blob_replacement_activates(self) -> None:
        self.git("checkout", "-q", self.base)
        self.git("rm", "-qr", self.tool_prefix.removesuffix("/"))
        root_path = self.root / self.tool_prefix.removesuffix("/")
        root_path.parent.mkdir(parents=True, exist_ok=True)
        root_path.write_text("replacement blob\n", encoding="utf-8")
        replacement_head = self.commit("replace protected tree with blob")
        snapshot = authorization.canonical_diff(
            self.root,
            base=self.base,
            head=replacement_head,
        )
        self.assertTrue(
            authorization.protected_paths_changed(
                self.policy,
                snapshot.entries,
            )
        )
        prefix_states = authorization.affected_prefix_states(
            self.policy,
            root=self.root,
            head=replacement_head,
            entries=snapshot.entries,
        )
        self.assertEqual(len(prefix_states), 1)
        self.assertEqual(prefix_states[0][0], self.tool_prefix)
        self.assertEqual(prefix_states[0][1].mode, "100644")
        self.assertEqual(prefix_states[0][1].object_type, "blob")
        with self.assertRaises(authorization.AuthorizationError):
            authorization.verify_authorization(
                self.policy,
                root=self.root,
                base=self.base,
                head=replacement_head,
                pr_number=self.pr_number,
                pr_url=self.pr_url,
                pull_request_record=self.make_pull_request(head=replacement_head),
                issue_record=self.issue,
                status_records=[],
                snapshot=snapshot,
            )

    def test_deleted_exact_path_descendant_add_activates_and_refuses_replay(self) -> None:
        self.git("rm", "-q", self.verifier)
        deleted_head = self.commit("delete protected exact path")
        (
            deletion_snapshot,
            deletion_description,
            deletion_pull,
            deletion_issue,
            deletion_statuses,
        ) = self.authorization_for(
            base=self.base,
            head=deleted_head,
            pr_number=self.pr_number,
            issue_number=self.issue_number,
        )
        self.assertEqual(
            authorization.verify_authorization(
                self.policy,
                root=self.root,
                base=self.base,
                head=deleted_head,
                pr_number=self.pr_number,
                pr_url=self.pr_url,
                pull_request_record=deletion_pull,
                issue_record=deletion_issue,
                status_records=deletion_statuses,
                snapshot=deletion_snapshot,
            ),
            (
                deletion_description,
                authorization.issue_target_url(self.policy, self.issue_number),
            ),
        )

        descendant = f"{self.verifier}/payload"
        self.write(descendant, "proposed governance payload\n")
        descendant_head = self.commit("add descendant below deleted exact path")
        snapshot = authorization.canonical_diff(
            self.root,
            base=deleted_head,
            head=descendant_head,
        )
        self.assertEqual(
            [(entry.status, entry.path) for entry in snapshot.entries],
            [("A", descendant)],
        )
        self.assertTrue(
            authorization.protected_paths_changed(
                self.policy,
                snapshot.entries,
            )
        )
        stale_status = self.make_status(
            description=deletion_description,
            head=descendant_head,
        )
        with self.assertRaises(authorization.AuthorizationError):
            authorization.verify_authorization(
                self.policy,
                root=self.root,
                base=deleted_head,
                head=descendant_head,
                pr_number=self.pr_number,
                pr_url=self.pr_url,
                pull_request_record=self.make_pull_request(
                    base=deleted_head,
                    head=descendant_head,
                ),
                issue_record=self.issue,
                status_records=[stale_status],
                snapshot=snapshot,
            )

    def test_blob_to_tree_then_descendant_edit_stays_protected(self) -> None:
        self.git("checkout", "-q", self.base)
        self.git("rm", "-q", self.verifier)
        descendant = f"{self.verifier}/payload"
        self.write(descendant, "first protected payload\n")
        replacement_head = self.commit("replace protected blob with tree")
        (
            replacement_snapshot,
            replacement_description,
            replacement_pull,
            replacement_issue,
            replacement_statuses,
        ) = self.authorization_for(
            base=self.base,
            head=replacement_head,
            pr_number=self.pr_number,
            issue_number=self.issue_number,
        )
        self.assertTrue(
            authorization.protected_paths_changed(
                self.policy,
                replacement_snapshot.entries,
            )
        )
        authorization.verify_authorization(
            self.policy,
            root=self.root,
            base=self.base,
            head=replacement_head,
            pr_number=self.pr_number,
            pr_url=self.pr_url,
            pull_request_record=replacement_pull,
            issue_record=replacement_issue,
            status_records=replacement_statuses,
            snapshot=replacement_snapshot,
        )

        self.write(descendant, "edited protected payload\n")
        edited_head = self.commit("edit descendant below protected exact path")
        edited_snapshot = authorization.canonical_diff(
            self.root,
            base=replacement_head,
            head=edited_head,
        )
        self.assertEqual(
            [(entry.status, entry.path) for entry in edited_snapshot.entries],
            [("M", descendant)],
        )
        self.assertTrue(
            authorization.protected_paths_changed(
                self.policy,
                edited_snapshot.entries,
            )
        )
        stale_status = self.make_status(
            description=replacement_description,
            head=edited_head,
        )
        with self.assertRaises(authorization.AuthorizationError):
            authorization.verify_authorization(
                self.policy,
                root=self.root,
                base=replacement_head,
                head=edited_head,
                pr_number=self.pr_number,
                pr_url=self.pr_url,
                pull_request_record=self.make_pull_request(
                    base=replacement_head,
                    head=edited_head,
                ),
                issue_record=self.issue,
                status_records=[stale_status],
                snapshot=edited_snapshot,
            )

    def test_delete_rename_and_copy_cannot_reuse_an_old_status(self) -> None:
        cases: list[tuple[str, Any]] = [
            ("delete", lambda: self.git("rm", "-q", self.workflow)),
            (
                "rename",
                lambda: self.git(
                    "mv",
                    self.tool_source,
                    f"{self.tool_prefix}src/core.rs",
                ),
            ),
            (
                "copy",
                lambda: self.write(
                    f"{self.tool_prefix}src/copied.rs",
                    (self.root / self.tool_source).read_text(encoding="utf-8"),
                ),
            ),
        ]
        for label, mutate in cases:
            with self.subTest(label=label):
                self.git("reset", "--hard", "-q", self.head)
                mutate()
                new_head = self.commit(f"{label} protected entry")
                snapshot = authorization.canonical_diff(
                    self.root,
                    base=self.base,
                    head=new_head,
                )
                statuses = {entry.status for entry in snapshot.entries}
                if label == "rename":
                    self.assertIn("A", statuses)
                    self.assertIn("D", statuses)
                forged = self.make_status(description=self.description, head=new_head)
                with self.assertRaises(authorization.AuthorizationError):
                    self.verify(
                        head=new_head,
                        pull_request=self.make_pull_request(head=new_head),
                        statuses=[forged],
                        snapshot=snapshot,
                    )

    def test_wrong_pr_base_head_creator_context_state_and_digest_fail(self) -> None:
        wrong_pr = self.make_pull_request(pr_number=self.pr_number + 1)
        with self.assertRaises(authorization.AuthorizationError):
            self.verify(pull_request=wrong_pr)
        wrong_base = self.make_pull_request(base="a" * 40)
        with self.assertRaises(authorization.AuthorizationError):
            self.verify(pull_request=wrong_base)
        wrong_head = self.make_pull_request(head="b" * 40)
        with self.assertRaises(authorization.AuthorizationError):
            self.verify(pull_request=wrong_head)
        for status in (
            self.make_status(login="contributor"),
            self.make_status(owner_id=1),
            self.make_status(context="nmp/other"),
            self.make_status(state="failure"),
            self.make_status(description="nmp-governance-v2:" + "0" * 64),
        ):
            with self.assertRaises(authorization.AuthorizationError):
                self.verify(statuses=[status])

    def test_latest_exact_context_status_controls_rerun(self) -> None:
        revoked = self.make_status(
            state="failure",
            identifier=11,
            created_at="2026-07-31T00:00:00Z",
        )
        with self.assertRaises(authorization.AuthorizationError):
            self.verify(statuses=[self.statuses[0], revoked])

    def test_closed_or_merged_pull_request_and_post_land_replay_fail(self) -> None:
        with self.assertRaises(authorization.AuthorizationError):
            self.verify(
                pull_request=self.make_pull_request(state="closed"),
            )
        with self.assertRaises(authorization.AuthorizationError):
            self.verify(
                pull_request=self.make_pull_request(state="closed", merged=True),
            )

    def test_issue_target_must_be_readable_open_same_repo_non_pr_issue(self) -> None:
        with self.assertRaises(authorization.AuthorizationError):
            self.verify(issue=self.make_issue(state="closed"))
        with self.assertRaises(authorization.AuthorizationError):
            self.verify(issue={})
        cross_target = self.make_status(
            target_url="https://github.com/other/repository/issues/1074"
        )
        with self.assertRaises(authorization.AuthorizationError):
            self.verify(statuses=[cross_target])
        with self.assertRaises(authorization.AuthorizationError):
            self.verify(issue=self.make_issue(pull_request=True))

    def test_distinct_later_migration_requires_its_own_fresh_status(self) -> None:
        self.assertEqual(self.verify()[0], self.description)
        with self.assertRaises(authorization.AuthorizationError):
            self.verify(
                pull_request=self.make_pull_request(state="closed", merged=True)
            )

        second_base = self.head
        second_pr = 2200
        second_issue = 922
        next_checker = "governance/next-check.py"
        self.assertNotIn(next_checker, self.policy.protected_paths)
        self.write(
            self.verifier,
            (
                "PROTECTED_PATHS = ('governance/next-check.py',)\n"
                "def verify():\n"
                "    return True\n"
            ),
        )
        self.write(next_checker, "def check():\n    return True\n")
        second_head = self.commit("extend protected policy in second migration")
        (
            second_snapshot,
            second_description,
            second_pull,
            second_issue_record,
            second_statuses,
        ) = self.authorization_for(
            base=second_base,
            head=second_head,
            pr_number=second_pr,
            issue_number=second_issue,
        )
        second_paths = {entry.path for entry in second_snapshot.entries}
        self.assertIn(self.verifier, second_paths)
        self.assertIn(next_checker, second_paths)
        self.assertTrue(
            authorization.protected_paths_changed(
                self.policy,
                second_snapshot.entries,
            )
        )
        self.assertEqual(
            authorization.verify_authorization(
                self.policy,
                root=self.root,
                base=second_base,
                head=second_head,
                pr_number=second_pr,
                pr_url=(
                    f"https://github.com/{self.policy.repository}/pull/{second_pr}"
                ),
                pull_request_record=second_pull,
                issue_record=second_issue_record,
                status_records=second_statuses,
                snapshot=second_snapshot,
            ),
            (
                second_description,
                authorization.issue_target_url(self.policy, second_issue),
            ),
        )
        replay = self.make_status(
            description=self.description,
            head=second_head,
            issue_number=self.issue_number,
        )
        with self.assertRaises(authorization.AuthorizationError):
            authorization.verify_authorization(
                self.policy,
                root=self.root,
                base=second_base,
                head=second_head,
                pr_number=second_pr,
                pr_url=(
                    f"https://github.com/{self.policy.repository}/pull/{second_pr}"
                ),
                pull_request_record=second_pull,
                issue_record=self.make_issue(issue_number=self.issue_number),
                status_records=[replay],
                snapshot=second_snapshot,
            )

        evolved_policy = dataclasses.replace(
            self.policy,
            protected_paths=self.policy.protected_paths + (next_checker,),
        )
        authorization.require_well_formed_policy(evolved_policy)
        self.write(next_checker, "def check():\n    return False\n")
        third_head = self.commit("ordinary edit to newly protected checker")
        third_snapshot = authorization.canonical_diff(
            self.root,
            base=second_head,
            head=third_head,
        )
        self.assertTrue(
            authorization.protected_paths_changed(
                evolved_policy,
                third_snapshot.entries,
            )
        )
        with self.assertRaises(authorization.AuthorizationError):
            authorization.verify_authorization(
                evolved_policy,
                root=self.root,
                base=second_head,
                head=third_head,
                pr_number=2300,
                pr_url=(
                    f"https://github.com/{self.policy.repository}/pull/2300"
                ),
                pull_request_record=self.make_pull_request(
                    base=second_head,
                    head=third_head,
                    pr_number=2300,
                ),
                issue_record=self.make_issue(issue_number=923),
                status_records=[],
                snapshot=third_snapshot,
            )

    def test_malformed_api_records_and_policy_fail_closed(self) -> None:
        with self.assertRaises(authorization.AuthorizationError):
            self.verify(pull_request=[])
        with self.assertRaises(authorization.AuthorizationError):
            self.verify(statuses={})
        with self.assertRaises(authorization.AuthorizationError):
            self.verify(issue=[])
        empty = dataclasses.replace(
            self.policy,
            protected_paths=(),
            protected_prefixes=(),
        )
        with self.assertRaises(authorization.AuthorizationError):
            authorization.require_well_formed_policy(empty)
        overlap = dataclasses.replace(
            self.policy,
            protected_paths=self.policy.protected_paths + (self.tool_source,),
        )
        with self.assertRaises(authorization.AuthorizationError):
            authorization.require_well_formed_policy(overlap)

    def test_malformed_truncated_and_duplicate_raw_entries_fail_closed(self) -> None:
        resolved_base = (self.base + "\n").encode("ascii")
        resolved_head = (self.head + "\n").encode("ascii")
        merge_base = resolved_base
        truncated = (
            b":100644 100644 "
            + b"1" * 40
            + b" "
            + b"2" * 40
            + b" M\0"
        )
        with mock.patch.object(
            authorization,
            "_git_bytes",
            side_effect=(resolved_base, resolved_head, merge_base, truncated),
        ):
            with self.assertRaises(authorization.AuthorizationError):
                authorization.canonical_diff(
                    self.root,
                    base=self.base,
                    head=self.head,
                )
        header = (
            b":100644 100644 "
            + b"1" * 40
            + b" "
            + b"2" * 40
            + b" M\0"
        )
        duplicate = header + b"same.txt\0" + header + b"same.txt\0"
        with mock.patch.object(
            authorization,
            "_git_bytes",
            side_effect=(resolved_base, resolved_head, merge_base, duplicate),
        ):
            with self.assertRaises(authorization.AuthorizationError):
                authorization.canonical_diff(
                    self.root,
                    base=self.base,
                    head=self.head,
                )


if __name__ == "__main__":
    loader = unittest.defaultTestLoader
    suite = unittest.TestSuite(
        loader.loadTestsFromTestCase(case)
        for case in (ScratchRemovalTests, MigrationAuthorizationTests)
    )
    result = unittest.TextTestRunner(verbosity=2).run(suite)
    if result.wasSuccessful():
        print("surface migration authorization adversarial tests passed")
    raise SystemExit(not result.wasSuccessful())
