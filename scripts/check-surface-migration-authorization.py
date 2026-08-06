#!/usr/bin/env python3
"""Verify exact owner authorization for protected governance migrations.

This program is extracted from the pull request base. The proposed head is Git
data only: the verifier derives the complete diff and object tuple without
importing or executing any proposed file.
"""

from __future__ import annotations

import argparse
import dataclasses
import hashlib
import json
import re
import subprocess
import sys
import traceback
from pathlib import Path
from typing import Any, Iterable, Sequence


class AuthorizationError(Exception):
    """The proposed governance migration is not exactly authorized."""


class StaleBaseError(AuthorizationError):
    """The head is not on the PR's current base, so nothing about it was judged.

    A verdict the author acts on by merging the current base in, not by changing
    the diff. It exits distinctly so a caller never has to parse the message
    (#1264).
    """


class GateMalfunction(Exception):
    """The verifier could not reach a verdict at all.

    Its inputs were unreadable or a Git command failed. This says nothing about
    the proposed change, so it must never be reported as a rejection. It still
    exits nonzero: the gate blocks either way.
    """


VERDICT_EXIT = 1
UNPROTECTED_EXIT = 3
STALE_BASE_EXIT = 4
MALFUNCTION_EXIT = 70


@dataclasses.dataclass(frozen=True)
class AuthorizationPolicy:
    repository: str
    context: str
    owner_login: str
    owner_id: int
    protected_paths: tuple[str, ...]
    protected_prefixes: tuple[str, ...]


@dataclasses.dataclass(frozen=True)
class ObjectState:
    mode: str
    object_type: str
    oid: str


@dataclasses.dataclass(frozen=True)
class DiffEntry:
    status: str
    path: str
    old_mode: str
    new_mode: str
    old_oid: str
    new_oid: str


@dataclasses.dataclass(frozen=True)
class DiffSnapshot:
    merge_base: str
    entries: tuple[DiffEntry, ...]


PRODUCTION_POLICY = AuthorizationPolicy(
    repository="pablof7z/nmp",
    context="nmp/surface-governance-migration",
    owner_login="pablof7z",
    owner_id=779813,
    protected_paths=(
        ".github/workflows/architecture-gates.yml",
        ".github/workflows/ci.yml",
        ".github/workflows/surface-governance.yml",
        "scripts/check-sdk-parity-allowlist.toml",
        "scripts/check-sdk-parity-allowlist.txt",
        "scripts/check-sdk-parity.sh",
        "scripts/check-surface-migration-authorization.py",
        "scripts/check-surface-governance.sh",
        "scripts/install-surface-tools.sh",
        "scripts/lib/require-commands.sh",
        "scripts/regenerate-surface-snapshots.sh",
        "scripts/report-surface-governance-verdict.sh",
        "scripts/run-surface-regeneration-governance.sh",
        "scripts/test-install-surface-tools.sh",
        "scripts/test-surface-governance.sh",
        "scripts/test-surface-migration-authorization.py",
        "tools/component-interface-snapshot/Cargo.lock",
        "tools/component-interface-snapshot/Cargo.toml",
        "tools/component-interface-snapshot/src/main.rs",
        "tools/rust-facade-snapshot/Cargo.lock",
        "tools/rust-facade-snapshot/Cargo.toml",
        "tools/rust-facade-snapshot/src/main.rs",
        "tools/surface-component-catalog/Cargo.lock",
        "tools/surface-component-catalog/Cargo.toml",
        "tools/surface-component-catalog/src/main.rs",
        "tools/surface-toolchain.env",
    ),
    protected_prefixes=(
        "tools/behavior-traceability/",
        "tools/rust-facade-snapshot/tests/fixtures/",
    ),
)

ABSENT = ObjectState(mode="absent", object_type="absent", oid="absent")


def _matches_exact_namespace(path: str, exact: str) -> bool:
    return path == exact or path.startswith(f"{exact}/")


def _matches_prefix_namespace(path: str, prefix: str) -> bool:
    root = prefix.removesuffix("/")
    return path == root or path.startswith(prefix)


def _path_is_protected(policy: AuthorizationPolicy, path: str) -> bool:
    return any(
        _matches_exact_namespace(path, exact)
        for exact in policy.protected_paths
    ) or any(
        _matches_prefix_namespace(path, prefix)
        for prefix in policy.protected_prefixes
    )


