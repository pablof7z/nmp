#!/usr/bin/env python3
"""Generate BUILD.bazel files for every first-party NMP crate.

Cargo remains the dependency-metadata authority: crate_universe imports the
third-party graph from Cargo.toml/Cargo.lock into @crates (see MODULE.bazel),
and `all_crate_deps()` resolves the third-party labels per package. This script
fills what crate_universe does NOT -- first-party path dependencies, target
shapes (lib / bin / integration tests / examples), and the Cargo *feature*
model for first-party crates, which rules_rust has no native support for.

Feature model
-------------
Cargo features do not map onto a Bazel concept, so they are modeled explicitly:

  * The production library `:lib` is built with the crate's **default** feature
    set only (mirrors `cargo build` with default features). Optional path deps
    that the `default` feature does not activate are not on the dep graph.

  * A `:lib_test` variant is built with the **workspace-unified test feature
    set** -- the union of every feature any dev-dependency edge in the workspace
    requests on that crate, expanded through Cargo's feature cascade
    (`dep:foo` activates optional dep foo; `othercrate/feat` enables feat on
    othercrate; a bare name does both). This mirrors `cargo test --workspace`'s
    feature unification: every crate is compiled once with the unified feature
    set, and first-party edges inside the test graph point at `:lib_test` so the
    cascade propagates. `--cfg feature="..."` flags carry the cfg side; the
    optional deps the features activate are added to the dep set.

  * Every test and example target (the things `cargo test` compiles) reaches its
    first-party deps through their `:lib_test` variants and gets the crate's
    dev-dependencies. Binaries (the things `cargo build` compiles) reach their
    first-party deps through `:lib` and get only normal deps.

Run from the workspace root:

    python3 tools/bazel/gen_buildfiles.py

Re-runnable; overwrites every first-party BUILD.bazel it owns (header-marked so
a human can tell generated from hand-edited). nmp-ffi is hand-maintained (multi
crate-type + optional feature surface) and skipped here.
"""

import json
import os
import re
import subprocess

# `mod support;` (optionally `pub mod support;`) declares a child module whose
# source is a sibling file (`support.rs`) or directory (`support/mod.rs`), and
# that module can itself declare further children. Cargo/rustc walks this tree
# from the crate root automatically; rules_rust compiles only the files in
# `srcs`, so we mirror the walk and list every reachable .rs file explicitly.
_MOD_RE = re.compile(r'^\s*(?:pub\s+)?mod\s+([A-Za-z_][A-Za-z0-9_]*)\s*;')
# `#[path = "../examples/support/probe.rs"]` overrides where `mod X;` looks.
# The path is relative to the file containing the declaration. Tracked as a
# pending attribute applied to the next `mod X;` (other attrs like #[cfg(test)]
# may sit between them).
_PATH_ATTR_RE = re.compile(r'^\s*#\[\s*path\s*=\s*"([^"]+)"\s*\]')


def collect_mod_sources(root_rel, mdir):
    """Return extra .rs files reachable from root_rel via `mod X;` declarations.

    Mirrors rustc's module resolution. For `mod X;` in a file `<dir>/<name>.rs`
    (or `<dir>/mod.rs`), candidates in priority order:
      <dir>/<name>/X.rs        (submodule dir named after the file, non-mod.rs)
      <dir>/<name>/X/mod.rs
      <dir>/X.rs               (sibling -- the mod.rs / crate-root form)
      <dir>/X/mod.rs
    The first existing candidate wins; that file is scanned for further `mod`s.
    A preceding `#[path = "..."]` attribute overrides resolution: the path is
    taken relative to the declaring file's directory.
    """
    seen = set()
    extra = []

    def walk(rel):
        if rel in seen:
            return
        seen.add(rel)
        fpath = os.path.join(mdir, rel)
        if not os.path.isfile(fpath):
            return
        d = os.path.dirname(rel)
        stem = os.path.splitext(os.path.basename(rel))[0]
        subdir = os.path.join(d, stem) if stem != "mod" else None
        pending_path = None
        with open(fpath, "r", encoding="utf-8", errors="replace") as fh:
            for line in fh:
                pm = _PATH_ATTR_RE.match(line)
                if pm:
                    pending_path = pm.group(1)
                    continue
                m = _MOD_RE.match(line)
                if not m:
                    # Only carry the path attr across non-mod lines that are
                    # themselves attributes (e.g. #[cfg(test)]); a blank/code
                    # line clears it, matching rustc's "immediately preceding".
                    if line.strip() and not line.strip().startswith("#"):
                        pending_path = None
                    continue
                mod = m.group(1)
                if pending_path:
                    c = os.path.normpath(os.path.join(d, pending_path))
                    pending_path = None
                    if os.path.isfile(os.path.join(mdir, c)) and c not in seen:
                        extra.append(c)
                        walk(c)
                    continue
                cands = []
                if subdir:
                    cands.append(os.path.join(subdir, mod + ".rs"))
                    cands.append(os.path.join(subdir, mod, "mod.rs"))
                cands.append(os.path.join(d, mod + ".rs"))
                cands.append(os.path.join(d, mod, "mod.rs"))
                for c in cands:
                    if os.path.isfile(os.path.join(mdir, c)):
                        extra.append(c)
                        walk(c)
                        break

    walk(root_rel)
    return extra


