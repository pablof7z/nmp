#!/usr/bin/env python3
"""Build the source-complete legacy-NMP reconciliation catalog.

The canonical record bank and the review ledgers are forensic inputs rather
than product authority.  This script joins them without treating a user-role
message as proof that every byte in that message was authored by the user.
"""

from __future__ import annotations

import argparse
import copy
import hashlib
import json
import re
from pathlib import Path
from typing import Any, Iterable


CATALOG_SCHEMA_VERSION = 1
ID_RE = re.compile(r"LNK-[A-Z0-9-]+")
NSEC_RE = re.compile(r"\bnsec1[023456789acdefghjklmnpqrstuvwxyz]{20,}", re.IGNORECASE)

ATOMIC_CLASSES = {
    "ALREADY_IMPLEMENTED",
    "APP_OR_PLATFORM_OWNED",
    "CONFLICT_OR_AMBIVALENCE",
    "CURRENT_INVARIANT",
    "DUPLICATE_OVERLAP",
    "HISTORICAL_RATIONALE",
    "SUPERSEDED_MECHANISM",
    "TRACKED_CURRENT_GAP",
    "UNDERSPECIFIED",
    "WRONG_OR_UNSUPPORTED",
}
HISTORICAL_CLASSES = {
    "APP_LOCAL_CONTEXT",
    "AUTHORSHIP_OR_PROVENANCE_WEAK",
    "CONTRADICTS_CURRENT",
    "EXPLAINS_FRONTIER",
    "MISSING_CURRENT_TEACHING",
    "NEEDS_RAW_CONTEXT",
    "NO_ACTIONABLE_TEACHING",
    "OVERLAPS_ATOMIC",
    "SUPERSEDED_MECHANISM",
}
RECONCILIATION_CLASSES = {
    "ALREADY_IMPLEMENTED",
    "APP_OR_PLATFORM_OWNED",
    "AUTHORSHIP_OR_PROVENANCE_WEAK",
    "CLARIFICATION_RESOLVED",
    "CURRENT_V2_INVARIANT",
    "DUPLICATE_OR_OVERLAP",
    "HISTORICAL_RATIONALE",
    "NEW_CURRENT_GAP",
    "NON_ACTIONABLE_TASK_CONTEXT",
    "SUPERSEDED_LEGACY_MECHANISM",
    "TRACKED_CURRENT_GAP",
    "TRUE_OPEN_DESIGN_QUESTION",
    "WORKFLOW_OR_REPO_POLICY",
    "WRONG_OR_UNSUPPORTED_EXTRACTION",
}
VALID_CLASSES = ATOMIC_CLASSES | HISTORICAL_CLASSES | RECONCILIATION_CLASSES

CONFIDENCE = {
    "high": 0.95,
    "medium-high": 0.85,
    "medium": 0.70,
    "low": 0.50,
}


