#!/usr/bin/env python3
"""Adversarial tests for reusable protected-governance authorization."""

from __future__ import annotations

import contextlib
import dataclasses
import importlib.util
import io
import json
import subprocess
import sys
import tempfile
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


class MigrationAuthorizationTests(unittest.TestCase):
    def setUp(self) -> None:
        self.temporary = tempfile.TemporaryDirectory()
        self.root = Path(self.temporary.name)
        self.git("init", "-q")
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
            repository="pablof7z/nmp",
            context="nmp/surface-governance-migration",
            owner_login="pablof7z",
            owner_id=779813,
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

    def tearDown(self) -> None:
        self.temporary.cleanup()

    def git(self, *args: str) -> str:
        return subprocess.check_output(
            ["git", "-C", str(self.root), *args],
            text=True,
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
        issue_number: int | None = None,
        context: str | None = None,
        state: str = "success",
        login: str = "pablof7z",
        owner_id: int = 779813,
        identifier: int = 10,
        created_at: str = "2026-07-30T23:59:00Z",
        target_url: str | None = None,
    ) -> dict[str, Any]:
        actual_issue = (
            issue_number if issue_number is not None else self.issue_number
        )
        return {
            "id": identifier,
            "created_at": created_at,
            "sha": head or self.head,
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

    def test_production_policy_protects_the_complete_program_and_itself(self) -> None:
        policy = authorization.PRODUCTION_POLICY
        for path in (
            ".github/workflows/architecture-gates.yml",
            ".github/workflows/ci.yml",
            ".github/workflows/surface-governance.yml",
            "scripts/check-surface-migration-authorization.py",
            "scripts/check-surface-governance.sh",
            "scripts/run-surface-regeneration-governance.sh",
            "scripts/test-surface-migration-authorization.py",
            "scripts/test-surface-governance.sh",
        ):
            self.assertIn(path, policy.protected_paths)
        self.assertIn("tools/behavior-traceability/", policy.protected_prefixes)
        authorization.require_well_formed_policy(policy)

    def test_exact_owner_authorization_passes_on_repeat_reruns(self) -> None:
        expected = (
            self.description,
            authorization.issue_target_url(self.policy, self.issue_number),
        )
        self.assertEqual(self.verify(), expected)
        self.assertEqual(self.verify(), expected)

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
    suite = unittest.defaultTestLoader.loadTestsFromTestCase(
        MigrationAuthorizationTests
    )
    result = unittest.TextTestRunner(verbosity=2).run(suite)
    if result.wasSuccessful():
        print("surface migration authorization adversarial tests passed")
    raise SystemExit(not result.wasSuccessful())
