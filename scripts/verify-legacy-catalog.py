#!/usr/bin/env python3
"""Verify completeness, fidelity, partitioning, and schema of the legacy catalog."""

from __future__ import annotations

import argparse
import copy
import hashlib
import json
import os
import re
import sys
from collections import Counter
from pathlib import Path
from typing import Any


EXPECTED_PARTITIONS = {
    "atomic-github": 345,
    "clarification-github": 699,
    "committed-record": 249,
    "local-only": 1896,
}
EXPECTED_DISPOSITIONS = {
    "atomic-github": {"ready": 345},
    "clarification-github": {"clarification": 699},
    "committed-record": {"context": 199, "historical": 50},
    "local-only": {"ready": 800, "clarification": 828, "context": 195, "historical": 73},
}
VALID_CLASSES = {
    "ALREADY_IMPLEMENTED", "APP_LOCAL_CONTEXT", "APP_OR_PLATFORM_OWNED",
    "AUTHORSHIP_OR_PROVENANCE_WEAK", "CLARIFICATION_RESOLVED",
    "CONFLICT_OR_AMBIVALENCE", "CONTRADICTS_CURRENT", "CURRENT_INVARIANT",
    "CURRENT_V2_INVARIANT", "DUPLICATE_OR_OVERLAP", "DUPLICATE_OVERLAP",
    "EXPLAINS_FRONTIER", "HISTORICAL_RATIONALE", "MISSING_CURRENT_TEACHING",
    "NEW_CURRENT_GAP", "NEEDS_RAW_CONTEXT", "NON_ACTIONABLE_TASK_CONTEXT",
    "NO_ACTIONABLE_TEACHING", "OVERLAPS_ATOMIC", "SUPERSEDED_LEGACY_MECHANISM",
    "SUPERSEDED_MECHANISM", "TRACKED_CURRENT_GAP", "TRUE_OPEN_DESIGN_QUESTION",
    "UNDERSPECIFIED", "WORKFLOW_OR_REPO_POLICY", "WRONG_OR_UNSUPPORTED",
    "WRONG_OR_UNSUPPORTED_EXTRACTION",
}
REQUIRED_SOURCE = {
    "confidence", "disposition", "evidence_gap", "filepath", "kind",
    "related_issue_ids", "scope", "source_clause_ids", "source_observation_ids",
    "statement", "status", "title", "topic", "user_emphasis",
}
REQUIRED_EVIDENCE = {
    "carrier_class", "evidence_id", "message_line", "observation_ids", "provider",
    "quote", "quote_end", "quote_source", "quote_start", "quote_verified",
    "session_id", "text_sha256", "timestamp",
}
REQUIRED_RECONCILIATION = {
    "class", "class_system", "confidence", "confidence_basis", "current_owner",
    "ledger", "raw_context_checked", "recommendation",
}
ID_RE = re.compile(r"LNK-[A-Z0-9-]+")
NSEC_RE = re.compile(r"\bnsec1[023456789acdefghjklmnpqrstuvwxyz]{20,}", re.IGNORECASE)


class VerificationError(Exception):
    pass


def fail(message: str) -> None:
    raise VerificationError(message)


def redact(value: Any, path: str = "") -> tuple[Any, list[dict[str, str]]]:
    redactions: list[dict[str, str]] = []
    if isinstance(value, str):
        def replace(match: re.Match[str]) -> str:
            token = match.group(0)
            redactions.append({
                "path": path,
                "kind": "nostr-secret-key",
                "original_sha256": hashlib.sha256(token.encode()).hexdigest(),
            })
            return "[REDACTED_NSEC]"
        return NSEC_RE.sub(replace, value), redactions
    if isinstance(value, list):
        output = []
        for index, item in enumerate(value):
            clean, found = redact(item, f"{path}/{index}")
            output.append(clean)
            redactions.extend(found)
        return output, redactions
    if isinstance(value, dict):
        output = {}
        for key, item in value.items():
            clean, found = redact(item, f"{path}/{key}")
            output[key] = clean
            redactions.extend(found)
        return output, redactions
    return value, redactions