# The original historical-record pass deliberately stopped at NEEDS_RAW_CONTEXT.
# These final dispositions come from the bounded raw-context follow-up recorded
# in raw-evidence-findings.md.  They replace the provisional class, not the
# canonical quote or source metadata.
RAW_CONTEXT_RESOLUTIONS: dict[str, tuple[str, str, str, float]] = {
    "LNK-FEEDS-42208DCC6D": (
        "OVERLAPS_ATOMIC", "Binding/literal query-set authority; #47",
        "Preserve as the active-account-binding versus literal multi-account-set distinction; no new issue.", 0.95),
    "LNK-FEEDS-5B639F5987": (
        "OVERLAPS_ATOMIC", "Binding/literal query-set authority; #47",
        "Preserve the deliberate all-account p-tag query case; no new issue.", 0.95),
    "LNK-FEEDS-B1C6EE5460": (
        "OVERLAPS_ATOMIC", "Binding/literal query-set authority; #47",
        "Preserve as the active binding versus literal-set boundary; no new issue.", 0.95),
    "LNK-FEEDS-D217CE37A3": (
        "OVERLAPS_ATOMIC", "VISION write intent; #45/#22",
        "Cross-link to immutable semantic construction, signing, envelope composition, and route policy.", 0.95),
    "LNK-OUTBOX-6E787F60E0": (
        "OVERLAPS_ATOMIC", "#22; #261/#262/#265/#267",
        "Preserve the full worked routing example as rationale for existing routing owners.", 0.95),
    "LNK-STORE-5B5CDBAAB3": (
        "OVERLAPS_ATOMIC", "README/VISION app-render boundary; #9/#155",
        "Preserve the corrected ownership test: NMP owns reliable primitives, apps choose rendering.", 0.90),
    "LNK-PERF-8DE7A968E6": (
        "EXPLAINS_FRONTIER", "#176 performance/proof",
        "Keep as a concrete iOS launch/resync falsifier, not a universal numeric SLA.", 0.90),
    "LNK-TESTING-E86FEC04EC": (
        "SUPERSEDED_MECHANISM", "current explicit public composition",
        "Archive the rejection of recreating nmp-defaults in tests; do not infer a ban on test helpers.", 0.90),
    "LNK-NORTH-10803DBE15": (
        "AUTHORSHIP_OR_PROVENANCE_WEAK", "supplied agent recommendation",
        "Do not attribute the pasted Android-only recommendation to Pablo without independent adoption evidence.", 0.99),
    "LNK-NORTH-5C591559D7": (
        "AUTHORSHIP_OR_PROVENANCE_WEAK", "supplied agent recommendation",
        "Retain the later directional adoption separately; do not call the pasted phrase a literal Pablo quote.", 0.99),
    "LNK-NORTH-F2F2DE705C": (
        "SUPERSEDED_MECHANISM", "current private reconciliation graph",
        "Archive Trellis library mechanics; retain only the generic NMP-owns-meaning boundary.", 0.90),
    "LNK-UPDATES-D5ADE0AB1C": (
        "SUPERSEDED_MECHANISM", "current query/delta architecture",
        "Archive as a question about the retired 4 Hz full-snapshot mechanism; it is not a requirement.", 0.99),
    "LNK-VIEWS-E0C58BD123": (
        "EXPLAINS_FRONTIER", "#63; content-session/UI frontier",
        "Preserve the NIP-51-list to NIP-29-feed ergonomics as acceptance context under existing owners.", 0.95),
    "LNK-CLIENTS-A13A3A47A7": (
        "EXPLAINS_FRONTIER", "#47 platform signer/provider lifecycle",
        "Keep Olas as prior art for fresh nostrconnect session and callback/liveness handling.", 0.90),
    "LNK-CLIENTS-A60528702A": (
        "OVERLAPS_ATOMIC", "README/VISION concept-owned reads; #155",
        "Preserve the rejection of a central fixed relation-summary API as anti-regression rationale.", 0.95),
    "LNK-CLIENTS-4D587E179B": (
        "EXPLAINS_FRONTIER", "#47 platform signer/provider lifecycle",
        "Record URL-scheme signer discovery/handoff as platform work, not NIP-89 core semantics.", 0.95),
    "LNK-IOS-5E70F5457C": (
        "APP_LOCAL_CONTEXT", "legacy iOS prototype; current mailbox/reactive engine",
        "Archive the one-frame-per-relay blocking defect as a falsifier; no current implementation gap.", 0.95),
    "LNK-IOS-A376C6E15F": (
        "SUPERSEDED_MECHANISM", "current typed immutable write composition",
        "Archive the missing generated legacy NIP-29 action-builder surface.", 0.95),
    "LNK-ANDROID-449F92A828": (
        "APP_LOCAL_CONTEXT", "legacy Android ProfileScreen; current parity work",
        "Archive the app navigation/follow defect; retain only generic platform-parity context.", 0.95),
    "LNK-NIP29-84B9F47DD3": (
        "OVERLAPS_ATOMIC", "query-first/kind-blind authority; #45",
        "Preserve the decision that group reads are ordinary #h filters, not a new core group noun.", 0.95),
    "LNK-NIP29-D7EC85325F": (
        "OVERLAPS_ATOMIC", "VISION query/write-intent split; #45",
        "Preserve the read-projection versus typed-write-builder distinction; no open question.", 0.95),
    "LNK-PROFILES-B965F2F447": (
        "EXPLAINS_FRONTIER", "#75/#376 content-session/UI lifecycle",
        "Keep native component lifecycle/reactivity with Rust as truth under the existing UI frontier.", 0.95),
    "LNK-NIP46-4EB1F11618": (
        "EXPLAINS_FRONTIER", "#47 signer reattachment/session liveness",
        "Keep the failure as liveness/recovery acceptance context, not evidence for NIP-89.", 0.90),
    "LNK-MEDIA-E526CF6913": (
        "NO_ACTIONABLE_TEACHING", "legacy registry CLI formatting",
        "Archive the accepted artifact-level grouping preference; do not promote it into current product semantics.", 0.95),
}