def _require_oid(label: str, value: str) -> str:
    if not re.fullmatch(r"[0-9a-f]{40}", value):
        raise AuthorizationError(f"{label} must be an exact 40-character Git object ID")
    return value


def _require_positive_integer(label: str, value: int) -> int:
    if not isinstance(value, int) or isinstance(value, bool) or value < 1:
        raise AuthorizationError(f"{label} must be a positive integer")
    return value


def _require_safe_path(label: str, value: str) -> str:
    parts = value.split("/")
    if (
        not value
        or value.startswith("/")
        or any(part in ("", ".", "..") for part in parts)
        or any(character in value for character in ("\0", "\n", "\r", "\t"))
    ):
        raise AuthorizationError(f"{label} is unsafe: {value!r}")
    return value


def require_well_formed_policy(policy: AuthorizationPolicy) -> None:
    if not re.fullmatch(r"[A-Za-z0-9_.-]+/[A-Za-z0-9_.-]+", policy.repository):
        raise AuthorizationError("fixed repository is malformed")
    if not policy.context or len(policy.context) > 100:
        raise AuthorizationError("fixed commit-status context is malformed")
    if not policy.owner_login:
        raise AuthorizationError("fixed owner login is empty")
    _require_positive_integer("fixed owner GitHub user ID", policy.owner_id)
    if not policy.protected_paths and not policy.protected_prefixes:
        raise AuthorizationError("protected governance inventory is empty")
    if len(policy.protected_paths) != len(set(policy.protected_paths)):
        raise AuthorizationError("protected governance path inventory has duplicates")
    if len(policy.protected_prefixes) != len(set(policy.protected_prefixes)):
        raise AuthorizationError("protected governance prefix inventory has duplicates")
    for path in policy.protected_paths:
        _require_safe_path("protected governance path", path)
    for prefix in policy.protected_prefixes:
        if not prefix.endswith("/"):
            raise AuthorizationError(
                f"protected governance prefix must end in '/': {prefix!r}"
            )
        _require_safe_path("protected governance prefix", prefix.removesuffix("/"))
    for path in policy.protected_paths:
        if any(
            _matches_prefix_namespace(path, prefix)
            for prefix in policy.protected_prefixes
        ):
            raise AuthorizationError(
                f"protected path duplicates a protected prefix: {path}"
            )


def _git_bytes(root: Path, *args: str) -> bytes:
    result = subprocess.run(
        ["git", "-C", str(root), *args],
        check=False,
        stdout=subprocess.PIPE,
        stderr=subprocess.PIPE,
    )
    if result.returncode != 0:
        detail = (
            result.stderr.decode("utf-8", errors="replace").strip()
            or result.stdout.decode("utf-8", errors="replace").strip()
            or "git command failed"
        )
        # Git failing is the repository or the checkout failing, never the
        # proposed change failing. Reporting it as an authorization rejection is
        # exactly the confusion #1264 removes.
        raise GateMalfunction(detail)
    return result.stdout


def _resolve_commit(root: Path, label: str, value: str) -> str:
    try:
        resolved = _git_bytes(
            root,
            "rev-parse",
            "--verify",
            f"{value}^{{commit}}",
        ).decode("ascii").strip()
    except UnicodeDecodeError as error:
        raise AuthorizationError(f"{label} commit ID is not ASCII") from error
    return _require_oid(label, resolved)


def _decode_path(raw: bytes) -> str:
    try:
        path = raw.decode("utf-8")
    except UnicodeDecodeError as error:
        raise AuthorizationError("changed path is not UTF-8") from error
    return _require_safe_path("changed path", path)


def _head_state(root: Path, head: str, path: str) -> ObjectState:
    raw = _git_bytes(root, "ls-tree", "-z", "--full-tree", head, "--", path)
    if not raw:
        return ABSENT
    records = [record for record in raw.split(b"\0") if record]
    if len(records) != 1:
        raise AuthorizationError(f"Git tree lookup for {path} is ambiguous")
    try:
        header, raw_path = records[0].split(b"\t", 1)
        mode, object_type, oid = header.decode("ascii").split(" ", 2)
    except (ValueError, UnicodeDecodeError) as error:
        raise AuthorizationError(f"Git tree entry for {path} is malformed") from error
    if _decode_path(raw_path) != path:
        raise AuthorizationError(f"Git tree lookup returned another path for {path}")
    if not re.fullmatch(r"[0-7]{6}", mode):
        raise AuthorizationError(f"Git tree mode for {path} is malformed")
    if object_type not in ("blob", "commit", "tree"):
        raise AuthorizationError(f"Git object type for {path} is unsupported")
    return ObjectState(
        mode=mode,
        object_type=object_type,
        oid=_require_oid(f"Git object ID for {path}", oid),
    )


