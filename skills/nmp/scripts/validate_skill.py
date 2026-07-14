#!/usr/bin/env python3
"""Fail-closed validation for the bundled NMP skill."""

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
UI_FIELD_RE = re.compile(
    r'^  (display_name|short_description|default_prompt): "([^"\n]*)"$'
)
EXPECTED_UI_FIELDS = {"display_name", "short_description", "default_prompt"}
FORBIDDEN_FILES = {
    "README.md",
    "CONTENTS.md",
    "manifest.txt",
    "validation.json",
    "CHANGELOG.md",
}


def fail(errors: list[str], message: str) -> None:
    errors.append(message)


def is_within(path: Path, root: Path) -> bool:
    try:
        path.relative_to(root)
        return True
    except ValueError:
        return False


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
        key, value = (part.strip() for part in line.split(":", 1))
        if key in values:
            fail(errors, f"duplicate frontmatter key: {key}")
        values[key] = value
    extra = set(values) - {"name", "description"}
    missing = {"name", "description"} - set(values)
    if extra:
        fail(errors, f"unexpected frontmatter keys: {sorted(extra)}")
    if missing:
        fail(errors, f"missing frontmatter keys: {sorted(missing)}")
    return values


def markdown_targets(markdown: Path) -> list[str]:
    return LINK_RE.findall(markdown.read_text(encoding="utf-8"))


def local_link_path(markdown: Path, target: str) -> Path | None:
    if target.startswith(("https://", "http://", "mailto:")):
        return None
    target = target.split("#", 1)[0]
    if not target:
        return None
    return (markdown.parent / target).resolve()


def find_repo(skill_dir: Path, explicit: Path | None) -> Path | None:
    if explicit:
        return explicit.resolve()
    for parent in [skill_dir, *skill_dir.parents]:
        if (parent / "Cargo.toml").is_file() and (parent / "crates/nmp").is_dir():
            return parent.resolve()
    return None


def official_validator() -> Path | None:
    candidates: list[Path] = []
    if os.environ.get("CODEX_HOME"):
        candidates.append(
            Path(os.environ["CODEX_HOME"])
            / "skills/.system/skill-creator/scripts/quick_validate.py"
        )
    candidates.append(
        Path.home() / ".codex/skills/.system/skill-creator/scripts/quick_validate.py"
    )
    return next((path for path in candidates if path.is_file()), None)


def validate_metadata(skill_dir: Path, errors: list[str]) -> None:
    metadata = skill_dir / "agents/openai.yaml"
    if not metadata.is_file():
        fail(errors, "agents/openai.yaml is required")
        return
    lines = metadata.read_text(encoding="utf-8").splitlines()
    if not lines or lines[0] != "interface:":
        fail(errors, "agents/openai.yaml must contain one interface mapping")
        return
    fields: dict[str, str] = {}
    for line in lines[1:]:
        match = UI_FIELD_RE.fullmatch(line)
        if not match:
            fail(errors, f"unsupported or unquoted agents/openai.yaml line: {line}")
            continue
        key, value = match.groups()
        if key in fields:
            fail(errors, f"duplicate agents/openai.yaml field: {key}")
        fields[key] = value
    if set(fields) != EXPECTED_UI_FIELDS:
        fail(
            errors,
            "agents/openai.yaml must contain exactly display_name, "
            "short_description, and default_prompt",
        )
        return
    if not fields["display_name"].strip():
        fail(errors, "display_name must not be empty")
    if not 25 <= len(fields["short_description"]) <= 64:
        fail(errors, "short_description must be 25-64 characters")
    prompt = fields["default_prompt"]
    if "$nmp" not in prompt:
        fail(errors, "default_prompt must explicitly name $nmp")
    if not prompt or prompt[-1] not in ".?!" or sum(prompt.count(mark) for mark in ".?!") != 1:
        fail(errors, "default_prompt must be one sentence")


def validate_links(skill_dir: Path, markdown_files: list[Path], errors: list[str]) -> None:
    for markdown in markdown_files:
        for target in markdown_targets(markdown):
            path = local_link_path(markdown, target)
            if path is None:
                continue
            if not is_within(path, skill_dir):
                fail(
                    errors,
                    f"local link escapes skill root in {markdown.relative_to(skill_dir)}: {target}",
                )
            elif not path.exists():
                fail(
                    errors,
                    f"broken link in {markdown.relative_to(skill_dir)}: {target}",
                )

    skill_file = skill_dir / "SKILL.md"
    directly_linked = {
        path
        for target in markdown_targets(skill_file)
        if (path := local_link_path(skill_file, target)) is not None
    }
    for reference in sorted((skill_dir / "references").glob("*.md")):
        if reference.resolve() not in directly_linked:
            fail(errors, f"reference is not linked directly from SKILL.md: {reference.name}")