def default_source(repo: Path) -> Path | None:
    candidates = []
    if os.environ.get("NMP_LEGACY_SOURCE"):
        candidates.append(Path(os.environ["NMP_LEGACY_SOURCE"]))
    candidates.extend([
        repo / "a/final/issues.json",
        repo.parent.parent / "nmp/a/final/issues.json",
    ])
    return next((path for path in candidates if path.is_file()), None)


def default_records(repo: Path) -> Path | None:
    candidates = [repo / "docs/wiki/legacy-nmp/records", repo / "docs/records/v1"]
    return next((path for path in candidates if path.is_dir()), None)


def load_jsonl(path: Path) -> list[dict[str, Any]]:
    rows = []
    for number, line in enumerate(path.read_text().splitlines(), 1):
        if not line.strip():
            fail(f"blank JSONL line at {number}")
        try:
            rows.append(json.loads(line))
        except json.JSONDecodeError as error:
            fail(f"invalid JSON on line {number}: {error}")
    return rows


def compare_source(row: dict[str, Any], canonical: dict[str, Any]) -> None:
    record = copy.deepcopy(canonical)
    evidence = record.pop("evidence")
    rationale = record.pop("rationale_quotes")
    record.pop("issue_id")
    expected_source, source_redactions = redact(record, "/source")
    expected_evidence, evidence_redactions = redact(evidence, "/evidence")
    expected_rationale, rationale_redactions = redact(rationale, "/rationale_evidence")
    if row["source"] != expected_source:
        fail(f"{row['id']}: source metadata differs from canonical record")
    if row["evidence"] != expected_evidence:
        fail(f"{row['id']}: evidence differs from canonical record after safe redaction")
    if row["rationale_evidence"] != expected_rationale:
        fail(f"{row['id']}: rationale evidence differs from canonical record after safe redaction")
    expected_redactions = source_redactions + evidence_redactions + rationale_redactions
    if row["provenance"]["redactions"] != expected_redactions:
        fail(f"{row['id']}: redaction manifest differs from canonical source")
    expected_hashes = {
        f"/{'evidence' if key == 'evidence' else 'rationale_evidence'}/{index}": hashlib.sha256(
            item["quote"].encode()
        ).hexdigest()
        for key in ("evidence", "rationale_quotes")
        for index, item in enumerate(canonical[key])
    }
    if row["provenance"]["quote_sha256"] != expected_hashes:
        fail(f"{row['id']}: original quote hash manifest differs from canonical source")