def _normalize_raw_side(
    path: str,
    label: str,
    mode: str,
    oid: str,
) -> tuple[str, str]:
    if mode == "000000":
        if oid != "0" * 40:
            raise AuthorizationError(
                f"{label} side for {path} has an object without a mode"
            )
        return "absent", "absent"
    if not re.fullmatch(r"[0-7]{6}", mode):
        raise AuthorizationError(f"{label} mode for {path} is malformed")
    if oid == "0" * 40:
        raise AuthorizationError(
            f"{label} side for {path} has a mode without an object"
        )
    return mode, _require_oid(f"{label} object ID for {path}", oid)


def canonical_diff(
    root: Path,
    *,
    base: str,
    head: str,
) -> DiffSnapshot:
    base = _resolve_commit(root, "base", base)
    head = _resolve_commit(root, "head", head)
    try:
        merge_base = _git_bytes(root, "merge-base", base, head).decode("ascii").strip()
    except UnicodeDecodeError as error:
        raise AuthorizationError("merge-base object ID is not ASCII") from error
    merge_base = _require_oid("derived merge base", merge_base)
    raw = _git_bytes(
        root,
        "diff-tree",
        "-r",
        "--raw",
        "-z",
        "--no-abbrev",
        "--no-renames",
        "--no-commit-id",
        merge_base,
        head,
    )
    values = raw.split(b"\0")
    if values and values[-1] == b"":
        values.pop()
    if len(values) % 2 != 0:
        raise AuthorizationError("raw Git tree diff is truncated")
    entries: list[DiffEntry] = []
    for index in range(0, len(values), 2):
        try:
            header = values[index].decode("ascii")
        except UnicodeDecodeError as error:
            raise AuthorizationError("raw Git tree diff header is not ASCII") from error
        if not header.startswith(":"):
            raise AuthorizationError("raw Git tree diff header is malformed")
        parts = header[1:].split(" ")
        if len(parts) != 5:
            raise AuthorizationError("raw Git tree diff header has the wrong arity")
        old_mode, new_mode, old_oid, new_oid, status = parts
        if status not in ("A", "D", "M", "T"):
            raise AuthorizationError(
                f"raw Git tree diff status is unsupported: {status!r}"
            )
        path = _decode_path(values[index + 1])
        old_mode, old_oid = _normalize_raw_side(
            path, "old", old_mode, old_oid
        )
        new_mode, new_oid = _normalize_raw_side(
            path, "new", new_mode, new_oid
        )
        if status == "A" and (old_mode != "absent" or new_mode == "absent"):
            raise AuthorizationError(f"added path has malformed sides: {path}")
        if status == "D" and (old_mode == "absent" or new_mode != "absent"):
            raise AuthorizationError(f"deleted path has malformed sides: {path}")
        if status in ("M", "T") and (
            old_mode == "absent" or new_mode == "absent"
        ):
            raise AuthorizationError(f"modified path has an absent side: {path}")
        entries.append(
            DiffEntry(
                status=status,
                path=path,
                old_mode=old_mode,
                new_mode=new_mode,
                old_oid=old_oid,
                new_oid=new_oid,
            )
        )
    paths = [entry.path for entry in entries]
    if len(paths) != len(set(paths)):
        raise AuthorizationError("raw Git tree diff contains a duplicate path")
    return DiffSnapshot(
        merge_base=merge_base,
        entries=tuple(sorted(entries, key=lambda entry: entry.path)),
    )


def protected_paths_changed(
    policy: AuthorizationPolicy,
    entries: Iterable[DiffEntry],
) -> bool:
    return any(_path_is_protected(policy, entry.path) for entry in entries)


def require_current_base(snapshot: DiffSnapshot, base: str) -> None:
    if snapshot.merge_base != base:
        raise StaleBaseError(
            "protected migration head is not descended from the current PR base"
        )