def _nearest_pkg_with_build(path, root):
    """Deepest ancestor of `path` (up to root) containing a BUILD.bazel."""
    cur = os.path.dirname(path)
    while True:
        if cur == root or os.path.isfile(os.path.join(cur, "BUILD.bazel")):
            return cur
        parent = os.path.dirname(cur)
        if parent == cur:
            return root
        cur = parent


def collect_compile_data(src_paths, mdir, root):
    """compile_data labels for `include_str!/include_bytes!` targets.

    Returns (labels, out_of_pkg_labels). `labels` are BUILD attr entries for
    compile_data (same-package relative labels for in-package non-.rs files
    not already in the src glob, plus cross-package labels for files outside
    this crate). `out_of_pkg_labels` are labels that need exports_files in the
    owning package's BUILD (root or another package) -- surfaced so the root
    BUILD can be hand-updated.
    """
    labels = []
    out_of_pkg = []
    seen = set()
    for rel in src_paths:
        fpath = os.path.join(mdir, rel)
        if not os.path.isfile(fpath):
            continue
        with open(fpath, "r", encoding="utf-8", errors="replace") as fh:
            text = fh.read()
        for m in INCLUDE_RE.finditer(text):
            inc = m.group(1)
            base = os.path.dirname(rel)
            target_abs = os.path.normpath(os.path.join(mdir, base, inc))
            in_pkg = (os.path.commonpath([mdir, target_abs]) == mdir
                      and target_abs != mdir)
            if in_pkg:
                rel_in_pkg = os.path.relpath(target_abs, mdir)
                # .rs files under src/ are already in the `src/**/*.rs` glob.
                if rel_in_pkg.endswith(".rs") and (rel_in_pkg == "src/" or rel_in_pkg.startswith("src/")):
                    continue
                key = rel_in_pkg
            else:
                pkg_dir = _nearest_pkg_with_build(target_abs, root)
                file_in_pkg = os.path.relpath(target_abs, pkg_dir).replace(os.sep, "/")
                if pkg_dir == root:
                    key = "//:" + file_in_pkg
                else:
                    key = "//" + os.path.relpath(pkg_dir, root).replace(os.sep, "/") + ":" + file_in_pkg
                out_of_pkg.append(key)
            if key not in seen:
                seen.add(key)
                labels.append('"%s"' % key)
    return labels, sorted(set(out_of_pkg))

HEADER = """# @generated by tools/bazel/gen_buildfiles.py -- do not edit by hand.
# First-party BUILD for the `{name}` crate. Third-party deps come from
# @crates via all_crate_deps(); first-party path deps + the Cargo feature
# model (lib vs lib_test) are explicit. Regenerate with:
#   python3 tools/bazel/gen_buildfiles.py
"""

# No first-party crates are hand-maintained: every workspace member (including
# nmp-ffi, whose staticlib/cdylib crate types and uniffi-bindgen bin are handled
# by gen_for) is generated from `cargo metadata`.
HAND_MAINTAINED = set()
PUBLIC = '    visibility = ["//visibility:public"],'


def run_cargo_metadata():
    out = subprocess.check_output(
        ["cargo", "metadata", "--format-version", "1", "--no-deps"],
        text=True,
    )
    return json.loads(out)


def is_first_party(name):
    return name == "nmp" or name.startswith("nmp-")


def target_kind(t):
    # Cargo reports kind as a list (e.g. a cdylib+staticlib+rlib lib target has
    # kind ["staticlib","cdylib","lib"]). Classify by membership so a multi
    # crate-type lib is still recognised as a lib, not as its first kind entry.
    k = t.get("kind") or []
    for kind in ("lib", "bin", "test", "example", "bench"):
        if kind in k:
            return kind
    return k[0] if k else ""


def normal_fp_deps(pkg):
    """Non-optional first-party normal dep names (always active under default)."""
    out = []
    for d in pkg["dependencies"]:
        if d.get("kind") is not None:
            continue
        n = d["name"]
        if n == pkg["name"] or not is_first_party(n):
            continue
        if d.get("optional"):
            continue
        out.append(n)
    return sorted(set(out))


def dev_fp_deps(pkg):
    """First-party dev-dependency names (always available to tests/examples)."""
    out = []
    for d in pkg["dependencies"]:
        if d.get("kind") != "dev":
            continue
        n = d["name"]
        if n == pkg["name"] or not is_first_party(n):
            continue
        out.append(n)
    return sorted(set(out))


def default_feature_set(pkg):
    """All feature names reachable from the `default` feature via the cascade."""
    features = pkg.get("features", {})
    if "default" not in features:
        return set()
    out = set()
    stack = list(features["default"])
    while stack:
        e = stack.pop()
        if e in out:
            continue
        if e.startswith("dep:") or "/" in e:
            continue  # dep activation / cross-crate feature -- not a local feature
        out.add(e)
        if e in features:
            stack.extend(features[e])
    return out