def validate_sources(
    skill_dir: Path,
    markdown_files: list[Path],
    repo: Path | None,
    skip_sources: bool,
    errors: list[str],
) -> None:
    source_map = skill_dir / "references/source-map.md"
    if not source_map.is_file():
        fail(errors, "references/source-map.md is required")
        return
    sources = SOURCE_RE.findall(source_map.read_text(encoding="utf-8"))
    if not sources:
        fail(errors, "source map must declare at least one Source path")

    revisions: list[tuple[Path, str]] = []
    for markdown in markdown_files:
        for revision in REVISION_RE.findall(markdown.read_text(encoding="utf-8")):
            revisions.append((markdown, revision))
    revision_files = {path.relative_to(skill_dir) for path, _ in revisions}
    required_revision_files = {
        Path("SKILL.md"),
        Path("references/current-surface.md"),
    }
    if revision_files != required_revision_files or len(revisions) != 2:
        fail(
            errors,
            "SKILL.md and references/current-surface.md must each declare one Verified-Revision",
        )
        revision = None
    elif revisions[0][1] != revisions[1][1]:
        fail(errors, "Verified-Revision pins do not match")
        revision = None
    else:
        revision = revisions[0][1]

    if skip_sources:
        print("warning: source validation explicitly bypassed for tests", file=sys.stderr)
        return
    if repo is None:
        fail(errors, "NMP repo not found; pass --repo-root")
        return

    checked_sources: list[str] = []
    for source in sources:
        source_path = Path(source)
        resolved = (repo / source_path).resolve()
        if source_path.is_absolute() or not is_within(resolved, repo):
            fail(errors, f"declared source escapes repository root: {source}")
        elif not resolved.exists():
            fail(errors, f"declared source does not exist: {source}")
        else:
            checked_sources.append(source)

    if revision is None or not checked_sources:
        return
    exists = subprocess.run(
        ["git", "-C", str(repo), "cat-file", "-e", f"{revision}^{{commit}}"],
        capture_output=True,
        check=False,
    )
    if exists.returncode:
        fail(errors, f"verified revision is unavailable: {revision}")
        return
    drift = subprocess.run(
        ["git", "-C", str(repo), "diff", "--quiet", revision, "--", *checked_sources],
        capture_output=True,
        check=False,
    )
    if drift.returncode == 1:
        fail(errors, "declared source changed since Verified-Revision; re-audit claims and advance the pin")
    elif drift.returncode != 0:
        fail(errors, "could not compare declared sources with Verified-Revision")


def validate_evaluations(skill_dir: Path, errors: list[str]) -> None:
    prompts = skill_dir / "references/evaluation-prompts.md"
    if not prompts.is_file():
        fail(errors, "references/evaluation-prompts.md is required")
        return
    text = prompts.read_text(encoding="utf-8")
    if len(re.findall(r'^Prompt: "', text, re.MULTILINE)) < 5:
        fail(errors, "evaluation-prompts.md must contain at least five raw prompts")
    forbidden = ("Pass if", "Fail if", "Fail for", "Expected answer", "Rubric:")
    for phrase in forbidden:
        if phrase.lower() in text.lower():
            fail(errors, f"raw evaluation prompts leak answer criteria: {phrase}")


def main() -> int:
    parser = argparse.ArgumentParser()
    parser.add_argument("skill_dir", nargs="?", default=".")
    parser.add_argument("--repo-root", type=Path)
    parser.add_argument("--test-skip-sources", action="store_true", help=argparse.SUPPRESS)
    parser.add_argument("--test-skip-official", action="store_true", help=argparse.SUPPRESS)
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
    if not meta.get("description", "").strip():
        fail(errors, "description is required")

    for path in skill_dir.rglob("*"):
        if path.is_symlink():
            fail(errors, f"skill package must not contain symlinks: {path.relative_to(skill_dir)}")
    for name in sorted(FORBIDDEN_FILES):
        if (skill_dir / name).exists():
            fail(errors, f"remove auxiliary/generated file: {name}")

    markdown_files = sorted(skill_dir.rglob("*.md"))
    validate_links(skill_dir, markdown_files, errors)
    validate_metadata(skill_dir, errors)
    validate_evaluations(skill_dir, errors)
    repo = find_repo(skill_dir, args.repo_root)
    validate_sources(skill_dir, markdown_files, repo, args.test_skip_sources, errors)

    if args.test_skip_official:
        print("warning: official validation explicitly bypassed for tests", file=sys.stderr)
    else:
        validator = official_validator()
        if validator is None:
            fail(errors, "official skill-creator quick_validate.py not found")
        else:
            result = subprocess.run(
                [sys.executable, str(validator), str(skill_dir)],
                text=True,
                capture_output=True,
                check=False,
            )
            if result.returncode:
                detail = (result.stdout + result.stderr).strip()
                fail(errors, f"official validator failed: {detail}")

    if errors:
        for error in errors:
            print(f"error: {error}", file=sys.stderr)
        return 1
    print(f"PASS: {skill_dir} ({len(markdown_files)} markdown files)")
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
