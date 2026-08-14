#!/usr/bin/env python3
"""Report production/test/documentation/generated line counts per Rust file
and per workspace package (#1561).

WHY THIS EXISTS. #1496 is decomposing several outsized files by hand with no
measurement behind it. Its own completion criterion -- production, test,
documentation and generated lines counted separately -- had no tool: someone
had to count by hand, and a naive `wc -l` is actively misleading because
`crates/nmp/src/runtime/mod.rs` and `crates/nmp/src/engine.rs` both interleave
`#[cfg(test)]` modules through the file rather than trailing a single test
module at the end. This script is that tool.

THIS IS A REPORT, NOT A GATE. It prints review triggers for a human --
500 / 1,000 / 1,500 non-test lines in one file, and workspace packages under
roughly 250 production lines (the opposite failure: a crate hollowed out
during decomposition, as happened to `nmp-executor`). Crossing a trigger
never fails the build; the exit code reports tool failure only (bad
arguments, an unreadable file, a broken `cargo metadata` call), never a
crossed threshold. Turning a trigger into a required check, and any named
per-file waiver mechanism, is a separate future decision, not this one.

CLASSIFICATION, AND ITS LIMITS. Every physical line of every tracked `.rs`
file under a root-workspace package lands in exactly one class:

  - generated    the file's first 5 lines carry an `@generated` marker (the
                 convention already used by protobuf/buf and Bazel gazelle,
                 not new repository vocabulary); the whole file counts here
                 and nothing else about it is examined. No tracked file in
                 this workspace carries the marker today -- uniffi's Swift/
                 Kotlin/Rust-scaffolding output is gitignored (`/gen`,
                 `/Packages/*/Sources/*FFI`), never tracked -- so this class
                 reads zero everywhere today. It exists so a future checked-in
                 generator has somewhere honest to land instead of silently
                 inflating "production".
  - test         (a) every file under a `tests/`, `benches/`, or `examples/`
                 directory (Cargo's own test/bench/example convention -- none
                 of that is shipped library surface), whole file; or (b) any
                 line that a `#[cfg(test)]`-gated item's brace scope covers,
                 tracked by walking real (non-comment/non-string/non-char-
                 literal) `{`/`}` pairs so a `#[cfg(test)] mod tests { ... }`
                 nested in the middle of a production file -- not trailing
                 it -- is still excluded from "production", however deep the
                 module is or however many separate cfg(test) blocks the file
                 has; or (c) a whole *separate* file reached only through a
                 `#[cfg(test)] mod name;` (or `#[path = "..."]`-overridden)
                 external module declaration -- `crates/nmp/src/engine.rs`'s
                 own trailing `#[cfg(test)] mod tests;` names
                 `crates/nmp/src/engine/tests.rs`, and that file carries no
                 `#[cfg(test)]` of its own, so it is reached by following
                 declarations rather than by anything inside the file itself.
                 The same following is applied transitively: a file already
                 reached this way (e.g. `core/admission_tests.rs`) that
                 further splits into `#[path = "admission_tests/clock.rs"]
                 mod clock;` siblings pulls those in too, however many levels
                 deep. `#[cfg(any(test, feature = "..."))]`-gated code (e.g.
                 `store.rs`'s `test-instrumentation`/`bench-instrumentation`
                 fields, or `redb_store/mod.rs`'s `#[path = "testing_tests.rs"]
                 pub mod testing`) is deliberately NOT test here: it compiles
                 into ordinary release builds behind a real Cargo feature and
                 is production instrumentation, not test-only code, so it
                 does not seed this following either.
  - documentation any remaining line whose content (after leading whitespace)
                 starts with `///` or `//!` -- outer/inner doc comments.
                 (`////`-prefixed separator comments are excluded, matching
                 rustdoc's own rule that four-or-more slashes is not a doc
                 comment. This workspace uses no `/** */`/`/*! */` block doc
                 comments, so those are not specially handled.)
  - production   everything else: ordinary code, ordinary `//`/`/* */`
                 comments, blank lines, closing braces -- the same
                 "count everything" convention `check-bdd-file-length.sh`
                 already uses for its 600-line cap.

This is a heuristic brace/string/comment scanner, not a real Rust parser. It
correctly skips braces inside string, raw-string, and char literals (`'{'`
appears literally in this workspace) and does not mistake a lifetime (`'a`)
for a char literal. It does not understand macros that manufacture cfg-gated
items token-by-token, and if a file's braces are ever genuinely unbalanced (a
sign this heuristic is the wrong tool for that file) it says so on stderr
per-file rather than silently mis-reporting.

Packages are the root `Cargo.toml` workspace's own members, read from
`cargo metadata --no-deps` -- the same source of truth
`check-dependency-direction.sh` uses -- not re-derived from directory
guessing. Tracked `.rs` files that belong to a *different*, detached
Cargo workspace (`tools/behavior-traceability`, `tools/fjall-journal-fault/
v3_1_*`, `benchmarks/*`) are outside this root workspace's package graph by
design (see the root `Cargo.toml`'s own comments) and are reported as
unassigned rather than silently dropped or force-fitted into a package they
are not part of.
"""