def markdown_cells(line: str) -> list[str]:
    """Split a Markdown table row while retaining escaped literal pipes."""
    text = line.strip()
    if text.startswith("|"):
        text = text[1:]
    if text.endswith("|"):
        text = text[:-1]
    cells: list[str] = []
    current: list[str] = []
    escaped = False
    for char in text:
        if escaped:
            current.append(char)
            escaped = False
        elif char == "\\":
            current.append(char)
            escaped = True
        elif char == "|":
            cells.append("".join(current).strip())
            current = []
        else:
            current.append(char)
    cells.append("".join(current).strip())
    return cells


def clean_cell(value: str) -> str:
    value = value.strip()
    if len(value) >= 2 and value.startswith("`") and value.endswith("`"):
        value = value[1:-1]
    return value.replace("\\|", "|")


def table_rows(paths: Iterable[Path], first_prefix: str, columns: int) -> Iterable[tuple[Path, list[str]]]:
    for path in paths:
        for line in path.read_text().splitlines():
            if not line.startswith("|"):
                continue
            cells = [clean_cell(cell) for cell in markdown_cells(line)]
            if not cells or not cells[0].startswith(first_prefix):
                continue
            if len(cells) != columns:
                raise ValueError(f"{path}: expected {columns} columns, got {len(cells)}: {line}")
            yield path, cells


def parse_confidence(value: str) -> tuple[float, str]:
    normalized = value.strip().lower()
    for label in ("medium-high", "high", "medium", "low"):
        if normalized == label or normalized.startswith(f"{label}."):
            return CONFIDENCE[label], f"review ledger label: {value}"
    try:
        number = float(normalized)
    except ValueError:
        raise ValueError(f"unknown confidence value: {value}")
    if not 0 <= number <= 1:
        raise ValueError(f"confidence outside [0,1]: {value}")
    return number, "review ledger numeric score"


def parse_atomic(ledger_dir: Path) -> dict[int, dict[str, Any]]:
    result: dict[int, dict[str, Any]] = {}
    paths = sorted(ledger_dir.glob("triage-issues-*.md"))
    for path, cells in table_rows(paths, "#", 6):
        issue = int(cells[0][1:])
        confidence, basis = parse_confidence(cells[5])
        result[issue] = {
            "class": cells[2],
            "class_system": "atomic-issue-triage-v1",
            "current_owner": cells[3],
            "recommendation": cells[4],
            "confidence": confidence,
            "confidence_basis": basis,
            "raw_context_checked": False,
            "ledger": path.name,
        }
    if len(result) != 166:
        raise ValueError(f"expected 166 atomic issue reviews, found {len(result)}")
    return result


def parse_reconciled_rows(paths: Iterable[Path], expected: int, class_system: str) -> dict[str, dict[str, Any]]:
    result: dict[str, dict[str, Any]] = {}
    for path, cells in table_rows(paths, "LNK-", 9):
        record_id = cells[0]
        confidence, basis = parse_confidence(cells[8])
        row = {
            "class": cells[4],
            "class_system": class_system,
            "current_owner": cells[5],
            "recommendation": cells[6],
            "confidence": confidence,
            "confidence_basis": basis,
            "raw_context_checked": cells[7].lower().startswith("yes"),
            "ledger": path.name,
        }
        if record_id in result:
            raise ValueError(f"duplicate reconciliation row: {record_id}")
        result[record_id] = row
    if len(result) != expected:
        raise ValueError(f"expected {expected} {class_system} rows, found {len(result)}")
    return result


def historical_default(class_name: str) -> tuple[str, float]:
    recommendations = {
        "APP_LOCAL_CONTEXT": "Archive as app-local historical context; promote only a generic falsifier if still current.",
        "AUTHORSHIP_OR_PROVENANCE_WEAK": "Retain provenance but do not attribute supplied or ambiguous prose to Pablo.",
        "CONTRADICTS_CURRENT": "Preserve as correction evidence and reconcile the conflicting current carrier.",
        "EXPLAINS_FRONTIER": "Attach as rationale to the named current owner; do not create a duplicate issue.",
        "MISSING_CURRENT_TEACHING": "Promote the verified teaching to a current owner after checking raw context.",
        "NEEDS_RAW_CONTEXT": "Do not promote until the surrounding source turn resolves its referent and authorship.",
        "NO_ACTIONABLE_TEACHING": "Archive as history; no current work item.",
        "OVERLAPS_ATOMIC": "Cross-link to the stronger current issue or authority; no duplicate work item.",
        "SUPERSEDED_MECHANISM": "Archive the legacy mechanism; retain only any still-current invariant.",
    }
    confidence = 0.65 if class_name in {"AUTHORSHIP_OR_PROVENANCE_WEAK", "NEEDS_RAW_CONTEXT"} else 0.80
    return recommendations[class_name], confidence