def required_features_satisfied(t, active):
    rf = t.get("required-features") or []
    return all(f in active for f in rf)


def active_optional_default(pkg):
    """Optional first-party normal deps activated by the `default` feature."""
    features = pkg.get("features", {})
    if "default" not in features:
        return set()
    optional_names = {
        d["name"] for d in pkg["dependencies"]
        if d.get("kind") is None and d.get("optional") and is_first_party(d["name"])
    }
    active = set()
    stack = list(features.get("default", []))
    visited = set()
    while stack:
        e = stack.pop()
        if e in visited:
            continue
        visited.add(e)
        if e.startswith("dep:"):
            if e[4:] in optional_names:
                active.add(e[4:])
        elif "/" in e:
            crate = e.split("/", 1)[0]
            if crate in optional_names:
                active.add(crate)
        elif e in optional_names:
            active.add(e)
        elif e in features:
            stack.extend(features[e])
    return active


def unified_features(members_by_name, include_dev):
    """Workspace-unified feature set + active optional deps per crate.

    Cargo unifies features across a build: each crate is compiled with the union
    of features requested by every active edge in the build graph, expanded
    through the feature cascade (`dep:foo` activates optional dep foo;
    `othercrate/feat` enables feat on othercrate; a bare name does both).

    For the production workspace build (`cargo build --workspace`) the active
    edges are the normal (build) deps of every workspace member. For the test
    build (`cargo test --workspace`) dev-dependency edges are active too. This
    function computes that unified set: `include_dev=False` gives the
    production-unified set used by `:lib`; `include_dev=True` adds dev-edge
    activations for `:lib_test` / tests / examples.

    Seeds each crate with its `default` feature (expanded by the cascade, so
    `dep:`/`/` entries in default activate the right optional deps), plus the
    `features=[...]` lists on every active edge. Then runs the cascade fixpoint.
    Returns (feats: {name:set(feat)}, active_opt: {name:set(dep)}). `feats`
    includes the synthetic name "default"; callers filter it out of cfg flags.
    """
    feats = {n: set() for n in members_by_name}
    active_opt = {n: set() for n in members_by_name}
    # seed: default feature (present on every crate that has one) + per-edge
    # feature requests from active edges (normal always; dev when include_dev).
    for n, p in members_by_name.items():
        if "default" in p.get("features", {}):
            feats[n].add("default")
    for p in members_by_name.values():
        for d in p["dependencies"]:
            kind = d.get("kind")
            is_dev = kind == "dev"
            if not (kind is None or (include_dev and is_dev)):
                continue
            tgt = d["name"]
            if tgt in members_by_name:
                for f in d.get("features") or []:
                    feats[tgt].add(f)
    # fixpoint cascade
    changed = True
    while changed:
        changed = False
        for n, p in members_by_name.items():
            ftable = p.get("features", {})
            optional_names = {
                dd["name"] for dd in p["dependencies"]
                if dd.get("kind") is None and dd.get("optional") and is_first_party(dd["name"])
            }
            for f in list(feats[n]):
                for e in ftable.get(f, []):
                    if e.startswith("dep:"):
                        dep = e[4:]
                        if dep in optional_names and dep not in active_opt[n]:
                            active_opt[n].add(dep)
                            changed = True
                    elif "/" in e:
                        oc, of = e.split("/", 1)
                        if oc in optional_names and oc not in active_opt[n]:
                            active_opt[n].add(oc)
                            changed = True
                        if oc in members_by_name and of not in feats[oc]:
                            feats[oc].add(of)
                            changed = True
                    else:
                        if e in optional_names and e not in active_opt[n]:
                            active_opt[n].add(e)
                            changed = True
                        if e in ftable and e not in feats[n]:
                            feats[n].add(e)
                            changed = True
    return feats, active_opt


def label(dep_name, test):
    return "//crates/" + dep_name + (":lib_test" if test else ":lib")


INCLUDE_RE = re.compile(r'include_(?:str|bytes|)! *\(\s*"([^"]+)"\s*\)')

_CARGO_BIN_EXE_RE = re.compile(r'CARGO_BIN_EXE_([A-Za-z0-9_-]+)')
# `env!("CARGO")` (the cargo binary path) is a Cargo-only compile-time var for
# tests that spawn cargo itself (e.g. `cargo tree` dependency-boundary gates).
# rules_rust does not set it; bake "cargo" so `env!()` resolves and the spawn
# resolves cargo from PATH at runtime.
_CARGO_EXE_RE = re.compile(r'env!\("CARGO"\)')
# Tests that spawn the host `git` (e.g. `git ls-files` / `git rev-parse` / a
# `git clone file://<repo>` external-consumer check) need the real .git repo
# and git on PATH -- they cannot run hermetically under Bazel. Likewise
# `env!("CARGO")` tests spawn the host cargo. Such tests are tagged `manual`
# so `bazel test //...` skips them (the "where practical" boundary: a test that
# shells out to the legacy build tool to introspect the workspace does not
# translate to a hermetic Bazel run). They remain runnable explicitly, e.g.
# `bazel test --spawn_strategy=local //crates/nmp-content:dependency_boundary`
# with cargo/git on PATH.
_GIT_CMD_RE = re.compile(r'Command\s*::\s*new\s*\(\s*"git"')