from __future__ import annotations

import json
import posixpath
import re
import subprocess
import sys
from dataclasses import dataclass, field
from pathlib import Path
from typing import Dict, List, Optional, Set, Tuple

GENERATED_MARKER = "@generated"
NON_PRODUCTION_DIR_COMPONENTS = {"tests", "benches", "examples"}

# A plain external module declaration: `mod name;` / `pub mod name;` /
# `pub(crate) mod name;`, with no `{ ... }` body -- the declaration that
# means "this module's content lives in another file", resolved the same way
# rustc resolves it: `<dir of this file>/name.rs` or `<dir of this
# file>/name/mod.rs`, unless overridden by an immediately preceding
# `#[path = "..."]`, which is resolved relative to that same directory.
MOD_DECL_RE = re.compile(r"^\s*(?:pub(?:\([^)]*\))?\s+)?mod\s+([A-Za-z_][A-Za-z0-9_]*)\s*;\s*$")
PATH_ATTR_RE = re.compile(r'^\s*#\[path\s*=\s*"([^"]+)"\]\s*$')

# rustc's own default submodule directory rule: a `mod.rs` (or crate-root
# `lib.rs`/`main.rs`) resolves an un-attributed `mod name;` inside its own
# directory; any other file `x.rs` resolves it inside a sibling `x/`
# directory named after itself -- `engine.rs`'s `mod tests;` names
# `engine/tests.rs`, not a `tests.rs` next to `engine.rs`.
MOD_STYLE_BASENAMES = {"mod.rs", "lib.rs", "main.rs"}

REVIEW_TRIGGER = 500
ASSESS_TRIGGER = 1000
FAIL_TRIGGER = 1500
PACKAGE_PRODUCTION_FLOOR = 250

LINE_CLASSES = ("production", "test", "documentation", "generated")


class ReportError(Exception):
    """A tool-level failure: bad arguments, unreadable input, no git/cargo."""


@dataclass
class FileReport:
    package: Optional[str]
    path: str
    counts: Dict[str, int]
    balanced: bool

    @property
    def total(self) -> int:
        return sum(self.counts[c] for c in LINE_CLASSES)

    @property
    def non_test(self) -> int:
        return self.total - self.counts["test"]

    def tier(self) -> Optional[str]:
        n = self.non_test
        if n >= FAIL_TRIGGER:
            return "FAIL(>=1500)"
        if n >= ASSESS_TRIGGER:
            return "ASSESS(>=1000)"
        if n >= REVIEW_TRIGGER:
            return "REVIEW(>=500)"
        return None


@dataclass
class PackageReport:
    name: str
    files: List[FileReport] = field(default_factory=list)

    def counts(self) -> Dict[str, int]:
        totals = {c: 0 for c in LINE_CLASSES}
        for f in self.files:
            for c in LINE_CLASSES:
                totals[c] += f.counts[c]
        return totals

    def total(self) -> int:
        return sum(self.counts().values())

    def under_floor(self) -> bool:
        return self.counts()["production"] < PACKAGE_PRODUCTION_FLOOR


def run_git(root: Path, *args: str) -> str:
    try:
        out = subprocess.run(
            ["git", "-C", str(root), *args],
            check=True,
            capture_output=True,
            text=True,
        )
    except FileNotFoundError as error:
        raise ReportError("git is not on PATH") from error
    except subprocess.CalledProcessError as error:
        raise ReportError(
            "git {} failed: {}".format(" ".join(args), error.stderr.strip())
        ) from error
    return out.stdout


def resolve_root(root_arg: str) -> Path:
    root = Path(root_arg)
    if not root.is_dir():
        raise ReportError("{} is not a directory".format(root))
    top = run_git(root, "rev-parse", "--show-toplevel").strip()
    physical_root = root.resolve()
    physical_top = Path(top).resolve()
    if physical_root != physical_top:
        raise ReportError(
            "{} is not the repository top level ({}); tracked paths and "
            "package directories would disagree".format(root, top)
        )
    return physical_root


