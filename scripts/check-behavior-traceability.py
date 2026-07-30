#!/usr/bin/env python3
"""Validate machine-readable status and evidence on governed behavior files.

The legacy root ``features/`` tree is being migrated owner by owner under
issue #1071. A behavior becomes governed when its ``.feature`` file moves
beside its semantic crate owner under ``crates/*/tests/behavior/`` or into the
public-facade acceptance target. This physical boundary permits incremental
migration without a second manifest or an allowlist that could become another
source of truth.
"""

from __future__ import annotations

import argparse
import re
import sys
from dataclasses import dataclass
from pathlib import Path
from typing import Iterable, Sequence


ID_RE = re.compile(r"^[A-Z][A-Z0-9]*-[A-Z][A-Z0-9]*-[0-9]{3}$")
ISSUE_RE = re.compile(
    r"^(?:#?[1-9][0-9]*|https://github\.com/[^/]+/[^/]+/issues/[1-9][0-9]*)$"
)
EVIDENCE_RE = re.compile(
    r"^(rust|property|compile|script|swift|kotlin):"
    r"([a-z0-9][a-z0-9-]*)::([A-Za-z0-9_./:-]+)$"
)
SCENARIO_RE = re.compile(r"^\s*Scenario(?: Outline)?:\s*(\S.*)$")
FEATURE_RE = re.compile(r"^\s*Feature:\s*(\S.*)$")
RULE_RE = re.compile(r"^\s*Rule:\s*(\S.*)$")
METADATA_RE = re.compile(r"^\s*#\s*nmp:([a-z-]+)=(\S.*)$")
TAG_RE = re.compile(r"@([A-Za-z0-9_-]+)")

ALLOWED_STATUSES = {"specified", "built", "known-violation"}
ALLOWED_GAPS = {"implementation", "evidence", "fixture", "platform"}
ALLOWED_KEYS = {"id", "status", "evidence", "falsifier", "gap", "issue"}
FORBIDDEN_TAGS = {"wip", "designed", "live"}


@dataclass(frozen=True)
class Problem:
    path: Path
    line: int
    message: str

    def render(self, root: Path) -> str:
        try:
            display = self.path.relative_to(root)
        except ValueError:
            display = self.path
        return f"{display}:{self.line}: {self.message}"


@dataclass(frozen=True)
class ScenarioRecord:
    path: Path
    line: int
    title: str
    metadata: dict[str, str]
    tags: frozenset[str]


def governed_files(root: Path) -> list[Path]:
    """Return every behavior file that has moved into a semantic owner."""
    files = set(root.glob("crates/*/tests/behavior/**/*.feature"))
    files.update(root.glob("crates/nmp/tests/acceptance/**/*.feature"))
    return sorted(path for path in files if path.is_file())


def supplied_files(paths: Sequence[Path]) -> list[Path]:
    files: set[Path] = set()
    for path in paths:
        if path.is_dir():
            files.update(path.rglob("*.feature"))
        else:
            files.add(path)
    return sorted(files)


def is_acceptance_path(path: Path) -> bool:
    return "/crates/nmp/tests/acceptance/" in f"/{path.as_posix().lstrip('/')}"


def metadata_before(lines: list[str], scenario_index: int) -> tuple[dict[str, str], set[str], list[str]]:
    """Read the contiguous metadata/tag block immediately above a scenario."""
    cursor = scenario_index - 1
    tags: set[str] = set()
    while cursor >= 0 and lines[cursor].lstrip().startswith("@"):
        tags.update(TAG_RE.findall(lines[cursor]))
        cursor -= 1

    metadata_lines: list[str] = []
    while cursor >= 0 and METADATA_RE.match(lines[cursor]):
        metadata_lines.append(lines[cursor])
        cursor -= 1
    metadata_lines.reverse()

    metadata: dict[str, str] = {}
    duplicates: list[str] = []
    for line in metadata_lines:
        match = METADATA_RE.match(line)
        assert match is not None
        key, value = match.groups()
        if key in metadata:
            duplicates.append(key)
        metadata[key] = value.strip()
    return metadata, tags, duplicates