def cargo_env_refs(src_paths, mdir):
    """(bin_exe_names, needs_cargo, uses_git) from scanning the given sources."""
    names = set()
    needs_cargo = False
    uses_git = False
    for rel in src_paths:
        fpath = os.path.join(mdir, rel)
        if not os.path.isfile(fpath):
            continue
        with open(fpath, "r", encoding="utf-8", errors="replace") as fh:
            text = fh.read()
        names.update(_CARGO_BIN_EXE_RE.findall(text))
        if _CARGO_EXE_RE.search(text):
            needs_cargo = True
        if _GIT_CMD_RE.search(text):
            uses_git = True
    return sorted(names), needs_cargo, uses_git


def gen_for(pkg, prod_feats, prod_opt, test_feats, test_opt_all, bin_label_by_name, root):
    name = pkg["name"]
    mdir = os.path.dirname(pkg["manifest_path"])
    crate_name = name.replace("-", "_")
    fp_normal = normal_fp_deps(pkg)
    fp_dev = dev_fp_deps(pkg)
    # Production-unified (`cargo build --workspace`): normal first-party deps +
    # optional first-party deps activated by any normal edge / default. The
    # test build adds dev-edge activations on top.
    prod_opt_n = prod_opt.get(name, set())
    test_opt_n = test_opt_all.get(name, set())
    prod_fp = sorted(set(fp_normal) | prod_opt_n)
    test_fp = sorted(set(fp_normal) | test_opt_n)

    targets = pkg.get("targets", [])
    lib_targets = [t for t in targets if target_kind(t) == "lib"]
    bin_targets = [t for t in targets if target_kind(t) == "bin"]
    test_targets = [t for t in targets if target_kind(t) == "test"]
    example_targets = [t for t in targets if target_kind(t) == "example"]

    def cfg_flags(fset):
        out = []
        for f in sorted(fset):
            if f == "default":
                continue  # default features are implicit, never a cfg flag
            out.append('"--cfg"')
            out.append('\'feature="%s"\'' % f)
        return out

    # Cargo sets CARGO_BIN_EXE_<name> at compile time for integration tests/
    # examples that spawn a workspace binary. rules_rust does not, so wire the
    # binary as a data dep and bake its runfiles path into the same env var via
    # rustc_env (compile-time, so `env!()` resolves). The runfiles-root-relative
    # path is valid from the test's CWD under `bazel test`.
    #
    # Returns (env_entries, labels) rather than formatted lines so callers can
    # MERGE these with their own env/data (notably CARGO_MANIFEST_DIR and
    # //:first_party_sources) into a single rustc_env / data attribute --
    # rules_rust rejects duplicate keyword args, so two `rustc_env = {...}`
    # lines (or two `data = [...]` lines) on one target is a hard error.
    def bin_exe_env(src_paths):
        names, needs_cargo, uses_git = cargo_env_refs(src_paths, mdir)
        labels = []
        env_entries = []
        if needs_cargo:
            env_entries.append('"CARGO": "cargo"')
        for n in names:
            lbl = bin_label_by_name.get(n)
            if lbl is None:
                continue
            labels.append(lbl)
            # $(rootpath ...) yields the workspace-relative path (no repo prefix).
            # Under `bazel test` the CWD is the main repo's runfiles subdir (e.g.
            # `_main/`), so a workspace-relative path resolves correctly; the
            # `$(rlocationpath ...)` form bakes the `_main/`-prefixed path, which
            # from that CWD double-prefixes and is not found.
            env_entries.append('"CARGO_BIN_EXE_%s": "$(rootpath %s)"' % (n, lbl))
        # host_tool = spawns the host cargo (`env!("CARGO")`) or git -- cannot
        # run hermetically; the caller tags the target `manual`.
        host_tool = needs_cargo or uses_git
        return env_entries, labels, host_tool

    # Emit a single `rustc_env = {...}` and a single `data = [...]` line merging
    # the bin-exe env/labels with (optionally) the CARGO_MANIFEST_DIR override
    # and the //:first_party_sources runfiles tree. Either line is omitted when
    # it would be empty. Tests pass manifest=first_party=True (source-scanning
    # falsifiers read crate sources via env!("CARGO_MANIFEST_DIR")); examples
    # and bins pass False (they only need CARGO_BIN_EXE_/CARGO when they spawn
    # a workspace binary).
    def emit_env_data(L, env_entries, labels, manifest=True, first_party=True):
        all_env = list(env_entries)
        if manifest:
            # "./crates/<name>" (not "crates/<name>") so a test computing the
            # workspace root as `Path::new(env!("CARGO_MANIFEST_DIR")).parent()
            # .parent()` gets "." (non-empty) rather than "" -- an empty root
            # passed as a CLI arg value is rejected by clap ("a value is
            # required"). Under `bazel test` the CWD is the main repo's runfiles
            # subdir, so "." resolves to the workspace root and "src" joins
            # resolve to <crate>/src as expected. The leading "./" is harmless
            # for `join()` and the OS normalizes "." / ".." on open.
            all_env.append('"CARGO_MANIFEST_DIR": "./crates/%s"' % name)
        if all_env:
            L.append("    rustc_env = {%s}," % ", ".join(all_env))
        all_data = []
        if first_party:
            # //:first_party_sources = every crate's src/**/*.rs (cross-crate
            # source scanners); //:workspace_data = root non-crate test inputs
            # (features/**/*.feature for the cucumber acceptance suite,
            # native/** for the nmp-cli catalog/contracts suite). Both are
            # workspace-relative and resolve from the `_main/` runfiles CWD.
            all_data.append('"//:first_party_sources"')
            all_data.append('"//:workspace_data"')
        all_data.extend('"%s"' % l for l in labels)
        if all_data:
            L.append("    data = [%s]," % ", ".join(all_data))

    # Production-unified cfg flags for `:lib` (matches `cargo build --workspace`
    # feature unification) and test-unified cfg flags for `:lib_test` / tests /
    # examples (matches `cargo test --workspace`, which adds dev-edge features and
    # `--cfg test` on the lib). The `default` feature is filtered out of cfg flags.
    prod_flags = cfg_flags(prod_feats.get(name, set()))
    feat_flags = cfg_flags(test_feats.get(name, set()))
    # :lib_test is the test-closure DEPENDENCY variant, not a cfg(test) build.
    # Under `cargo test`, a crate's lib is linked by *other* crates' test
    # closures with the unified feature set but WITHOUT cfg(test) and WITHOUT
    # that consumer's dev-deps -- only the test root (the crate's own unit
    # tests, integration tests, examples) gets cfg(test) + dev-deps. Modeling
    # lib_test as the dep variant (no --cfg test, no dev-deps) matches Cargo
    # exactly and, crucially, breaks dev-dependency cycles: a testkit that this
    # crate dev-depends on is NOT pulled into lib_test (dev-deps live only on
    # test targets), so the testkit's normal edge back to this crate's lib_test
    # can no longer form a cycle.
    lib_test_flags = feat_flags

    # Actual source files compiled by :lib / :lib_test (the glob expansion),
    # used to find include_str!/include_bytes! targets needing compile_data.
    lib_src_files = []
    for dp, _dn, fns in os.walk(os.path.join(mdir, "src")):
        for fn in fns:
            if fn.endswith(".rs"):
                lib_src_files.append(os.path.relpath(os.path.join(dp, fn), mdir))

    def compile_data_lines(src_paths):
        labels, _oop = collect_compile_data(src_paths, mdir, root)
        # Cargo.toml is always a compile input: proc-macros (uniffi, etc.) read
        # it via CARGO_MANIFEST_DIR to resolve the package name/version at
        # expansion time, and rules_rust does not add it to the sandbox by default.
        labels = ['"Cargo.toml"'] + labels
        return ["    compile_data = [%s]," % ", ".join(labels)]

    def deps_block(test_graph, dev=False, extra_first_party=None):
        """Compose the deps attribute.

        test_graph: first-party edges point at :lib_test (True) or :lib (False).
        dev:        include external dev-dependencies (test/example targets and
                    the :lib_test variant, which under `cargo test` is compiled
                    with dev-deps on the graph; never on the production :lib).
        """
        kw = "normal=True, normal_dev=True" if dev else "normal=True"
        parts = ["all_crate_deps(%s)" % kw]
        labels = []
        base = test_fp if test_graph else prod_fp
        labels.extend(label(n, test_graph) for n in base)
        if extra_first_party:
            labels.extend(extra_first_party)
        if dev:
            labels.extend(label(n, True) for n in fp_dev)
        seen = set()
        ulabels = []
        for l in labels:
            if l not in seen:
                seen.add(l)
                ulabels.append(l)
        if ulabels:
            parts.append("[%s]" % ", ".join('"%s"' % l for l in ulabels))
        return " + ".join(parts)

    L = []
    L.append(HEADER.format(name=name))
    L.append('load("@crates//:defs.bzl", "aliases", "all_crate_deps", "crate_edition")')
    need = set()
    if lib_targets:
        need.add("rust_library")
    if bin_targets or example_targets:
        need.add("rust_binary")
    if test_targets or lib_targets:
        need.add("rust_test")
    # non-rlib crate types (staticlib/cdylib) on the lib target get their own
    # rust_static_library / rust_shared_library targets -- a single
    # rust_library produces one rlib; Cargo's `crate-type = [rlib, staticlib,
    # cdylib]` is modeled as :lib (rlib) plus one target per extra type.
    extra_crate_types = []
    if lib_targets:
        extra_crate_types = sorted(set(lib_targets[0].get("crate_types", [])) - {"bin", "lib"})
        if "staticlib" in extra_crate_types:
            need.add("rust_static_library")
        if "cdylib" in extra_crate_types:
            need.add("rust_shared_library")
    L.append('load("@rules_rust//rust:defs.bzl", %s)' % ", ".join('"%s"' % s for s in sorted(need)))
    L.append("")

    srcs = '    srcs = glob(["src/**/*.rs"]),'

    # production library -- production-unified features (`cargo build --workspace`).
    # First-party edges point at :lib; no dev-deps, no --cfg test. This is the
    # rlib variant only; staticlib/cdylib crate types on the same Cargo lib target
    # get separate targets emitted just below.
    if lib_targets:
        L.append("rust_library(")
        L.append('    name = "lib",')
        L.append('    crate_name = "%s",' % crate_name)
        L.append(srcs)
        L.append("    edition = crate_edition(),")
        L.append("    aliases = aliases(),")
        L.append("    deps = %s," % deps_block(test_graph=False))
        L.append("    proc_macro_deps = all_crate_deps(proc_macro=True),")
        L.extend(compile_data_lines(lib_src_files))
        if prod_flags:
            L.append("    rustc_flags = [%s]," % ", ".join(prod_flags))
        L.append(PUBLIC)
        L.append(")")
        L.append("")

    # staticlib / cdylib companions to :lib (same srcs, prod features, prod dep
    # edges). Cargo's `crate-type = [rlib, staticlib, cdylib]` builds all three;
    # rules_rust builds one crate type per target, so emit one per extra type.
    if "staticlib" in extra_crate_types:
        L.append("rust_static_library(")
        L.append('    name = "%s_static",' % crate_name)
        L.append('    crate_name = "%s",' % crate_name)
        L.append(srcs)
        L.append("    edition = crate_edition(),")
        L.append("    aliases = aliases(),")
        L.append("    deps = %s," % deps_block(test_graph=False))
        L.append("    proc_macro_deps = all_crate_deps(proc_macro=True),")
        L.extend(compile_data_lines(lib_src_files))
        if prod_flags:
            L.append("    rustc_flags = [%s]," % ", ".join(prod_flags))
        L.append(PUBLIC)
        L.append(")")
        L.append("")
    if "cdylib" in extra_crate_types:
        L.append("rust_shared_library(")
        L.append('    name = "%s_cdylib",' % crate_name)
        L.append('    crate_name = "%s",' % crate_name)
        L.append(srcs)
        L.append("    edition = crate_edition(),")
        L.append("    aliases = aliases(),")
        L.append("    deps = %s," % deps_block(test_graph=False))
        L.append("    proc_macro_deps = all_crate_deps(proc_macro=True),")
        L.extend(compile_data_lines(lib_src_files))
        if prod_flags:
            L.append("    rustc_flags = [%s]," % ", ".join(prod_flags))
        L.append(PUBLIC)
        L.append(")")
        L.append("")

    # test-closure library variant -- the dep edge other crates' test closures
    # link against. Carries the test-unified feature cfgs but NO --cfg test and
    # NO dev-deps: dependencies never inherit the consumer's cfg(test) or
    # dev-deps in Cargo, and keeping dev-deps off this variant is what breaks
    # dev-dependency cycles (the crate's testkit dev-dep is not pulled in here,
    # so the testkit's normal back-edge to :lib_test cannot form a cycle).
    # cfg(test) + dev-deps are added only on the unit_tests target below.
    if lib_targets:
        L.append("rust_library(")
        L.append('    name = "lib_test",')
        L.append('    crate_name = "%s",' % crate_name)
        L.append(srcs)
        L.append("    edition = crate_edition(),")
        L.append("    aliases = aliases(),")
        L.append("    deps = %s," % deps_block(test_graph=True, dev=False))
        L.append("    proc_macro_deps = all_crate_deps(proc_macro=True),")
        L.extend(compile_data_lines(lib_src_files))
        if lib_test_flags:
            L.append("    rustc_flags = [%s]," % ", ".join(lib_test_flags))
        L.append(PUBLIC)
        L.append(")")
        L.append("")

    # inline unit tests. rust_test(crate = ":lib_test") recompiles the lib's
    # srcs with --test (giving cfg(test)) using the test-unified feature cfgs
    # (rust_test does not inherit the lib's rustc_flags, so feat_flags are set
    # here). The lib's code is compiled *into* the test binary, so :lib_test is
    # NOT also listed as a dep (that would double-link the rlib). dev-deps are
    # present (dev=True) so `#[cfg(test)] use some_dev_dep` in the lib resolves.
    # data + rustc_env: source-scanning falsifier tests read crate sources at
    # runtime via `env!("CARGO_MANIFEST_DIR")`; rules_rust bakes that to the
    # compile execroot path (invalid in the run sandbox), so override it to a
    # runfiles-relative path (CWD is the workspace runfiles root) and stage the
    # whole workspace source tree as runfiles via //:first_party_sources.
    if lib_targets:
        L.append("rust_test(")
        L.append('    name = "unit_tests",')
        L.append('    crate = ":lib_test",')
        L.append("    edition = crate_edition(),")
        L.append("    aliases = aliases(normal_dev=True, proc_macro_dev=True),")
        L.append("    deps = %s," % deps_block(test_graph=True, dev=True))
        L.append("    proc_macro_deps = all_crate_deps(proc_macro=True, proc_macro_dev=True),")
        if feat_flags:
            L.append("    rustc_flags = [%s]," % ", ".join(feat_flags))
        unit_env, unit_labels, unit_host = bin_exe_env(lib_src_files)
        emit_env_data(L, unit_env, unit_labels)
        if unit_host:
            L.append('    tags = ["manual"],')
        L.append(PUBLIC)
        L.append(")")
        L.append("")

    # integration tests
    for t in test_targets:
        rel = os.path.relpath(t["src_path"], mdir)
        tname = os.path.splitext(os.path.basename(rel))[0].replace("-", "_")
        extra = collect_mod_sources(rel, mdir)
        srcs = [rel] + extra
        L.append("rust_test(")
        L.append('    name = "%s",' % tname)
        L.append('    srcs = [%s],' % ", ".join('"%s"' % s for s in srcs))
        if os.path.splitext(os.path.basename(rel))[0] != tname:
            L.append('    crate_root = "%s",' % rel)
        L.append("    edition = crate_edition(),")
        L.append("    aliases = aliases(normal_dev=True, proc_macro_dev=True),")
        L.append("    deps = %s," % deps_block(test_graph=True, dev=True, extra_first_party=[":lib_test"]))
        L.append("    proc_macro_deps = all_crate_deps(proc_macro=True, proc_macro_dev=True),")
        if feat_flags:
            L.append("    rustc_flags = [%s]," % ", ".join(feat_flags))
        L.extend(compile_data_lines(srcs))
        integ_env, integ_labels, integ_host = bin_exe_env(srcs)
        emit_env_data(L, integ_env, integ_labels)
        if integ_host:
            L.append('    tags = ["manual"],')
        L.append(PUBLIC)
        L.append(")")
        L.append("")

    # bins (cargo build; default features, normal deps via :lib). A Cargo bin
    # implicitly depends on its package's lib (so `nmp_cli::Result` resolves);
    # rules_rust has no such implicit edge, so wire it explicitly when present.
    # Targets with `required-features` not in the default set are skipped (Cargo
    # skips them under `cargo build` / `cargo test --workspace`).
    own_lib = [":lib"] if lib_targets else None
    bin_active = prod_feats.get(name, set())
    for t in bin_targets:
        if not required_features_satisfied(t, bin_active):
            continue
        rel = os.path.relpath(t["src_path"], mdir)
        L.append("rust_binary(")
        L.append('    name = "%s",' % t["name"])
        L.append('    srcs = ["%s"],' % rel)
        if os.path.splitext(os.path.basename(rel))[0] != t["name"]:
            L.append('    crate_root = "%s",' % rel)
        L.append("    edition = crate_edition(),")
        L.append("    aliases = aliases(),")
        L.append("    deps = %s," % deps_block(test_graph=False, dev=False, extra_first_party=own_lib))
        L.append("    proc_macro_deps = all_crate_deps(proc_macro=True),")
        L.append(PUBLIC)
        L.append(")")
        L.append("")

    # examples (cargo test compiles them; dev-dep context -> test profile).
    # Active features = default ∪ unified test features; required-features not
    # in that set (e.g. bench-instrumentation) means Cargo skips the example, so
    # we skip emitting it too.
    example_active = test_feats.get(name, set())
    for t in example_targets:
        if not required_features_satisfied(t, example_active):
            continue
        rel = os.path.relpath(t["src_path"], mdir)
        ename = "example_" + os.path.splitext(os.path.basename(rel))[0].replace("-", "_")
        extra = collect_mod_sources(rel, mdir)
        srcs = [rel] + extra
        L.append("rust_binary(")
        L.append('    name = "%s",' % ename)
        L.append('    srcs = [%s],' % ", ".join('"%s"' % s for s in srcs))
        # The target name is `example_<stem>` (prefixed to dodge bin/test name
        # collisions), so rules_rust cannot infer the crate root by filename --
        # point it at the example's own entry file explicitly.
        L.append('    crate_root = "%s",' % rel)
        L.append("    edition = crate_edition(),")
        L.append("    aliases = aliases(normal_dev=True, proc_macro_dev=True),")
        L.append("    deps = %s," % deps_block(test_graph=True, dev=True, extra_first_party=[":lib_test"]))
        L.append("    proc_macro_deps = all_crate_deps(proc_macro=True, proc_macro_dev=True),")
        if feat_flags:
            L.append("    rustc_flags = [%s]," % ", ".join(feat_flags))
        L.extend(compile_data_lines(srcs))
        ex_env, ex_labels, _ = bin_exe_env(srcs)
        emit_env_data(L, ex_env, ex_labels, manifest=False, first_party=False)
        L.append(PUBLIC)
        L.append(")")
        L.append("")

    # Export this crate's full source tree as a filegroup. Tests read workspace
    # files at runtime via env!("CARGO_MANIFEST_DIR")-relative paths: the
    # source-scanning "grep-guard" falsifiers walk a crate's src/ (and sibling
    # crates' src/), and the nmp-cli catalog/contracts suite reads other crates'
    # Cargo.toml + native source trees. //:first_party_sources aggregates every
    # crate's :crate_files so any test can pull the whole workspace source tree
    # with one data label. `target/**` is excluded so a leftover Cargo build
    # output dir is never staged as runfiles.
    L.append("filegroup(")
    L.append('    name = "crate_files",')
    L.append('    srcs = glob(["**"], exclude = ["target/**", "target"]),')
    L.append("    visibility = [\"//visibility:public\"],")
    L.append(")")
    L.append("")

    return "\n".join(L).rstrip() + "\n"