def tracked_rust_files(root: Path) -> List[str]:
    out = run_git(root, "ls-files", "-z", "--", "*.rs")
    files = [p for p in out.split("\0") if p]
    if not files:
        raise ReportError(
            "no tracked .rs file found under {}; a scan over it would be vacuous".format(
                root
            )
        )
    return files


def workspace_packages(root: Path) -> Dict[str, str]:
    """Return {package_dir_relative_to_root: package_name} for every member
    of the root workspace, from `cargo metadata` -- never by guessing which
    directories look like crates."""
    manifest = root / "Cargo.toml"
    try:
        out = subprocess.run(
            [
                "cargo",
                "metadata",
                "--no-deps",
                "--format-version",
                "1",
                "--manifest-path",
                str(manifest),
            ],
            check=True,
            capture_output=True,
            text=True,
        )
    except FileNotFoundError as error:
        raise ReportError("cargo is not on PATH") from error
    except subprocess.CalledProcessError as error:
        raise ReportError(
            "cargo metadata failed: {}".format(error.stderr.strip())
        ) from error

    try:
        data = json.loads(out.stdout)
    except json.JSONDecodeError as error:
        raise ReportError("cargo metadata printed invalid JSON: {}".format(error)) from error

    packages: Dict[str, str] = {}
    for pkg in data.get("packages", []):
        manifest_path = Path(pkg["manifest_path"]).resolve()
        try:
            rel_dir = manifest_path.parent.relative_to(root).as_posix()
        except ValueError:
            # A package whose manifest lives outside this checkout (should not
            # happen for a workspace member); skip rather than mis-assign.
            continue
        packages[rel_dir] = pkg["name"]
    if not packages:
        raise ReportError("cargo metadata reported zero workspace packages")
    return packages


def owning_package(file_path: str, package_dirs: List[str]) -> Optional[str]:
    """Longest-prefix match on path components, so `crates/nmp` never claims
    a file that actually belongs to `crates/nmp-store`."""
    best: Optional[str] = None
    for pkg_dir in package_dirs:
        if file_path == pkg_dir or file_path.startswith(pkg_dir + "/"):
            if best is None or len(pkg_dir) > len(best):
                best = pkg_dir
    return best


def _char_literal_len(line: str, j: int) -> int:
    """Length (including both quotes) of the char literal starting at
    `line[j] == "'"`, or 0 if this is a lifetime (`'a`, `'static`), not a
    literal."""
    n = len(line)
    if j + 1 >= n:
        return 0
    if line[j + 1] == "\\":
        k = j + 2
        if k < n and line[k] == "u" and k + 1 < n and line[k + 1] == "{":
            k += 2
            while k < n and line[k] != "}":
                k += 1
            k += 1  # step over '}'
        else:
            k += 1  # step over the escaped character itself
        if k < n and line[k] == "'":
            return k - j + 1
        return 0
    if j + 2 < n and line[j + 2] == "'":
        return 3
    return 0