def parse_historical(ledger_dir: Path) -> dict[str, dict[str, Any]]:
    result: dict[str, dict[str, Any]] = {}
    paths = sorted(ledger_dir.glob("triage-records-*.md"))
    for path, cells in table_rows(paths, "LNK-", 6):
        record_id = cells[0]
        recommendation, confidence = historical_default(cells[2])
        row = {
            "class": cells[2],
            "class_system": "historical-record-triage-v1",
            "current_owner": cells[3],
            "recommendation": recommendation,
            "confidence": confidence,
            "confidence_basis": "conservative catalog default; historical review did not score rows",
            "raw_context_checked": False,
            "ledger": path.name,
        }
        if record_id in RAW_CONTEXT_RESOLUTIONS:
            class_name, owner, resolution, score = RAW_CONTEXT_RESOLUTIONS[record_id]
            row.update({
                "class": class_name,
                "class_system": "historical-record-raw-context-resolution-v1",
                "current_owner": owner,
                "recommendation": resolution,
                "confidence": score,
                "confidence_basis": "bounded nmp.json context review summarized in raw-evidence-findings.md",
                "raw_context_checked": True,
                "ledger": "raw-evidence-findings.md",
            })
        if record_id in result:
            raise ValueError(f"duplicate historical review: {record_id}")
        result[record_id] = row
    if len(result) != 249:
        raise ValueError(f"expected 249 historical reviews, found {len(result)}")
    unresolved = [key for key, value in result.items() if value["class"] == "NEEDS_RAW_CONTEXT"]
    if unresolved:
        raise ValueError(f"raw-context resolutions missing for: {', '.join(unresolved)}")
    return result


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


def locate_record_dir(repo: Path) -> Path:
    for candidate in (repo / "docs/records/v1", repo / "docs/wiki/legacy-nmp/records"):
        if candidate.is_dir():
            return candidate
    raise FileNotFoundError("could not find committed historical records")