def parse_file(path: Path) -> tuple[list[ScenarioRecord], list[Problem]]:
    try:
        lines = path.read_text(encoding="utf-8").splitlines()
    except (OSError, UnicodeError) as error:
        return [], [Problem(path, 1, f"cannot read behavior file: {error}")]

    problems: list[Problem] = []
    feature_lines = [index for index, line in enumerate(lines) if FEATURE_RE.match(line)]
    if len(feature_lines) != 1:
        problems.append(
            Problem(path, 1, f"expected exactly one Feature block, found {len(feature_lines)}")
        )

    records: list[ScenarioRecord] = []
    for index, line in enumerate(lines):
        scenario = SCENARIO_RE.match(line)
        if scenario is None:
            continue
        line_number = index + 1
        if not feature_lines or feature_lines[0] > index:
            problems.append(Problem(path, line_number, "Scenario appears before its Feature"))

        metadata, tags, duplicates = metadata_before(lines, index)
        for key in duplicates:
            problems.append(Problem(path, line_number, f"duplicate nmp:{key} metadata"))
        records.append(
            ScenarioRecord(
                path=path,
                line=line_number,
                title=scenario.group(1),
                metadata=metadata,
                tags=frozenset(tags),
            )
        )

    if not records:
        problems.append(Problem(path, 1, "Feature contains no Scenario or Scenario Outline"))

    # Parsing Rules is intentional even though status belongs to scenarios.
    # A Rule before Feature is structurally invalid, and catching it prevents
    # a malformed hierarchy from looking governed merely because IDs parse.
    for index, line in enumerate(lines):
        if RULE_RE.match(line) and (not feature_lines or feature_lines[0] > index):
            problems.append(Problem(path, index + 1, "Rule appears before its Feature"))

    return records, problems


def validate_record(record: ScenarioRecord) -> list[Problem]:
    problems: list[Problem] = []
    metadata = record.metadata

    if not metadata:
        return [
            Problem(
                record.path,
                record.line,
                "missing contiguous nmp metadata immediately above Scenario",
            )
        ]

    unknown = sorted(set(metadata) - ALLOWED_KEYS)
    for key in unknown:
        problems.append(Problem(record.path, record.line, f"unknown nmp metadata key: {key}"))

    scenario_id = metadata.get("id")
    if scenario_id is None:
        problems.append(Problem(record.path, record.line, "missing required nmp:id"))
    elif ID_RE.fullmatch(scenario_id) is None:
        problems.append(
            Problem(
                record.path,
                record.line,
                f"invalid nmp:id {scenario_id!r}; expected <DOMAIN>-<CONTEXT>-<NNN>",
            )
        )

    status = metadata.get("status")
    if status is None:
        problems.append(Problem(record.path, record.line, "missing required nmp:status"))
    elif status not in ALLOWED_STATUSES:
        problems.append(
            Problem(
                record.path,
                record.line,
                f"invalid nmp:status {status!r}; allowed: {', '.join(sorted(ALLOWED_STATUSES))}",
            )
        )

    evidence = metadata.get("evidence")
    if evidence is not None and EVIDENCE_RE.fullmatch(evidence) is None:
        problems.append(
            Problem(
                record.path,
                record.line,
                f"invalid nmp:evidence locator {evidence!r}",
            )
        )

    issue = metadata.get("issue")
    if issue is not None and ISSUE_RE.fullmatch(issue) is None:
        problems.append(
            Problem(record.path, record.line, f"invalid nmp:issue locator {issue!r}")
        )

    gap = metadata.get("gap")
    if gap is not None and gap not in ALLOWED_GAPS:
        problems.append(
            Problem(
                record.path,
                record.line,
                f"invalid nmp:gap {gap!r}; allowed: {', '.join(sorted(ALLOWED_GAPS))}",
            )
        )

    if status == "built":
        if evidence is None:
            problems.append(
                Problem(record.path, record.line, "built scenario requires nmp:evidence")
            )
        if not metadata.get("falsifier"):
            problems.append(
                Problem(record.path, record.line, "built scenario requires nmp:falsifier")
            )
        if gap is not None:
            problems.append(
                Problem(record.path, record.line, "built scenario cannot carry nmp:gap")
            )
    elif status == "specified":
        if gap is None:
            problems.append(
                Problem(record.path, record.line, "specified scenario requires nmp:gap")
            )
        if issue is None:
            problems.append(
                Problem(record.path, record.line, "specified scenario requires nmp:issue")
            )
    elif status == "known-violation" and issue is None:
        problems.append(
            Problem(record.path, record.line, "known-violation scenario requires nmp:issue")
        )

    forbidden = sorted(
        tag
        for tag in record.tags
        if tag in FORBIDDEN_TAGS or tag.startswith("requires-")
    )
    for tag in forbidden:
        problems.append(
            Problem(
                record.path,
                record.line,
                f"@{tag} is forbidden as lifecycle/applicability metadata",
            )
        )

    if "acceptance" in record.tags and status != "built":
        problems.append(
            Problem(
                record.path,
                record.line,
                "@acceptance is allowed only on a built public-facade capstone",
            )
        )
    if "acceptance" in record.tags and not is_acceptance_path(record.path):
        problems.append(
            Problem(
                record.path,
                record.line,
                "@acceptance scenario must live under crates/nmp/tests/acceptance",
            )
        )
    evidence_match = EVIDENCE_RE.fullmatch(evidence) if evidence is not None else None
    if (
        "acceptance" in record.tags
        and evidence_match is not None
        and evidence_match.group(2) != "nmp"
    ):
        problems.append(
            Problem(
                record.path,
                record.line,
                "@acceptance evidence must be owned by the nmp public-facade target",
            )
        )

    return problems