def classify_lines(lines: List[str]) -> Tuple[List[str], bool]:
    """Classify each physical line into one of LINE_CLASSES. Returns
    (classes, balanced) where `balanced` is False if the file's `{`/`}`
    nesting never returned to zero -- a signal this heuristic could not
    safely track cfg(test) scope for the whole file."""
    n = len(lines)
    if n == 0:
        return [], True

    for line in lines[:5]:
        if GENERATED_MARKER in line:
            return ["generated"] * n, True

    classes = ["production"] * n
    test_stack = [False]
    pending_test = False
    state = "code"  # code | block_comment | string | raw_string
    block_depth = 0
    raw_hashes = 0

    for i, line in enumerate(lines):
        state_at_line_start = state
        line_test = test_stack[-1]
        lstripped = line.lstrip()

        j = 0
        if state == "code" and lstripped.startswith("#[cfg(test)]"):
            pending_test = True
            line_test = True
            j = len(line) - len(lstripped) + len("#[cfg(test)]")

        ln = len(line)
        while j < ln:
            c = line[j]
            if state == "code":
                if c == "/" and j + 1 < ln and line[j + 1] == "/":
                    break  # rest of the physical line is a line comment
                if c == "/" and j + 1 < ln and line[j + 1] == "*":
                    state = "block_comment"
                    block_depth = 1
                    j += 2
                    continue
                if c == "r" and j + 1 < ln and (line[j + 1] == '"' or line[j + 1] == "#"):
                    k = j + 1
                    hashes = 0
                    while k < ln and line[k] == "#":
                        hashes += 1
                        k += 1
                    if k < ln and line[k] == '"':
                        state = "raw_string"
                        raw_hashes = hashes
                        j = k + 1
                        continue
                if c == '"':
                    state = "string"
                    j += 1
                    continue
                if c == "'":
                    consumed = _char_literal_len(line, j)
                    if consumed:
                        j += consumed
                        continue
                    j += 1  # a lifetime, not a literal -- keep scanning
                    continue
                if c == "{":
                    is_test = pending_test or test_stack[-1]
                    test_stack.append(is_test)
                    pending_test = False
                    if is_test:
                        line_test = True
                    j += 1
                    continue
                if c == "}":
                    was_test = test_stack.pop() if len(test_stack) > 1 else test_stack[0]
                    if was_test:
                        line_test = True
                    j += 1
                    continue
                if pending_test or test_stack[-1]:
                    line_test = True
                if c == ";" and pending_test:
                    pending_test = False
                j += 1
                continue
            if state == "block_comment":
                if c == "*" and j + 1 < ln and line[j + 1] == "/":
                    block_depth -= 1
                    j += 2
                    if block_depth == 0:
                        state = "code"
                    continue
                if c == "/" and j + 1 < ln and line[j + 1] == "*":
                    block_depth += 1
                    j += 2
                    continue
                j += 1
                continue
            if state == "string":
                if c == "\\":
                    j += 2
                    continue
                if c == '"':
                    state = "code"
                    j += 1
                    continue
                j += 1
                continue
            if state == "raw_string":
                if c == '"' and line[j + 1 : j + 1 + raw_hashes] == "#" * raw_hashes:
                    j = j + 1 + raw_hashes
                    state = "code"
                    continue
                j += 1
                continue

        if line_test:
            classes[i] = "test"
        elif state_at_line_start == "code" and (
            (lstripped.startswith("///") and not lstripped.startswith("////"))
            or lstripped.startswith("//!")
        ):
            classes[i] = "documentation"
        else:
            classes[i] = "production"

    return classes, len(test_stack) == 1


def is_dir_bypass(rel_path: str) -> bool:
    parts = Path(rel_path).parts
    return any(component in NON_PRODUCTION_DIR_COMPONENTS for component in parts[:-1])


def is_generated(lines: List[str]) -> bool:
    return any(GENERATED_MARKER in line for line in lines[:5])


def extract_mod_decls(lines: List[str]) -> List[Tuple[int, str, Optional[str]]]:
    """Every plain `mod name;` declaration in `lines`, as
    (line_index, module_name, path_override_or_None). `path_override` is the
    string from an immediately preceding `#[path = "..."]`, when present."""
    decls: List[Tuple[int, str, Optional[str]]] = []
    for i, line in enumerate(lines):
        match = MOD_DECL_RE.match(line)
        if not match:
            continue
        path_override = None
        if i > 0:
            path_match = PATH_ATTR_RE.match(lines[i - 1])
            if path_match:
                path_override = path_match.group(1)
        decls.append((i, match.group(1), path_override))
    return decls


def resolve_mod_target(
    rel_path: str, name: str, path_override: Optional[str], tracked: Set[str]
) -> Optional[str]:
    """The tracked file a `mod name;` declaration inside `rel_path` names,
    resolved the way rustc resolves it, or None if no tracked file matches."""
    base_dir = posixpath.dirname(rel_path)
    if path_override is not None:
        # `#[path = "..."]` is always relative to the declaring file's own
        # directory, regardless of that file's name.
        candidate = posixpath.normpath(posixpath.join(base_dir, path_override))
        return candidate if candidate in tracked else None

    basename = posixpath.basename(rel_path)
    if basename in MOD_STYLE_BASENAMES:
        submodule_dir = base_dir
    else:
        stem = basename[: -len(".rs")] if basename.endswith(".rs") else basename
        submodule_dir = posixpath.join(base_dir, stem)
    for suffix in (name + ".rs", name + "/mod.rs"):
        candidate = posixpath.normpath(posixpath.join(submodule_dir, suffix))
        if candidate in tracked:
            return candidate
    return None