def main() -> None:
    parser = argparse.ArgumentParser()
    parser.add_argument("--source", type=Path, required=True, help="canonical a/final/issues.json")
    parser.add_argument("--live-issues", type=Path, required=True, help="live GitHub issue JSON snapshot")
    parser.add_argument("--ledgers", type=Path, required=True, help="completed private reconciliation-ledger directory")
    parser.add_argument("--repo", type=Path, default=Path(__file__).resolve().parents[1])
    parser.add_argument("--output", type=Path)
    args = parser.parse_args()

    repo = args.repo.resolve()
    output = args.output or repo / "docs/wiki/legacy-nmp/catalog.jsonl"
    source_records = json.loads(args.source.read_text())
    live_issues = json.loads(args.live_issues.read_text())
    if len(source_records) != 3189:
        raise ValueError(f"expected 3,189 canonical records, found {len(source_records)}")
    source_by_id = {record["issue_id"]: record for record in source_records}
    if len(source_by_id) != len(source_records):
        raise ValueError("canonical source contains duplicate issue_id values")

    issue_by_number = {issue["number"]: issue for issue in live_issues}
    atomic_refs: dict[str, list[int]] = {}
    clarification_refs: dict[str, list[int]] = {}
    for issue in live_issues:
        ids = set(ID_RE.findall(issue.get("body") or ""))
        target = None
        if 244 <= issue["number"] <= 409:
            target = atomic_refs
        elif 412 <= issue["number"] <= 437:
            target = clarification_refs
        if target is not None:
            for record_id in ids:
                target.setdefault(record_id, []).append(issue["number"])

    record_dir = locate_record_dir(repo)
    committed_paths: dict[str, str] = {}
    for path in sorted(record_dir.glob("*.md")):
        for record_id in re.findall(r"^### (LNK-[A-Z0-9-]+)", path.read_text(), re.MULTILINE):
            if record_id in committed_paths:
                raise ValueError(f"duplicate committed record heading: {record_id}")
            committed_paths[record_id] = str(path.relative_to(repo))

    partitions = {
        "atomic-github": set(atomic_refs),
        "clarification-github": set(clarification_refs),
        "committed-record": set(committed_paths),
    }
    durable = set().union(*partitions.values())
    partitions["local-only"] = set(source_by_id) - durable
    expected_partition_counts = {
        "atomic-github": 345,
        "clarification-github": 699,
        "committed-record": 249,
        "local-only": 1896,
    }
    actual_partition_counts = {key: len(value) for key, value in partitions.items()}
    if actual_partition_counts != expected_partition_counts:
        raise ValueError(f"carrier partition mismatch: {actual_partition_counts}")
    for left, left_ids in partitions.items():
        for right, right_ids in partitions.items():
            if left < right and left_ids & right_ids:
                raise ValueError(f"carrier overlap between {left} and {right}")

    atomic_reviews = parse_atomic(args.ledgers)
    clarification_reviews = parse_reconciled_rows(
        [args.ledgers / "reconcile-published-clarifications.md"], 699,
        "published-clarification-reconciliation-v1")
    local_reviews = parse_reconciled_rows(
        sorted(args.ledgers.glob("reconcile-local-only-*.md")), 1896,
        "local-only-reconciliation-v1")
    historical_reviews = parse_historical(args.ledgers)

    rows = []
    for record_id in sorted(source_by_id):
        canonical = copy.deepcopy(source_by_id[record_id])
        evidence = canonical.pop("evidence")
        rationale = canonical.pop("rationale_quotes")
        canonical.pop("issue_id")
        source, source_redactions = redact(canonical, "/source")
        evidence, evidence_redactions = redact(evidence, "/evidence")
        rationale, rationale_redactions = redact(rationale, "/rationale_evidence")
        redactions = source_redactions + evidence_redactions + rationale_redactions

        if record_id in partitions["atomic-github"]:
            partition = "atomic-github"
            issue_numbers = sorted(atomic_refs[record_id])
            primary = atomic_reviews[issue_numbers[0]].copy()
            additional = [
                {"github_issue": number, **atomic_reviews[number]}
                for number in issue_numbers[1:]
                if atomic_reviews[number] != atomic_reviews[issue_numbers[0]]
            ]
            reconciliation = primary
            if additional:
                reconciliation["additional_reviews"] = additional
        elif record_id in partitions["clarification-github"]:
            partition = "clarification-github"
            issue_numbers = sorted(clarification_refs[record_id])
            reconciliation = clarification_reviews[record_id]
        elif record_id in partitions["committed-record"]:
            partition = "committed-record"
            issue_numbers = []
            reconciliation = historical_reviews[record_id]
        else:
            partition = "local-only"
            issue_numbers = []
            reconciliation = local_reviews[record_id]

        github_issues = [
            {
                "number": number,
                "title": issue_by_number[number]["title"],
                "url": issue_by_number[number]["url"],
            }
            for number in issue_numbers
        ]
        carrier_classes = sorted({item["carrier_class"] for item in evidence + rationale})
        unsafe = (
            reconciliation["class"] == "AUTHORSHIP_OR_PROVENANCE_WEAK"
            or any(value != "direct_human" for value in carrier_classes)
        )
        row = {
            "schema_version": CATALOG_SCHEMA_VERSION,
            "id": record_id,
            "source": source,
            "evidence": evidence,
            "rationale_evidence": rationale,
            "provenance": {
                "authorship": "unsafe_or_supplied_material" if unsafe else "unverified_not_inferred_from_message_role",
                "message_role_inference_used": False,
                "carrier_classes": carrier_classes,
                "quotes_verified_at_offsets": all(item.get("quote_verified") is True for item in evidence + rationale),
                "quote_sha256": {
                    f"/{'evidence' if key == 'evidence' else 'rationale_evidence'}/{index}": hashlib.sha256(
                        item["quote"].encode()
                    ).hexdigest()
                    for key in ("evidence", "rationale_quotes")
                    for index, item in enumerate(source_by_id[record_id][key])
                },
                "redactions": redactions,
            },
            "carrier": {
                "partition": partition,
                "github_issues": github_issues,
                "committed_record": committed_paths.get(record_id),
            },
            "reconciliation": reconciliation,
        }
        if reconciliation["class"] not in VALID_CLASSES:
            raise ValueError(f"invalid reconciliation class for {record_id}: {reconciliation['class']}")
        rows.append(row)

    output.parent.mkdir(parents=True, exist_ok=True)
    with output.open("w") as handle:
        for row in rows:
            handle.write(json.dumps(row, ensure_ascii=False, separators=(",", ":"), sort_keys=True))
            handle.write("\n")
    print(f"wrote {len(rows)} rows to {output}")
    print("partitions: " + ", ".join(f"{key}={value}" for key, value in actual_partition_counts.items()))
    print(f"redacted sensitive values: {sum(len(row['provenance']['redactions']) for row in rows)}")


if __name__ == "__main__":
    main()