def main():
    meta = run_cargo_metadata()
    root = os.path.abspath(meta["workspace_root"])
    pkgs = {p["id"]: p for p in meta["packages"]}
    members = [pkgs[m] for m in meta["workspace_members"]]
    members_by_name = {p["name"]: p for p in members}
    # Production-unified features (`cargo build --workspace`) and test-unified
    # features (`cargo test --workspace`, adds dev-edge activations).
    prod_feats, prod_opt = unified_features(members_by_name, include_dev=False)
    test_feats, test_opt = unified_features(members_by_name, include_dev=True)
    # bin name -> label, for CARGO_BIN_EXE_<name> wiring in tests/examples.
    bin_label_by_name = {}
    for p in members:
        for t in p.get("targets", []):
            if target_kind(t) == "bin":
                bin_label_by_name[t["name"]] = "//crates/%s:%s" % (p["name"], t["name"])
    written = []
    for p in members:
        if p["name"] in HAND_MAINTAINED:
            continue
        with open(os.path.join(os.path.dirname(p["manifest_path"]), "BUILD.bazel"), "w") as f:
            f.write(gen_for(p, prod_feats, prod_opt, test_feats, test_opt,
                            bin_label_by_name, root))
        written.append(p["name"])
    # Root BUILD: export Cargo.toml/Cargo.lock (crate_universe source of truth
    # + compile_data for proc-macros) and the workspace fixture included via
    # include_str! from nmp-test-support; plus the first_party_sources filegroup
    # that aggregates every crate's :src_files so source-scanning tests can pull
    # the whole workspace source tree into runfiles with one data label.
    crate_src_labels = sorted("//crates/%s:crate_files" % n for n in members_by_name)
    root_build = "\n".join([
        "# @generated by tools/bazel/gen_buildfiles.py -- do not edit by hand.",
        "# Root package. Exports the crate_universe source of truth and the",
        "# workspace fixture, and aggregates every first-party crate's source",
        "# tree into //:first_party_sources for source-scanning tests.",
        'exports_files(["Cargo.toml", "Cargo.lock"])',
        "",
        "# Workspace-root fixture included via include_str! from nmp-test-support",
        "# (crates/nmp-test-support/src/reference_fixtures.rs -> ../../../fixtures/).",
        'exports_files(["fixtures/reference-locators.json"])',
        "",
        "filegroup(",
        '    name = "first_party_sources",',
        "    srcs = [%s]," % ", ".join('"%s"' % l for l in crate_src_labels),
        '    visibility = ["//visibility:public"],',
        ")",
        "",
        "# Root non-crate test inputs staged as runfiles for tests that read",
        "# workspace-level data via env!(\"CARGO_MANIFEST_DIR\")-relative paths:",
        "# features/**/*.feature -> the cucumber acceptance suite (nmp:main);",
        "# native/** + Packages/** + Cargo.toml/Cargo.lock -> the nmp-cli",
        "# catalog/contracts suite (features.toml, the SDK source trees it",
        "# validates, and the source-checkout markers the CLI requires).",
        "# Resolved from the `_main/` runfiles CWD.",
        "filegroup(",
        '    name = "workspace_data",',
        "    srcs = glob([\"features/**/*.feature\", \"native/**\", \"Packages/**\"]) + [",
        '        "Cargo.toml",',
        '        "Cargo.lock",',
        "    ],",
        '    visibility = ["//visibility:public"],',
        ")",
        "",
    ])
    with open(os.path.join(root, "BUILD.bazel"), "w") as f:
        f.write(root_build)
    written.append("<root>")
    def clean(s):
        return sorted(x for x in s if x != "default")
    summary = {n: {"prod": clean(prod_feats[n]), "test": clean(test_feats[n]),
                   "opt": clean(test_opt[n])}
               for n in members_by_name
               if clean(prod_feats[n]) or clean(test_feats[n]) or test_opt[n]}
    print("Wrote %d BUILD.bazel files; feature-active crates: %s" % (len(written), summary or "none"))


if __name__ == "__main__":
    main()