def affected_prefix_states(
    policy: AuthorizationPolicy,
    *,
    root: Path,
    head: str,
    entries: Sequence[DiffEntry],
) -> tuple[tuple[str, ObjectState], ...]:
    changed_paths = {entry.path for entry in entries}
    affected = [
        prefix
        for prefix in policy.protected_prefixes
        if any(
            _matches_prefix_namespace(path, prefix)
            for path in changed_paths
        )
    ]
    return tuple(
        (prefix, _head_state(root, head, prefix.removesuffix("/")))
        for prefix in sorted(affected)
    )


def _canonical_field(name: str, value: str) -> bytes:
    name_bytes = name.encode("utf-8")
    value_bytes = value.encode("utf-8")
    return (
        str(len(name_bytes)).encode("ascii")
        + b":"
        + name_bytes
        + b"\0"
        + str(len(value_bytes)).encode("ascii")
        + b":"
        + value_bytes
        + b"\0"
    )


def issue_target_url(policy: AuthorizationPolicy, issue_number: int) -> str:
    issue_number = _require_positive_integer("migration issue number", issue_number)
    return f"https://github.com/{policy.repository}/issues/{issue_number}"


def issue_number_from_target(policy: AuthorizationPolicy, target_url: Any) -> int:
    if not isinstance(target_url, str):
        raise AuthorizationError("migration status has no trusted issue target")
    prefix = f"https://github.com/{policy.repository}/issues/"
    if not target_url.startswith(prefix):
        raise AuthorizationError(
            "migration status target is not a same-repository issue"
        )
    number = target_url.removeprefix(prefix)
    if not re.fullmatch(r"[1-9][0-9]*", number):
        raise AuthorizationError("migration status issue target is malformed")
    issue_number = int(number)
    if issue_target_url(policy, issue_number) != target_url:
        raise AuthorizationError("migration status issue target is not canonical")
    return issue_number


def authorization_description(
    policy: AuthorizationPolicy,
    *,
    root: Path,
    base: str,
    head: str,
    pr_number: int,
    issue_number: int,
    snapshot: DiffSnapshot | None = None,
) -> str:
    require_well_formed_policy(policy)
    base = _require_oid("base", base)
    head = _require_oid("head", head)
    pr_number = _require_positive_integer("pull request number", pr_number)
    issue_number = _require_positive_integer("migration issue number", issue_number)
    actual_snapshot = (
        snapshot
        if snapshot is not None
        else canonical_diff(root, base=base, head=head)
    )
    actual_entries = actual_snapshot.entries
    if not protected_paths_changed(policy, actual_entries):
        raise AuthorizationError("pull request changes no protected governance path")
    require_current_base(actual_snapshot, base)

    fields = [
        ("domain", "nmp-surface-governance-migration-v2"),
        ("repository", policy.repository),
        ("pull_request", str(pr_number)),
        ("issue", str(issue_number)),
        ("issue_target", issue_target_url(policy, issue_number)),
        ("context", policy.context),
        ("owner_login", policy.owner_login),
        ("owner_id", str(policy.owner_id)),
        ("base", base),
        ("head", head),
        ("merge_base", actual_snapshot.merge_base),
    ]
    for entry in sorted(actual_entries, key=lambda item: item.path):
        fields.extend(
            (
                ("change_status", entry.status),
                ("path", entry.path),
                ("old_mode", entry.old_mode),
                ("new_mode", entry.new_mode),
                ("old_oid", entry.old_oid),
                ("new_oid", entry.new_oid),
            )
        )
    for prefix, state in affected_prefix_states(
        policy,
        root=root,
        head=head,
        entries=actual_entries,
    ):
        fields.extend(
            (
                ("protected_prefix", prefix),
                ("tree_mode", state.mode),
                ("tree_type", state.object_type),
                ("tree_oid", state.oid),
            )
        )
    encoded = b"".join(_canonical_field(name, value) for name, value in fields)
    return f"nmp-governance-v2:{hashlib.sha256(encoded).hexdigest()}"