def build_report(root: Path) -> Tuple[List[FileReport], List[str]]:
    packages = workspace_packages(root)
    package_dirs = sorted(packages.keys(), key=len, reverse=True)
    files = tracked_rust_files(root)
    tracked: Set[str] = set(files)

    file_lines: Dict[str, List[str]] = {}
    file_kind: Dict[str, str] = {}  # "generated" | "dirtest" | "normal"
    file_classes: Dict[str, List[str]] = {}
    file_balanced: Dict[str, bool] = {}
    mod_decls: Dict[str, List[Tuple[int, str, Optional[str]]]] = {}

    for rel_path in files:
        try:
            text = (root / rel_path).read_text(encoding="utf-8")
        except (OSError, UnicodeDecodeError) as error:
            raise ReportError("cannot read {}: {}".format(rel_path, error)) from error
        lines = text.splitlines()
        file_lines[rel_path] = lines
        mod_decls[rel_path] = extract_mod_decls(lines)

        if is_generated(lines):
            file_kind[rel_path] = "generated"
        elif is_dir_bypass(rel_path):
            file_kind[rel_path] = "dirtest"
        else:
            file_kind[rel_path] = "normal"
            file_classes[rel_path], file_balanced[rel_path] = classify_lines(lines)

    # A file reached only through an external `#[cfg(test)] mod name;` (or a
    # `#[path = ...]`-overridden sibling of one already reached that way)
    # carries no test marker of its own -- `engine/tests.rs` has none -- so it
    # must be found by following declarations, not by scanning its own text.
    forced_test: Set[str] = {p for p in files if file_kind[p] == "dirtest"}
    frontier: Set[str] = set(forced_test)

    for rel_path, decls in mod_decls.items():
        if file_kind[rel_path] != "normal":
            continue
        classes = file_classes[rel_path]
        for line_index, name, path_override in decls:
            if classes[line_index] != "test":
                continue
            target = resolve_mod_target(rel_path, name, path_override, tracked)
            if target and target not in forced_test:
                forced_test.add(target)
                frontier.add(target)

    while frontier:
        rel_path = frontier.pop()
        for line_index, name, path_override in mod_decls.get(rel_path, []):
            target = resolve_mod_target(rel_path, name, path_override, tracked)
            if target and target not in forced_test:
                forced_test.add(target)
                frontier.add(target)

    file_reports: List[FileReport] = []
    unassigned: List[str] = []
    for rel_path in sorted(files):
        pkg_dir = owning_package(rel_path, package_dirs)
        pkg_name = packages[pkg_dir] if pkg_dir is not None else None
        lines = file_lines[rel_path]
        kind = file_kind[rel_path]
        balanced = True

        if kind == "generated":
            counts = {"generated": len(lines), "test": 0, "documentation": 0, "production": 0}
        elif kind == "dirtest" or rel_path in forced_test:
            counts = {"test": len(lines), "production": 0, "documentation": 0, "generated": 0}
        else:
            classes = file_classes[rel_path]
            balanced = file_balanced[rel_path]
            counts = {c: 0 for c in LINE_CLASSES}
            for c in classes:
                counts[c] += 1

        if not balanced:
            print(
                "report-source-concentration: WARNING: unbalanced braces in {}; "
                "test/production split may be inaccurate for this file".format(rel_path),
                file=sys.stderr,
            )
        file_reports.append(FileReport(pkg_name, rel_path, counts, balanced))
        if pkg_name is None:
            unassigned.append(rel_path)

    return file_reports, unassigned


def format_row(cols: List[str], widths: List[int]) -> str:
    return "  ".join(col.ljust(w) for col, w in zip(cols, widths))