def main() -> int:
    repo = Path(__file__).resolve().parents[1]
    parser = argparse.ArgumentParser()
    parser.add_argument("--catalog", type=Path, default=repo / "docs/wiki/legacy-nmp/catalog.jsonl")
    parser.add_argument("--source", type=Path)
    parser.add_argument(
        "--self-contained", action="store_true",
        help="do not discover a private canonical source; verify only committed artifacts",
    )
    parser.add_argument("--live-issues", type=Path, help="optional live GitHub issue JSON snapshot")
    parser.add_argument("--records", type=Path, help="committed record directory")
    args = parser.parse_args()
    if args.self_contained and args.source is not None:
        fail("--self-contained and --source are mutually exclusive")
    source_path = None if args.self_contained else (args.source or default_source(repo))
    records_path = args.records or default_records(repo)
    if records_path is None:
        fail("committed historical-record directory not found")

    rows = load_jsonl(args.catalog)
    canonical: dict[str, dict[str, Any]] | None = None
    if source_path is not None:
        canonical_rows = json.loads(source_path.read_text())
        canonical = {row["issue_id"]: row for row in canonical_rows}
        if len(canonical_rows) != 3189 or len(canonical) != 3189:
            fail(f"canonical source must contain 3,189 unique IDs; got {len(canonical_rows)}/{len(canonical)}")
    ids = [row.get("id") for row in rows]
    if len(rows) != 3189 or len(set(ids)) != 3189:
        fail(f"catalog must contain 3,189 unique IDs; got {len(rows)}/{len(set(ids))}")
    if canonical is not None and set(ids) != set(canonical):
        fail(f"catalog/source ID set mismatch: missing={len(set(canonical)-set(ids))}, extra={len(set(ids)-set(canonical))}")
    if ids != sorted(ids):
        fail("catalog rows are not deterministically sorted by ID")

    partitions = Counter()
    dispositions: dict[str, Counter[str]] = {name: Counter() for name in EXPECTED_PARTITIONS}
    partition_sets: dict[str, set[str]] = {name: set() for name in EXPECTED_PARTITIONS}
    for row in rows:
        record_id = row["id"]
        if not isinstance(record_id, str) or ID_RE.fullmatch(record_id) is None:
            fail(f"invalid record ID: {record_id!r}")
        if row.get("schema_version") != 1:
            fail(f"{record_id}: unsupported schema_version")
        for key in ("source", "evidence", "rationale_evidence", "provenance", "carrier", "reconciliation"):
            if key not in row:
                fail(f"{record_id}: missing required field {key}")
        if set(row["source"]) != REQUIRED_SOURCE:
            fail(f"{record_id}: source fields differ: {set(row['source']) ^ REQUIRED_SOURCE}")
        for evidence in row["evidence"] + row["rationale_evidence"]:
            if set(evidence) != REQUIRED_EVIDENCE:
                fail(f"{record_id}: evidence fields differ: {set(evidence) ^ REQUIRED_EVIDENCE}")
        provenance = row["provenance"]
        if provenance.get("message_role_inference_used") is not False:
            fail(f"{record_id}: authorship may not be inferred from message role")
        if provenance.get("authorship") not in {"unsafe_or_supplied_material", "unverified_not_inferred_from_message_role"}:
            fail(f"{record_id}: invalid authorship status")
        redactions = provenance.get("redactions")
        if not isinstance(redactions, list):
            fail(f"{record_id}: redactions must be a list")
        for redaction in redactions:
            if set(redaction) != {"path", "kind", "original_sha256"}:
                fail(f"{record_id}: invalid redaction manifest entry")
            if redaction["kind"] != "nostr-secret-key" or not re.fullmatch(r"[0-9a-f]{64}", redaction["original_sha256"]):
                fail(f"{record_id}: invalid redaction kind or hash")
        quote_hashes = provenance.get("quote_sha256")
        expected_quote_paths = {
            *(f"/evidence/{index}" for index in range(len(row["evidence"]))),
            *(f"/rationale_evidence/{index}" for index in range(len(row["rationale_evidence"]))),
        }
        if not isinstance(quote_hashes, dict) or set(quote_hashes) != expected_quote_paths:
            fail(f"{record_id}: quote hash path manifest is incomplete")
        if any(not isinstance(value, str) or not re.fullmatch(r"[0-9a-f]{64}", value) for value in quote_hashes.values()):
            fail(f"{record_id}: invalid quote hash")
        if NSEC_RE.search(json.dumps(row, ensure_ascii=False)):
            fail(f"{record_id}: unredacted Nostr secret key in committed catalog")
        reconciliation = row["reconciliation"]
        if not REQUIRED_RECONCILIATION <= set(reconciliation):
            fail(f"{record_id}: missing reconciliation fields")
        for field in ("class_system", "confidence_basis", "current_owner", "ledger", "recommendation"):
            if not isinstance(reconciliation[field], str) or not reconciliation[field].strip():
                fail(f"{record_id}: reconciliation {field} must be a non-empty string")
        if reconciliation["class"] not in VALID_CLASSES:
            fail(f"{record_id}: invalid class {reconciliation['class']}")
        if not isinstance(reconciliation["confidence"], (int, float)) or not 0 <= reconciliation["confidence"] <= 1:
            fail(f"{record_id}: invalid confidence")
        if not isinstance(reconciliation["raw_context_checked"], bool):
            fail(f"{record_id}: raw_context_checked must be boolean")
        for additional in reconciliation.get("additional_reviews", []):
            if additional.get("class") not in VALID_CLASSES:
                fail(f"{record_id}: invalid additional review class")

        partition = row["carrier"].get("partition")
        if partition not in EXPECTED_PARTITIONS:
            fail(f"{record_id}: invalid carrier partition {partition}")
        partitions[partition] += 1
        dispositions[partition][row["source"]["disposition"]] += 1
        partition_sets[partition].add(record_id)
        issues = row["carrier"].get("github_issues")
        committed = row["carrier"].get("committed_record")
        if partition == "atomic-github":
            if not issues or any(not 244 <= issue["number"] <= 409 for issue in issues) or committed is not None:
                fail(f"{record_id}: invalid atomic GitHub carrier")
        elif partition == "clarification-github":
            if not issues or any(not 412 <= issue["number"] <= 437 for issue in issues) or committed is not None:
                fail(f"{record_id}: invalid clarification GitHub carrier")
        elif partition == "committed-record":
            if issues or not isinstance(committed, str) or not (repo / committed).is_file():
                fail(f"{record_id}: invalid committed-record carrier")
        elif issues or committed is not None:
            fail(f"{record_id}: local-only record has a durable carrier")
        if canonical is not None:
            compare_source(row, canonical[record_id])

    if dict(partitions) != EXPECTED_PARTITIONS:
        fail(f"partition counts differ: {dict(partitions)}")
    for partition, expected in EXPECTED_DISPOSITIONS.items():
        if dict(dispositions[partition]) != expected:
            fail(f"{partition} source-disposition counts differ: {dict(dispositions[partition])}")
    names = list(partition_sets)
    for index, left in enumerate(names):
        for right in names[index + 1:]:
            overlap = partition_sets[left] & partition_sets[right]
            if overlap:
                fail(f"partition overlap {left}/{right}: {len(overlap)}")

    heading_ids = set()
    for path in records_path.glob("*.md"):
        heading_ids.update(re.findall(r"^### (LNK-[A-Z0-9-]+)", path.read_text(), re.MULTILINE))
    if heading_ids != partition_sets["committed-record"]:
        fail(f"committed-record heading set mismatch: headings={len(heading_ids)}, catalog={len(partition_sets['committed-record'])}")

    if args.live_issues:
        atomic_ids: set[str] = set()
        clarification_ids: set[str] = set()
        for issue in json.loads(args.live_issues.read_text()):
            found = set(ID_RE.findall(issue.get("body") or ""))
            if 244 <= issue["number"] <= 409:
                atomic_ids |= found
            elif 412 <= issue["number"] <= 437:
                clarification_ids |= found
        if atomic_ids != partition_sets["atomic-github"]:
            fail(f"atomic live-issue set mismatch: live={len(atomic_ids)}, catalog={len(partition_sets['atomic-github'])}")
        if clarification_ids != partition_sets["clarification-github"]:
            fail(f"clarification live-issue set mismatch: live={len(clarification_ids)}, catalog={len(partition_sets['clarification-github'])}")

    print("legacy catalog verification passed")
    print(f"  catalog IDs: {len(rows)}")
    print("  partitions: " + ", ".join(f"{name}={count}" for name, count in partitions.items()))
    print(f"  sensitive values redacted with hash provenance: {sum(len(row['provenance']['redactions']) for row in rows)}")
    if canonical is not None:
        print(f"  canonical comparison: passed ({source_path})")
        print("  source metadata and evidence: exact after declared safe redaction")
    else:
        print("  canonical comparison: skipped (self-contained clean-clone mode)")
    return 0


if __name__ == "__main__":
    try:
        sys.exit(main())
    except VerificationError as error:
        print(f"legacy catalog verification failed: {error}", file=sys.stderr)
        sys.exit(1)