def _read_json(path: Path, label: str) -> Any:
    try:
        return json.loads(path.read_text(encoding="utf-8"))
    except (OSError, UnicodeDecodeError, json.JSONDecodeError) as error:
        # The workflow fetched these records; an unreadable one means the gate
        # never obtained the evidence it judges with. A fork head whose statuses
        # are unreachable still gets a readable `[]`, which fails closed as an
        # ordinary rejection.
        raise GateMalfunction(
            f"{label} is unavailable or invalid: {error}"
        ) from error


def _nested(record: Any, *keys: str) -> Any:
    value = record
    for key in keys:
        if not isinstance(value, dict):
            return None
        value = value.get(key)
    return value


def require_open_pull_request(
    policy: AuthorizationPolicy,
    record: Any,
    *,
    base: str,
    head: str,
    pr_number: int,
    pr_url: str,
) -> None:
    if not isinstance(record, dict):
        raise AuthorizationError("pull request API record must be a JSON object")
    expected_url = f"https://github.com/{policy.repository}/pull/{pr_number}"
    checks = (
        (record.get("number") == pr_number, "pull request number differs"),
        (record.get("state") == "open", "pull request is not open"),
        (record.get("merged") is False, "pull request is already merged"),
        (record.get("html_url") == expected_url, "pull request URL differs"),
        (pr_url == expected_url, "event pull request URL differs"),
        (
            _nested(record, "base", "sha") == base,
            "pull request base differs from the bound base",
        ),
        (
            _nested(record, "head", "sha") == head,
            "pull request head differs from the bound head",
        ),
        (
            _nested(record, "base", "repo", "full_name") == policy.repository,
            "pull request base repository differs",
        ),
        (
            _nested(record, "head", "repo", "full_name") == policy.repository,
            "pull request head repository differs",
        ),
    )
    for passed, message in checks:
        if not passed:
            raise AuthorizationError(message)


def require_open_issue(
    policy: AuthorizationPolicy,
    record: Any,
    *,
    issue_number: int,
    target_url: str,
) -> None:
    if not isinstance(record, dict):
        raise AuthorizationError("issue API record must be a JSON object")
    expected_api_repository = f"https://api.github.com/repos/{policy.repository}"
    checks = (
        (record.get("number") == issue_number, "migration issue number differs"),
        (record.get("state") == "open", "migration issue is not open"),
        (record.get("html_url") == target_url, "migration issue URL differs"),
        (
            record.get("repository_url") == expected_api_repository,
            "migration issue repository differs",
        ),
        (
            "pull_request" not in record,
            "migration status target is a pull request, not an issue",
        ),
    )
    for passed, message in checks:
        if not passed:
            raise AuthorizationError(message)


def _latest_context_status(statuses: Any, context: str) -> dict[str, Any]:
    if not isinstance(statuses, list):
        raise AuthorizationError("commit-status API record must be a JSON array")
    matching = [
        status
        for status in statuses
        if isinstance(status, dict) and status.get("context") == context
    ]
    if not matching:
        raise AuthorizationError(f"no status exists in exact context {context}")

    def order(status: dict[str, Any]) -> tuple[str, int]:
        created_at = status.get("created_at")
        identifier = status.get("id")
        return (
            created_at if isinstance(created_at, str) else "",
            (
                identifier
                if isinstance(identifier, int) and not isinstance(identifier, bool)
                else -1
            ),
        )

    return max(matching, key=order)


def require_owner_status_identity(
    policy: AuthorizationPolicy,
    status: dict[str, Any],
    *,
    head: str,
) -> None:
    checks = (
        (status.get("state") == "success", "latest migration status is not successful"),
        (status.get("sha") == head, "migration status is attached to another commit"),
        (
            _nested(status, "creator", "login") == policy.owner_login,
            "migration status creator is not the fixed repository owner",
        ),
        (
            _nested(status, "creator", "id") == policy.owner_id,
            "migration status creator has the wrong immutable GitHub user ID",
        ),
        (
            _nested(status, "creator", "type") == "User",
            "migration status creator is not a GitHub user",
        ),
    )
    for passed, message in checks:
        if not passed:
            raise AuthorizationError(message)


