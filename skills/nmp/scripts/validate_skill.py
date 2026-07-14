#!/usr/bin/env python3
"""Validate the NMP skill's identity, links, reachability, and source map."""

from __future__ import annotations

import argparse
import os
from pathlib import Path
import re
import subprocess
import sys


NAME_RE = re.compile(r"^[a-z0-9]+(?:-[a-z0-9]+)*$")
LINK_RE = re.compile(r"\[[^\]]+\]\(([^)]+)\)")
SOURCE_RE = re.compile(r"^- Source: `([^`]+)`\s*$", re.MULTILINE)
REVISION_RE = re.compile(r"^Verified-Revision: `([0-9a-f]{40})`\s*$", re.MULTILINE)


def fail(errors: list[str], message: str) -> None:
    errors.append(message)


def frontmatter(skill_file: Path, errors: list[str]) -> dict[str, str]:
    text = skill_file.read_text(encoding="utf-8")
    if not text.startswith("---\n"):
        fail(errors, "SKILL.md must start with YAML frontmatter")
        return {}
    end = text.find("\n---\n", 4)
    if end < 0:
        fail(errors, "SKILL.md frontmatter is not closed")
        return {}
    values: dict[str, str] = {}
    for line in text[4:end].splitlines():
        if ":" not in line:
            fail(errors, f"unsupported frontmatter line: {line}")
            continue
        key, value = line.split(":", 1)
        values[key.strip()] = value.strip()
    extra = set(values) - {"name", "description"}
    if extra:
        fail(errors, f"unexpected frontmatter keys: {sorted(extra)}")
    return values


def local_links(markdown: Path) -> list[Path]:
    links: list[Path] = []
    for target in LINK_RE.findall(markdown.read_text(encoding="utf-8")):
        target = target.split("#", 1)[0]
        if not target or "://" in target or target.startswith("mailto:"):
            continue
        links.append((markdown.parent / target).resolve())
    return links


def find_repo(skill_dir: Path, explicit: Path | None) -> Path | None:
    if explicit:
        return explicit.resolve()
    for parent in [skill_dir, *skill_dir.parents]:
        if (parent / "Cargo.toml").is_file() and (parent / "crates/nmp").is_dir():
            return parent
    return None


def canonical_validator() -> Path | None:
    candidates = []
    if os.environ.get("CODEX_HOME"):
        candidates.append(Path(os.environ["CODEX_HOME"]) / "skills/.system/skill-creator/scripts/quick_validate.py")
    candidates.append(Path.home() / ".codex/skills/.system/skill-creator/scripts/quick_validate.py")
    return next((path for path in candidates if path.is_file()), None)


def main() -> int:
    parser = argparse.ArgumentParser()
    parser.add_argument("skill_dir", nargs="?", default=".")
    parser.add_argument("--repo-root", type=Path)
    parser.add_argument("--skip-sources", action="store_true")
    parser.add_argument("--skip-canonical", action="store_true")
    args = parser.parse_args()

    skill_dir = Path(args.skill_dir).resolve()
    errors: list[str] = []
    skill_file = skill_dir / "SKILL.md"
    if not skill_file.is_file():
        print("error: SKILL.md not found", file=sys.stderr)
        return 1

    meta = frontmatter(skill_file, errors)
    name = meta.get("name", "")
    if not NAME_RE.fullmatch(name):
        fail(errors, f"name must be lowercase hyphen-case: {name!r}")
    if name != skill_dir.name:
        fail(errors, f"name {name!r} must match folder {skill_dir.name!r}")
    if not meta.get("description"):
        fail(errors, "description is required")

    markdown_files = sorted(skill_dir.rglob("*.md"))
    for markdown in markdown_files:
        for target in local_links(markdown):
            if not target.exists():
                fail(errors, f"broken link in {markdown.relative_to(skill_dir)}: {target}")

    directly_linked = set(local_links(skill_file))
    for reference in sorted((skill_dir / "references").glob("*.md")):
        if reference.resolve() not in directly_linked:
            fail(errors, f"reference is not linked directly from SKILL.md: {reference.name}")

    forbidden = {"README.md", "CONTENTS.md", "manifest.txt", "validation.json", "CHANGELOG.md"}
    for name in sorted(forbidden):
        if (skill_dir / name).exists():
            fail(errors, f"remove auxiliary/generated file: {name}")

    repo = find_repo(skill_dir, args.repo_root)
    source_map = skill_dir / "references/source-map.md"
    sources = SOURCE_RE.findall(source_map.read_text(encoding="utf-8")) if source_map.is_file() else []
    if repo and not args.skip_sources:
        for source in sources:
            if not (repo / source).exists():
                fail(errors, f"declared source does not exist: {source}")
        surface = skill_dir / "references/current-surface.md"
        revisions = REVISION_RE.findall(surface.read_text(encoding="utf-8")) if surface.is_file() else []
        if len(revisions) != 1:
            fail(errors, "current-surface.md must contain exactly one Verified-Revision")
        elif sources:
            revision = revisions[0]
            exists = subprocess.run(
                ["git", "-C", str(repo), "cat-file", "-e", f"{revision}^{{commit}}"],
                capture_output=True,
                check=False,
            )
            if exists.returncode:
                fail(errors, f"verified revision is unavailable: {revision}")
            else:
                drift = subprocess.run(
                    ["git", "-C", str(repo), "diff", "--quiet", revision, "--", *sources],
                    check=False,
                )
                if drift.returncode == 1:
                    fail(errors, "declared source changed since Verified-Revision; re-audit claims and advance the pin")
                elif drift.returncode != 0:
                    fail(errors, "could not compare declared sources with Verified-Revision")
    elif args.skip_sources:
        print("warning: source-path and revision validation explicitly skipped", file=sys.stderr)
    else:
        fail(errors, "NMP repo not found; pass --repo-root or explicitly use --skip-sources")

    validator = canonical_validator()
    if not args.skip_canonical and validator:
        result = subprocess.run(
            [sys.executable, str(validator), str(skill_dir)],
            text=True,
            capture_output=True,
            check=False,
        )
        if result.returncode:
            detail = (result.stdout + result.stderr).strip()
            fail(errors, f"canonical validator failed: {detail}")
    elif not args.skip_canonical:
        print("warning: canonical skill-creator validator not found", file=sys.stderr)

    if errors:
        for error in errors:
            print(f"error: {error}", file=sys.stderr)
        return 1
    print(f"PASS: {skill_dir} ({len(markdown_files)} markdown files)")
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