def render(file_reports: List[FileReport], unassigned: List[str], full: bool) -> str:
    out: List[str] = []
    out.append(
        "Source concentration report (#1561) -- production/test/documentation/generated "
        "line counts.\nReview triggers only: crossing a threshold below is a signal for a "
        "human, never a build failure.\n"
    )

    packages: Dict[str, PackageReport] = {}
    for f in file_reports:
        if f.package is None:
            continue
        packages.setdefault(f.package, PackageReport(f.package)).files.append(f)

    out.append("== Per-package totals ==")
    header = ["package", "files", "total", "production", "test", "doc", "generated", "flag"]
    rows = [header]
    for name in sorted(packages):
        pkg = packages[name]
        counts = pkg.counts()
        flag = "UNDER-FLOOR(<{})".format(PACKAGE_PRODUCTION_FLOOR) if pkg.under_floor() else ""
        rows.append(
            [
                name,
                str(len(pkg.files)),
                str(pkg.total()),
                str(counts["production"]),
                str(counts["test"]),
                str(counts["documentation"]),
                str(counts["generated"]),
                flag,
            ]
        )
    widths = [max(len(r[i]) for r in rows) for i in range(len(header))]
    for r in rows:
        out.append(format_row(r, widths))
    out.append("")

    flagged = sorted(
        (f for f in file_reports if f.tier() is not None),
        key=lambda f: f.non_test,
        reverse=True,
    )
    out.append("== Flagged files (non-test lines >= {}) ==".format(REVIEW_TRIGGER))
    if not flagged:
        out.append("(none)")
    else:
        fheader = ["tier", "package", "path", "total", "production", "test", "doc", "generated", "non_test"]
        frows = [fheader]
        for f in flagged:
            c = f.counts
            frows.append(
                [
                    f.tier() or "",
                    f.package or "(unassigned)",
                    f.path,
                    str(f.total),
                    str(c["production"]),
                    str(c["test"]),
                    str(c["documentation"]),
                    str(c["generated"]),
                    str(f.non_test),
                ]
            )
        fwidths = [max(len(r[i]) for r in frows) for i in range(len(fheader))]
        for r in frows:
            out.append(format_row(r, fwidths))
    out.append("")

    if full:
        out.append("== Every tracked .rs file ==")
        aheader = ["package", "path", "total", "production", "test", "doc", "generated", "non_test", "tier"]
        arows = [aheader]
        for f in sorted(file_reports, key=lambda f: (f.package or "", f.path)):
            c = f.counts
            arows.append(
                [
                    f.package or "(unassigned)",
                    f.path,
                    str(f.total),
                    str(c["production"]),
                    str(c["test"]),
                    str(c["documentation"]),
                    str(c["generated"]),
                    str(f.non_test),
                    f.tier() or "",
                ]
            )
        awidths = [max(len(r[i]) for r in arows) for i in range(len(aheader))]
        for r in arows:
            out.append(format_row(r, awidths))
        out.append("")

    if unassigned:
        out.append(
            "{} tracked .rs file(s) belong to a Cargo workspace other than the root "
            "Cargo.toml (detached tool/benchmark trees) and are not aggregated into any "
            "package above. Pass --full to see every file, or --file <path> to inspect "
            "one directly:".format(len(unassigned))
        )
        for path in sorted(unassigned):
            out.append("  " + path)
        out.append("")

    return "\n".join(out)


def render_single(file_reports: List[FileReport], rel_path: str) -> str:
    matches = [f for f in file_reports if f.path == rel_path]
    if not matches:
        raise ReportError("{} is not a tracked .rs file under this root".format(rel_path))
    f = matches[0]
    lines = [
        "{}  (package: {}):".format(f.path, f.package or "(unassigned)"),
        "  total:          {}".format(f.total),
        "  production:     {}".format(f.counts["production"]),
        "  test:           {}".format(f.counts["test"]),
        "  documentation:  {}".format(f.counts["documentation"]),
        "  generated:      {}".format(f.counts["generated"]),
        "  non_test:       {}".format(f.non_test),
    ]
    if not f.balanced:
        lines.append(
            "  WARNING: unbalanced braces detected; test/production split may be inaccurate"
        )
    return "\n".join(lines)


def main(argv: List[str]) -> int:
    if not argv:
        print("usage: report-source-concentration.py <root> [--full] [--file <path>]", file=sys.stderr)
        return 2

    root_arg = argv[0]
    rest = argv[1:]
    full = False
    single_file: Optional[str] = None
    i = 0
    while i < len(rest):
        if rest[i] == "--full":
            full = True
            i += 1
        elif rest[i] == "--file":
            if i + 1 >= len(rest):
                print("--file requires a path argument", file=sys.stderr)
                return 2
            single_file = rest[i + 1]
            i += 2
        else:
            print("unrecognized argument: {}".format(rest[i]), file=sys.stderr)
            return 2

    try:
        root = resolve_root(root_arg)
        file_reports, unassigned = build_report(root)
        if single_file is not None:
            print(render_single(file_reports, single_file))
        else:
            print(render(file_reports, unassigned, full))
    except ReportError as error:
        print("report-source-concentration: {}".format(error), file=sys.stderr)
        return 1

    return 0


if __name__ == "__main__":
    sys.exit(main(sys.argv[1:]))