def verify_authorization(
    policy: AuthorizationPolicy,
    *,
    root: Path,
    base: str,
    head: str,
    pr_number: int,
    pr_url: str,
    pull_request_record: Any,
    issue_record: Any,
    status_records: Any,
    snapshot: DiffSnapshot | None = None,
) -> tuple[str, str]:
    require_well_formed_policy(policy)
    actual_snapshot = (
        snapshot
        if snapshot is not None
        else canonical_diff(root, base=base, head=head)
    )
    actual_entries = actual_snapshot.entries
    if not protected_paths_changed(policy, actual_entries):
        raise AuthorizationError("pull request changes no protected governance path")
    require_current_base(actual_snapshot, base)
    require_open_pull_request(
        policy,
        pull_request_record,
        base=base,
        head=head,
        pr_number=pr_number,
        pr_url=pr_url,
    )
    status = _latest_context_status(status_records, policy.context)
    require_owner_status_identity(policy, status, head=head)
    issue_number = issue_number_from_target(policy, status.get("target_url"))
    target_url = issue_target_url(policy, issue_number)
    require_open_issue(
        policy,
        issue_record,
        issue_number=issue_number,
        target_url=target_url,
    )
    description = authorization_description(
        policy,
        root=root,
        base=base,
        head=head,
        pr_number=pr_number,
        issue_number=issue_number,
        snapshot=actual_snapshot,
    )
    if status.get("description") != description:
        raise AuthorizationError(
            "migration status does not bind the exact PR/base/head/diff/object tuple"
        )
    return description, target_url


def _parser() -> argparse.ArgumentParser:
    parser = argparse.ArgumentParser()
    parser.add_argument("--root", required=True, type=Path)
    parser.add_argument("--base", required=True)
    parser.add_argument("--head", required=True)
    parser.add_argument("--pr-number", required=True, type=int)
    subparsers = parser.add_subparsers(dest="mode", required=True)

    verify = subparsers.add_parser("verify")
    verify.add_argument("--pr-url", required=True)
    verify.add_argument("--pull-request-record", required=True, type=Path)
    verify.add_argument("--issue-record", required=True, type=Path)
    verify.add_argument("--status-records", required=True, type=Path)

    print_status = subparsers.add_parser("print-status")
    print_status.add_argument("--issue-number", required=True, type=int)
    return parser


def main(argv: Sequence[str] | None = None) -> int:
    args = _parser().parse_args(argv)
    try:
        base = _resolve_commit(args.root, "base", args.base)
        head = _resolve_commit(args.root, "head", args.head)
        snapshot = canonical_diff(args.root, base=base, head=head)
        if not protected_paths_changed(PRODUCTION_POLICY, snapshot.entries):
            return UNPROTECTED_EXIT
        if args.mode == "print-status":
            description = authorization_description(
                PRODUCTION_POLICY,
                root=args.root,
                base=base,
                head=head,
                pr_number=args.pr_number,
                issue_number=args.issue_number,
                snapshot=snapshot,
            )
            target_url = issue_target_url(PRODUCTION_POLICY, args.issue_number)
        else:
            description, target_url = verify_authorization(
                PRODUCTION_POLICY,
                root=args.root,
                base=base,
                head=head,
                pr_number=args.pr_number,
                pr_url=args.pr_url,
                pull_request_record=_read_json(
                    args.pull_request_record, "pull request API record"
                ),
                issue_record=_read_json(args.issue_record, "issue API record"),
                status_records=_read_json(
                    args.status_records, "commit-status API record"
                ),
                snapshot=snapshot,
            )
        print(f"context={PRODUCTION_POLICY.context}")
        print(f"description={description}")
        print(f"target_url={target_url}")
        return 0
    except StaleBaseError as error:
        print(f"surface-migration-authorization: {error}", file=sys.stderr)
        return STALE_BASE_EXIT
    except AuthorizationError as error:
        print(f"surface-migration-authorization: {error}", file=sys.stderr)
        return VERDICT_EXIT
    except GateMalfunction as error:
        print(
            f"surface-migration-authorization-malfunction: {error}",
            file=sys.stderr,
        )
        return MALFUNCTION_EXIT
    except Exception:
        # An unhandled defect in the verifier is the loudest possible
        # malfunction. It must not exit 1, because 1 is the code that means
        # "the head is not authorized".
        traceback.print_exc()
        print(
            "surface-migration-authorization-malfunction: "
            "the verifier crashed without rendering a verdict",
            file=sys.stderr,
        )
        return MALFUNCTION_EXIT


if __name__ == "__main__":
    raise SystemExit(main())