def resolve_evidence(record: ScenarioRecord, root: Path) -> list[Problem]:
    locator = record.metadata.get("evidence")
    if locator is None or EVIDENCE_RE.fullmatch(locator) is None:
        return []

    kind, owner, target = EVIDENCE_RE.fullmatch(locator).groups()  # type: ignore[union-attr]
    if kind in {"rust", "property"}:
        owner_root = root / "crates" / owner
        if not owner_root.is_dir():
            return [
                Problem(
                    record.path,
                    record.line,
                    f"evidence owner crate does not exist: crates/{owner}",
                )
            ]
        test_name = target.rsplit("::", 1)[-1]
        definition = re.compile(
            rf"\b(?:async\s+)?fn\s+{re.escape(test_name)}\s*\("
        )
        for source in owner_root.rglob("*.rs"):
            try:
                if definition.search(source.read_text(encoding="utf-8")):
                    return []
            except (OSError, UnicodeError):
                continue
        return [
            Problem(
                record.path,
                record.line,
                f"evidence test {test_name!r} is absent from crates/{owner}",
            )
        ]

    if kind in {"script", "compile"} and "/" in target:
        if not (root / target).is_file():
            return [
                Problem(
                    record.path,
                    record.line,
                    f"evidence path does not exist: {target}",
                )
            ]
    return []


def validate(paths: Iterable[Path], root: Path, *, resolve: bool = True) -> tuple[list[ScenarioRecord], list[Problem]]:
    records: list[ScenarioRecord] = []
    problems: list[Problem] = []
    for path in paths:
        parsed, parse_problems = parse_file(path)
        records.extend(parsed)
        problems.extend(parse_problems)
        for record in parsed:
            problems.extend(validate_record(record))
            if resolve:
                problems.extend(resolve_evidence(record, root))

    by_id: dict[str, ScenarioRecord] = {}
    for record in records:
        scenario_id = record.metadata.get("id")
        if scenario_id is None or ID_RE.fullmatch(scenario_id) is None:
            continue
        previous = by_id.get(scenario_id)
        if previous is not None:
            problems.append(
                Problem(
                    record.path,
                    record.line,
                    f"duplicate nmp:id {scenario_id}; first declared at "
                    f"{previous.path}:{previous.line}",
                )
            )
        else:
            by_id[scenario_id] = record
    return records, problems


def parse_args(argv: Sequence[str]) -> argparse.Namespace:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument(
        "paths",
        nargs="*",
        type=Path,
        help="specific .feature files/directories; defaults to governed owner paths",
    )
    parser.add_argument(
        "--no-resolve-evidence",
        action="store_true",
        help="validate locator syntax without resolving it (test-fixture use only)",
    )
    return parser.parse_args(argv)


def main(argv: Sequence[str] | None = None) -> int:
    args = parse_args(sys.argv[1:] if argv is None else argv)
    root = Path(__file__).resolve().parent.parent
    paths = supplied_files(args.paths) if args.paths else governed_files(root)
    if not paths:
        print("behavior-traceability: no governed .feature files found", file=sys.stderr)
        return 2

    records, problems = validate(
        paths, root, resolve=not args.no_resolve_evidence
    )
    if problems:
        for problem in problems:
            print(problem.render(root), file=sys.stderr)
        print(
            f"behavior-traceability: FAIL ({len(problems)} problem(s), "
            f"{len(records)} scenario(s), {len(paths)} file(s))",
            file=sys.stderr,
        )
        return 1

    print(
        f"behavior-traceability: ok "
        f"({len(records)} scenario(s), {len(paths)} governed file(s))"
    )
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
