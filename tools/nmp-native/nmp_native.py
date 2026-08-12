#!/usr/bin/env python3
"""Prepare one feature-selected NMP native artifact from an app manifest.

The implementation is deliberately ignorant of NMP feature family names.  A
checked catalog owns the mapping from stable app keys to Cargo forwarding
features and hand-written SDK sources.  Cargo metadata owns activation and
dependency resolution; this tool only projects the resolved activation into a
matching native artifact and SDK source set.
"""

from __future__ import annotations

import argparse
import dataclasses
import hashlib
import json
import os
import platform
import re
import shutil
import subprocess
import sys
import tempfile
import tomllib
from pathlib import Path
from typing import Any, Iterable, Mapping, Sequence


SCHEMA_VERSION = 1
GENERATED_MARKER = ".nmp-native-generated"
PROVENANCE_FILE = "nmp-native-provenance.json"
KEY_RE = re.compile(r"^[a-z][a-z0-9]*(?:-[a-z0-9]+)*$")
NAME_RE = re.compile(r"^[A-Za-z_][A-Za-z0-9_]*$")
PROFILE_RE = re.compile(r"^[a-z][a-z0-9-]*$")
IF_MARKER_RE = re.compile(
    r"^(?P<prefix>\s*(?://+|\*)\s*)nmp-native:if\s+"
    r"(?P<key>[a-z][a-z0-9]*(?:-[a-z0-9]+)*)(?P<suffix>\s*(?:\*/)?\s*)$"
)
ENDIF_MARKER_RE = re.compile(
    r"^(?P<prefix>\s*(?://+|\*)\s*)nmp-native:endif(?P<suffix>\s*(?:\*/)?\s*)$"
)


class NativePrepareError(RuntimeError):
    """A deterministic, user-actionable preparation refusal."""


@dataclasses.dataclass(frozen=True)
class SourceSpec:
    path: str
    destination: str
    target: str | None = None


@dataclasses.dataclass(frozen=True)
class FeatureSpec:
    key: str
    cargo_feature: str
    ffi_sources: tuple[str, ...]
    swift_sources: tuple[SourceSpec, ...]
    kotlin_sources: tuple[SourceSpec, ...]


@dataclasses.dataclass(frozen=True)
class SwiftTarget:
    name: str
    dependencies: tuple[str, ...]


@dataclasses.dataclass(frozen=True)
class AppleSlice:
    name: str
    targets: tuple[str, ...]


@dataclasses.dataclass(frozen=True)
class ArtifactSpec:
    ffi_package: str
    ffi_manifest: str
    library_stem: str
    bindgen_bin: str


@dataclasses.dataclass(frozen=True)
class AppleSpec:
    package_name: str
    xcframework_name: str
    binary_target: str
    ffi_target: str
    macos_deployment_target: str
    platforms: tuple[str, ...]
    linked_frameworks: tuple[str, ...]
    targets: tuple[SwiftTarget, ...]
    slices: tuple[AppleSlice, ...]


@dataclasses.dataclass(frozen=True)
class KotlinSpec:
    project_name: str
    group: str
    version: str
    jvm_toolchain: int
    dependencies: tuple[str, ...]


@dataclasses.dataclass(frozen=True)
class Catalog:
    path: Path
    artifact: ArtifactSpec
    apple: AppleSpec
    kotlin: KotlinSpec
    core_ffi_sources: tuple[str, ...]
    core_swift_sources: tuple[SourceSpec, ...]
    core_kotlin_sources: tuple[SourceSpec, ...]
    features: tuple[FeatureSpec, ...]

    @property
    def by_key(self) -> dict[str, FeatureSpec]:
        return {feature.key: feature for feature in self.features}

    @property
    def by_cargo_feature(self) -> dict[str, FeatureSpec]:
        return {feature.cargo_feature: feature for feature in self.features}


@dataclasses.dataclass(frozen=True)
class AppManifest:
    path: Path
    features: tuple[str, ...]


@dataclasses.dataclass(frozen=True)
class CargoResolution:
    metadata: Mapping[str, Any]
    active_ffi_features: tuple[str, ...]
    resolved_features: tuple[FeatureSpec, ...]
    packages: tuple[str, ...]


@dataclasses.dataclass(frozen=True)
class CommandResult:
    stdout: str = ""
    stderr: str = ""


class CommandRunner:
    """Subprocess seam kept small enough for deterministic fake-tool tests."""

    def __init__(self, *, verbose: bool = True) -> None:
        self.verbose = verbose

    def run(
        self,
        args: Sequence[str],
        *,
        cwd: Path,
        env: Mapping[str, str] | None = None,
        capture: bool = False,
    ) -> CommandResult:
        if self.verbose:
            print("+ " + " ".join(args), file=sys.stderr)
        merged_env = os.environ.copy()
        if env:
            merged_env.update(env)
        completed = subprocess.run(
            list(args),
            cwd=cwd,
            env=merged_env,
            check=True,
            text=True,
            # The CLI reserves stdout for its final machine-readable result.
            # Cargo/bindgen tools that print ordinary progress to stdout are
            # routed to stderr just like Cargo's own compilation progress.
            stdout=subprocess.PIPE if capture else sys.stderr,
            stderr=subprocess.PIPE if capture else None,
        )
        return CommandResult(completed.stdout or "", completed.stderr or "")


def _expect_table(value: Any, where: str) -> Mapping[str, Any]:
    if not isinstance(value, dict):
        raise NativePrepareError(f"{where} must be a TOML table")
    return value


def _expect_list(value: Any, where: str) -> list[Any]:
    if not isinstance(value, list):
        raise NativePrepareError(f"{where} must be a TOML array")
    return value


def _expect_string(value: Any, where: str) -> str:
    if not isinstance(value, str) or not value:
        raise NativePrepareError(f"{where} must be a nonempty string")
    return value


def _strict_keys(table: Mapping[str, Any], allowed: set[str], where: str) -> None:
    unknown = sorted(set(table) - allowed)
    if unknown:
        raise NativePrepareError(
            f"{where} contains unknown field(s): {', '.join(unknown)}"
        )


def _string_list(value: Any, where: str, *, unique: bool = True) -> tuple[str, ...]:
    items = tuple(
        _expect_string(item, f"{where}[{index}]")
        for index, item in enumerate(_expect_list(value, where))
    )
    if unique and len(items) != len(set(items)):
        raise NativePrepareError(f"{where} contains duplicates")
    return items


def _relative_path(value: Any, where: str) -> str:
    raw = _expect_string(value, where)
    candidate = Path(raw)
    if candidate.is_absolute() or ".." in candidate.parts or raw in {".", ".."}:
        raise NativePrepareError(f"{where} must be a safe relative path")
    return candidate.as_posix()


def _parse_source(value: Any, where: str, *, swift: bool) -> SourceSpec:
    table = _expect_table(value, where)
    allowed = {"path", "destination", "target"} if swift else {"path", "destination"}
    _strict_keys(table, allowed, where)
    source = SourceSpec(
        path=_relative_path(table.get("path"), f"{where}.path"),
        destination=_relative_path(
            table.get("destination"), f"{where}.destination"
        ),
        target=(
            _expect_string(table.get("target"), f"{where}.target") if swift else None
        ),
    )
    if swift and not NAME_RE.fullmatch(source.target or ""):
        raise NativePrepareError(f"{where}.target is not a valid Swift target name")
    return source


def _parse_sources(value: Any, where: str, *, swift: bool) -> tuple[SourceSpec, ...]:
    return tuple(
        _parse_source(item, f"{where}[{index}]", swift=swift)
        for index, item in enumerate(_expect_list(value, where))
    )


def load_manifest(path: Path) -> AppManifest:
    try:
        data = tomllib.loads(path.read_text(encoding="utf-8"))
    except (OSError, tomllib.TOMLDecodeError) as error:
        raise NativePrepareError(f"cannot read app manifest {path}: {error}") from error
    _strict_keys(data, {"schema", "features"}, f"app manifest {path}")
    if data.get("schema") != SCHEMA_VERSION:
        raise NativePrepareError(
            f"app manifest {path} has unsupported schema {data.get('schema')!r}; "
            f"expected {SCHEMA_VERSION}"
        )
    features = _string_list(data.get("features"), f"app manifest {path}.features")
    for key in features:
        if not KEY_RE.fullmatch(key):
            raise NativePrepareError(f"invalid app feature key {key!r}")
    return AppManifest(path.resolve(), tuple(sorted(features)))


def load_catalog(path: Path, repo_root: Path) -> Catalog:
    try:
        data = tomllib.loads(path.read_text(encoding="utf-8"))
    except (OSError, tomllib.TOMLDecodeError) as error:
        raise NativePrepareError(f"cannot read native catalog {path}: {error}") from error
    _strict_keys(
        data,
        {"schema", "artifact", "apple", "kotlin", "core", "features"},
        f"native catalog {path}",
    )
    if data.get("schema") != SCHEMA_VERSION:
        raise NativePrepareError(
            f"native catalog {path} has unsupported schema {data.get('schema')!r}; "
            f"expected {SCHEMA_VERSION}"
        )

    artifact_data = _expect_table(data.get("artifact"), "catalog.artifact")
    _strict_keys(
        artifact_data,
        {"ffi_package", "ffi_manifest", "library_stem", "bindgen_bin"},
        "catalog.artifact",
    )
    artifact = ArtifactSpec(
        ffi_package=_expect_string(
            artifact_data.get("ffi_package"), "catalog.artifact.ffi_package"
        ),
        ffi_manifest=_relative_path(
            artifact_data.get("ffi_manifest"), "catalog.artifact.ffi_manifest"
        ),
        library_stem=_expect_string(
            artifact_data.get("library_stem"), "catalog.artifact.library_stem"
        ),
        bindgen_bin=_expect_string(
            artifact_data.get("bindgen_bin"), "catalog.artifact.bindgen_bin"
        ),
    )
    for value, where in (
        (artifact.ffi_package.replace("-", "_"), "artifact.ffi_package"),
        (artifact.library_stem, "artifact.library_stem"),
        (artifact.bindgen_bin.replace("-", "_"), "artifact.bindgen_bin"),
    ):
        if not NAME_RE.fullmatch(value):
            raise NativePrepareError(f"catalog.{where} has invalid characters")

    apple_data = _expect_table(data.get("apple"), "catalog.apple")
    _strict_keys(
        apple_data,
        {
            "package_name",
            "xcframework_name",
            "binary_target",
            "ffi_target",
            "macos_deployment_target",
            "platforms",
            "linked_frameworks",
            "targets",
            "slices",
        },
        "catalog.apple",
    )
    swift_targets: list[SwiftTarget] = []
    for index, raw in enumerate(_expect_list(apple_data.get("targets"), "catalog.apple.targets")):
        where = f"catalog.apple.targets[{index}]"
        table = _expect_table(raw, where)
        _strict_keys(table, {"name", "dependencies"}, where)
        name = _expect_string(table.get("name"), f"{where}.name")
        if not NAME_RE.fullmatch(name):
            raise NativePrepareError(f"{where}.name is not a valid Swift target name")
        swift_targets.append(
            SwiftTarget(
                name,
                _string_list(table.get("dependencies"), f"{where}.dependencies"),
            )
        )
    slices: list[AppleSlice] = []
    for index, raw in enumerate(_expect_list(apple_data.get("slices"), "catalog.apple.slices")):
        where = f"catalog.apple.slices[{index}]"
        table = _expect_table(raw, where)
        _strict_keys(table, {"name", "targets"}, where)
        targets = _string_list(table.get("targets"), f"{where}.targets")
        if not targets:
            raise NativePrepareError(f"{where}.targets must not be empty")
        slices.append(
            AppleSlice(_expect_string(table.get("name"), f"{where}.name"), targets)
        )
    apple = AppleSpec(
        package_name=_expect_string(
            apple_data.get("package_name"), "catalog.apple.package_name"
        ),
        xcframework_name=_expect_string(
            apple_data.get("xcframework_name"), "catalog.apple.xcframework_name"
        ),
        binary_target=_expect_string(
            apple_data.get("binary_target"), "catalog.apple.binary_target"
        ),
        ffi_target=_expect_string(
            apple_data.get("ffi_target"), "catalog.apple.ffi_target"
        ),
        macos_deployment_target=_expect_string(
            apple_data.get("macos_deployment_target"),
            "catalog.apple.macos_deployment_target",
        ),
        platforms=_string_list(
            apple_data.get("platforms"), "catalog.apple.platforms"
        ),
        linked_frameworks=_string_list(
            apple_data.get("linked_frameworks"),
            "catalog.apple.linked_frameworks",
        ),
        targets=tuple(swift_targets),
        slices=tuple(slices),
    )
    for value, where in (
        (apple.package_name, "package_name"),
        (apple.binary_target, "binary_target"),
        (apple.ffi_target, "ffi_target"),
    ):
        if not NAME_RE.fullmatch(value):
            raise NativePrepareError(f"catalog.apple.{where} is not a valid name")
    if not apple.xcframework_name.endswith(".xcframework"):
        raise NativePrepareError("catalog.apple.xcframework_name must end in .xcframework")

    kotlin_data = _expect_table(data.get("kotlin"), "catalog.kotlin")
    _strict_keys(
        kotlin_data,
        {"project_name", "group", "version", "jvm_toolchain", "dependencies"},
        "catalog.kotlin",
    )
    jvm_toolchain = kotlin_data.get("jvm_toolchain")
    if not isinstance(jvm_toolchain, int) or jvm_toolchain < 8:
        raise NativePrepareError("catalog.kotlin.jvm_toolchain must be an integer >= 8")
    kotlin = KotlinSpec(
        project_name=_expect_string(
            kotlin_data.get("project_name"), "catalog.kotlin.project_name"
        ),
        group=_expect_string(kotlin_data.get("group"), "catalog.kotlin.group"),
        version=_expect_string(
            kotlin_data.get("version"), "catalog.kotlin.version"
        ),
        jvm_toolchain=jvm_toolchain,
        dependencies=_string_list(
            kotlin_data.get("dependencies"), "catalog.kotlin.dependencies"
        ),
    )

    core_data = _expect_table(data.get("core"), "catalog.core")
    _strict_keys(
        core_data, {"ffi_sources", "swift_sources", "kotlin_sources"}, "catalog.core"
    )
    core_ffi_sources = tuple(
        _relative_path(item, f"catalog.core.ffi_sources[{index}]")
        for index, item in enumerate(
            _expect_list(core_data.get("ffi_sources"), "catalog.core.ffi_sources")
        )
    )
    core_swift_sources = _parse_sources(
        core_data.get("swift_sources"), "catalog.core.swift_sources", swift=True
    )
    core_kotlin_sources = _parse_sources(
        core_data.get("kotlin_sources"), "catalog.core.kotlin_sources", swift=False
    )

    features: list[FeatureSpec] = []
    for index, raw in enumerate(_expect_list(data.get("features"), "catalog.features")):
        where = f"catalog.features[{index}]"
        table = _expect_table(raw, where)
        _strict_keys(
            table,
            {"key", "cargo_feature", "ffi_sources", "swift_sources", "kotlin_sources"},
            where,
        )
        key = _expect_string(table.get("key"), f"{where}.key")
        cargo_feature = _expect_string(
            table.get("cargo_feature"), f"{where}.cargo_feature"
        )
        if not KEY_RE.fullmatch(key) or not KEY_RE.fullmatch(cargo_feature):
            raise NativePrepareError(f"{where} has an invalid key or Cargo feature")
        ffi_sources = tuple(
            _relative_path(item, f"{where}.ffi_sources[{source_index}]")
            for source_index, item in enumerate(
                _expect_list(table.get("ffi_sources"), f"{where}.ffi_sources")
            )
        )
        if not ffi_sources:
            raise NativePrepareError(f"{where}.ffi_sources must not be empty")
        features.append(
            FeatureSpec(
                key=key,
                cargo_feature=cargo_feature,
                ffi_sources=ffi_sources,
                swift_sources=_parse_sources(
                    table.get("swift_sources"), f"{where}.swift_sources", swift=True
                ),
                kotlin_sources=_parse_sources(
                    table.get("kotlin_sources"), f"{where}.kotlin_sources", swift=False
                ),
            )
        )

    keys = [feature.key for feature in features]
    cargo_features = [feature.cargo_feature for feature in features]
    if keys != sorted(keys):
        raise NativePrepareError("catalog.features must be in canonical key order")
    if len(keys) != len(set(keys)):
        raise NativePrepareError("catalog.features contains duplicate keys")
    if len(cargo_features) != len(set(cargo_features)):
        raise NativePrepareError("catalog.features contains duplicate Cargo features")

    catalog = Catalog(
        path=path.resolve(),
        artifact=artifact,
        apple=apple,
        kotlin=kotlin,
        core_ffi_sources=core_ffi_sources,
        core_swift_sources=core_swift_sources,
        core_kotlin_sources=core_kotlin_sources,
        features=tuple(features),
    )
    _validate_catalog_paths(catalog, repo_root.resolve())
    return catalog


def _resolve_repo_path(repo_root: Path, relative: str, where: str) -> Path:
    resolved = (repo_root / relative).resolve()
    try:
        resolved.relative_to(repo_root.resolve())
    except ValueError as error:
        raise NativePrepareError(f"{where} escapes the repository: {relative}") from error
    return resolved


def _validate_catalog_paths(catalog: Catalog, repo_root: Path) -> None:
    paths: list[tuple[str, str, bool]] = [
        (catalog.artifact.ffi_manifest, "catalog.artifact.ffi_manifest", True)
    ]
    paths.extend(
        (path, "catalog.core.ffi_sources", True)
        for path in catalog.core_ffi_sources
    )
    paths.extend(
        (source.path, "catalog.core.swift_sources", True)
        for source in catalog.core_swift_sources
    )
    paths.extend(
        (source.path, "catalog.core.kotlin_sources", True)
        for source in catalog.core_kotlin_sources
    )
    for feature in catalog.features:
        paths.extend(
            (path, f"catalog feature {feature.key}.ffi_sources", False)
            for path in feature.ffi_sources
        )
        paths.extend(
            (source.path, f"catalog feature {feature.key}.swift_sources", False)
            for source in feature.swift_sources
        )
        paths.extend(
            (source.path, f"catalog feature {feature.key}.kotlin_sources", False)
            for source in feature.kotlin_sources
        )
    for relative, where, required_for_every_selection in paths:
        resolved = _resolve_repo_path(repo_root, relative, where)
        if required_for_every_selection and not resolved.is_file():
            raise NativePrepareError(f"{where} does not exist as a file: {relative}")

    target_names = {target.name for target in catalog.apple.targets}
    reserved_names = {catalog.apple.binary_target, catalog.apple.ffi_target}
    if target_names & reserved_names:
        raise NativePrepareError("catalog.apple target names collide with generated targets")
    if len(target_names) != len(catalog.apple.targets):
        raise NativePrepareError("catalog.apple.targets contains duplicate names")
    for target in catalog.apple.targets:
        unknown = sorted(set(target.dependencies) - target_names - reserved_names)
        if unknown:
            raise NativePrepareError(
                f"Swift target {target.name} has unknown dependencies: {', '.join(unknown)}"
            )
    all_swift_sources = list(catalog.core_swift_sources)
    for feature in catalog.features:
        all_swift_sources.extend(feature.swift_sources)
    for source in all_swift_sources:
        if source.target not in target_names:
            raise NativePrepareError(
                f"Swift source {source.path} names unknown target {source.target}"
            )

    all_slice_targets = [target for item in catalog.apple.slices for target in item.targets]
    if len(all_slice_targets) != len(set(all_slice_targets)):
        raise NativePrepareError("catalog.apple.slices assigns one Rust target more than once")


def filter_source(text: str, selected: set[str], known: set[str], source: str) -> str:
    """Remove generic conditional blocks whose catalog keys are not active."""

    active_stack: list[tuple[str, bool]] = []
    output: list[str] = []
    for line_number, line in enumerate(text.splitlines(keepends=True), 1):
        stripped_line = line.rstrip("\r\n")
        line_ending = line[len(stripped_line) :]
        if_match = IF_MARKER_RE.match(stripped_line)
        if if_match:
            key = if_match.group("key")
            if key not in known:
                raise NativePrepareError(
                    f"{source}:{line_number}: conditional marker names unknown feature {key!r}"
                )
            active_stack.append((key, key in selected))
            scaffold = _marker_scaffold(if_match, line_ending)
            if scaffold and all(enabled for _, enabled in active_stack[:-1]):
                output.append(scaffold)
            continue
        endif_match = ENDIF_MARKER_RE.match(stripped_line)
        if endif_match:
            if not active_stack:
                raise NativePrepareError(
                    f"{source}:{line_number}: nmp-native:endif has no matching if"
                )
            parent_enabled = all(enabled for _, enabled in active_stack[:-1])
            active_stack.pop()
            scaffold = _marker_scaffold(endif_match, line_ending)
            if scaffold and parent_enabled:
                output.append(scaffold)
            continue
        if all(enabled for _, enabled in active_stack):
            output.append(line)
    if active_stack:
        keys = ", ".join(key for key, _ in active_stack)
        raise NativePrepareError(f"{source}: unterminated conditional block(s): {keys}")
    return "".join(output)


def _marker_scaffold(match: re.Match[str], line_ending: str) -> str:
    """Keep a block-comment terminator that shares a marker line."""

    suffix = match.group("suffix")
    if "*/" not in suffix:
        return ""
    return f"{match.group('prefix').rstrip()} */{line_ending}"


class NativePreparer:
    def __init__(
        self,
        *,
        repo_root: Path,
        catalog: Catalog,
        runner: CommandRunner,
        cache_dir: Path,
        system: str | None = None,
        machine: str | None = None,
    ) -> None:
        self.repo_root = repo_root.resolve()
        self.catalog = catalog
        self.runner = runner
        self.cache_dir = cache_dir.resolve()
        self.system = system or platform.system()
        self.machine = machine or platform.machine()

    def resolve(self, manifest: AppManifest) -> CargoResolution:
        unknown = sorted(set(manifest.features) - set(self.catalog.by_key))
        if unknown:
            raise NativePrepareError(
                "app manifest selects unknown or internal-only feature(s): "
                + ", ".join(unknown)
            )
        requested_cargo_features = [
            self.catalog.by_key[key].cargo_feature for key in manifest.features
        ]
        # Workspace metadata unifies every member's development graph, including
        # other packages' all-feature development dependencies. Asking metadata
        # at the workspace root can therefore falsely turn a core-only app into
        # an all-features app. A generated, standalone consumer manifest asks
        # Cargo the real production-dependency question and leaves the
        # repository/app manifests untouched.
        with tempfile.TemporaryDirectory(prefix="nmp-native-resolver-") as temporary:
            resolver_root = Path(temporary)
            dependency_path = (
                self.repo_root / self.catalog.artifact.ffi_manifest
            ).resolve().parent
            feature_list = ", ".join(
                json.dumps(feature) for feature in requested_cargo_features
            )
            resolver_manifest = resolver_root / "Cargo.toml"
            resolver_manifest.write_text(
                "\n".join(
                    [
                        "[package]",
                        'name = "nmp-native-resolver"',
                        'version = "0.0.0"',
                        'edition = "2021"',
                        "publish = false",
                        "",
                        "[workspace]",
                        "",
                        "[dependencies.selected-nmp-ffi]",
                        f'package = {json.dumps(self.catalog.artifact.ffi_package)}',
                        f'path = {json.dumps(str(dependency_path))}',
                        "default-features = false",
                        f"features = [{feature_list}]",
                        "",
                    ]
                ),
                encoding="utf-8",
            )
            resolver_source = resolver_root / "src"
            resolver_source.mkdir()
            (resolver_source / "lib.rs").write_text("", encoding="utf-8")
            lockfile = self.repo_root / "Cargo.lock"
            if lockfile.is_file():
                shutil.copy2(lockfile, resolver_root / "Cargo.lock")
            args = [
                "cargo",
                "metadata",
                "--format-version",
                "1",
                "--manifest-path",
                str(resolver_manifest),
                "--no-default-features",
            ]
            # Reconcile the copied workspace lock to the generated root
            # package, then prove the result is stable under --locked. The
            # reconciliation may remove packages unreachable from this exact
            # selection; it may not select a registry/git package absent from
            # the repository lock.
            self.runner.run(args, cwd=resolver_root, capture=True)
            self._verify_resolver_lock(resolver_root / "Cargo.lock", lockfile)
            result = self.runner.run(
                [*args[:4], "--locked", *args[4:]],
                cwd=resolver_root,
                capture=True,
            )
        try:
            metadata = json.loads(result.stdout)
        except json.JSONDecodeError as error:
            raise NativePrepareError(f"cargo metadata returned invalid JSON: {error}") from error

        expected_manifest = (self.repo_root / self.catalog.artifact.ffi_manifest).resolve()
        candidates = [
            package
            for package in metadata.get("packages", [])
            if package.get("name") == self.catalog.artifact.ffi_package
            and Path(package.get("manifest_path", "")).resolve() == expected_manifest
        ]
        if len(candidates) != 1:
            raise NativePrepareError(
                f"cargo metadata did not resolve exactly one {self.catalog.artifact.ffi_package} "
                f"at {self.catalog.artifact.ffi_manifest}"
            )
        package_id = candidates[0]["id"]
        resolve = metadata.get("resolve") or {}
        nodes = [node for node in resolve.get("nodes", []) if node.get("id") == package_id]
        if len(nodes) != 1:
            raise NativePrepareError("cargo metadata lacks the nmp-ffi resolve node")
        active = tuple(sorted(set(nodes[0].get("features", []))))
        unregistered = sorted(
            set(active) - set(self.catalog.by_cargo_feature) - {"default"}
        )
        if unregistered:
            raise NativePrepareError(
                "Cargo activated app-facing nmp-ffi feature(s) with no catalog metadata: "
                + ", ".join(unregistered)
            )
        resolved_features = tuple(
            feature for feature in self.catalog.features if feature.cargo_feature in active
        )
        resolved_keys = {feature.key for feature in resolved_features}
        missing = sorted(set(manifest.features) - resolved_keys)
        if missing:
            raise NativePrepareError(
                "Cargo did not activate requested forwarding feature(s): " + ", ".join(missing)
            )
        resolved_ids = {node.get("id") for node in resolve.get("nodes", [])}
        packages = tuple(
            sorted(
                f"{package.get('name')}@{package.get('version')}"
                for package in metadata.get("packages", [])
                if package.get("id") in resolved_ids
            )
        )
        return CargoResolution(metadata, active, resolved_features, packages)

    @staticmethod
    def _verify_resolver_lock(generated: Path, repository: Path) -> None:
        if not generated.is_file() or not repository.is_file():
            raise NativePrepareError("Cargo did not produce a resolver lockfile")
        try:
            generated_data = tomllib.loads(generated.read_text(encoding="utf-8"))
            repository_data = tomllib.loads(repository.read_text(encoding="utf-8"))
        except (OSError, tomllib.TOMLDecodeError) as error:
            raise NativePrepareError(f"cannot validate resolver lockfile: {error}") from error

        def external_packages(data: Mapping[str, Any]) -> set[tuple[str, str, str, str]]:
            packages: set[tuple[str, str, str, str]] = set()
            for package in data.get("package", []):
                source = package.get("source")
                if not source:
                    continue
                packages.add(
                    (
                        str(package.get("name", "")),
                        str(package.get("version", "")),
                        str(source),
                        str(package.get("checksum", "")),
                    )
                )
            return packages

        unexpected = sorted(
            external_packages(generated_data) - external_packages(repository_data)
        )
        if unexpected:
            rendered = ", ".join(f"{name}@{version}" for name, version, _, _ in unexpected)
            raise NativePrepareError(
                "generated Cargo resolver selected packages outside Cargo.lock: " + rendered
            )

    def prepare(
        self,
        *,
        manifest: AppManifest,
        output: Path,
        platforms: tuple[str, ...],
        profile: str,
        apple_targets: tuple[str, ...] = (),
    ) -> Mapping[str, Any]:
        if not platforms:
            raise NativePrepareError("at least one --platform is required")
        if len(platforms) != len(set(platforms)):
            raise NativePrepareError("--platform contains duplicates")
        unknown_platforms = sorted(set(platforms) - {"apple", "kotlin-jvm"})
        if unknown_platforms:
            raise NativePrepareError(
                "unsupported native platform(s): " + ", ".join(unknown_platforms)
            )
        if not PROFILE_RE.fullmatch(profile):
            raise NativePrepareError(f"invalid Cargo profile {profile!r}")
        if apple_targets and "apple" not in platforms:
            raise NativePrepareError("--apple-target requires --platform apple")

        resolution = self.resolve(manifest)
        self._validate_selected_feature_sources(resolution)
        effective_apple_targets = self._effective_apple_targets(apple_targets)
        if "apple" not in platforms:
            effective_apple_targets = ()
        host_target = self._host_target() if "kotlin-jvm" in platforms else None
        identity_inputs = self._identity_inputs(
            manifest=manifest,
            resolution=resolution,
            platforms=tuple(sorted(platforms)),
            profile=profile,
            apple_targets=effective_apple_targets,
            host_target=host_target,
        )
        identity = hashlib.sha256(_canonical_json(identity_inputs)).hexdigest()
        cache_entry = self.cache_dir / identity
        cache_output = cache_entry / "output"
        cache_hit = cache_output.is_dir()
        if cache_hit:
            self._verify_cached_output(cache_output, identity)
        else:
            self.cache_dir.mkdir(parents=True, exist_ok=True)
            staging = Path(tempfile.mkdtemp(prefix=".prepare-", dir=self.cache_dir))
            try:
                build_output = staging / "output"
                build_output.mkdir()
                selected_keys = {feature.key for feature in resolution.resolved_features}
                if "apple" in platforms:
                    self._build_apple(
                        output=build_output / "apple",
                        build_root=staging / "build-apple",
                        resolution=resolution,
                        selected_keys=selected_keys,
                        profile=profile,
                        targets=effective_apple_targets,
                    )
                if "kotlin-jvm" in platforms:
                    assert host_target is not None
                    self._build_kotlin(
                        output=build_output / "kotlin-jvm",
                        build_root=staging / "build-kotlin",
                        resolution=resolution,
                        selected_keys=selected_keys,
                        profile=profile,
                        host_target=host_target,
                    )
                contents = _content_inventory(build_output)
                provenance = {
                    "schema": SCHEMA_VERSION,
                    "identity": identity,
                    "identity_inputs": identity_inputs,
                    "contents": contents,
                }
                (build_output / PROVENANCE_FILE).write_bytes(
                    json.dumps(provenance, sort_keys=True, indent=2).encode("utf-8") + b"\n"
                )
                (build_output / GENERATED_MARKER).write_text(identity + "\n", encoding="utf-8")
                for transient in (staging / "build-apple", staging / "build-kotlin"):
                    if transient.exists():
                        shutil.rmtree(transient)
                try:
                    os.replace(staging, cache_entry)
                except OSError:
                    if cache_output.is_dir():
                        shutil.rmtree(staging)
                        self._verify_cached_output(cache_output, identity)
                    else:
                        raise
            except BaseException:
                if staging.exists():
                    shutil.rmtree(staging)
                raise

        self._materialize(cache_output, output.resolve())
        provenance = json.loads((cache_output / PROVENANCE_FILE).read_text(encoding="utf-8"))
        return {
            "identity": identity,
            "cache_hit": cache_hit,
            "cache_entry": str(cache_entry),
            "output": str(output.resolve()),
            "requested_features": list(manifest.features),
            "resolved_features": [feature.key for feature in resolution.resolved_features],
            "contents": provenance["contents"],
        }

    def _validate_selected_feature_sources(self, resolution: CargoResolution) -> None:
        for feature in resolution.resolved_features:
            sources = list(feature.ffi_sources)
            sources.extend(source.path for source in feature.swift_sources)
            sources.extend(source.path for source in feature.kotlin_sources)
            for relative in sources:
                resolved = _resolve_repo_path(
                    self.repo_root,
                    relative,
                    f"selected feature {feature.key} source",
                )
                if not resolved.is_file():
                    raise NativePrepareError(
                        f"selected feature {feature.key} source does not exist as a file: "
                        f"{relative}"
                    )

    def _effective_apple_targets(self, requested: tuple[str, ...]) -> tuple[str, ...]:
        available = tuple(
            target for apple_slice in self.catalog.apple.slices for target in apple_slice.targets
        )
        if not requested:
            return available
        if len(requested) != len(set(requested)):
            raise NativePrepareError("--apple-target contains duplicates")
        unknown = sorted(set(requested) - set(available))
        if unknown:
            raise NativePrepareError(
                "unknown Apple target(s) for this catalog: " + ", ".join(unknown)
            )
        return tuple(target for target in available if target in set(requested))

    def _host_target(self) -> str:
        result = self.runner.run(["rustc", "-vV"], cwd=self.repo_root, capture=True)
        for line in result.stdout.splitlines():
            if line.startswith("host: "):
                return line.removeprefix("host: ").strip()
        raise NativePrepareError("rustc -vV did not report a host target")

    def _identity_inputs(
        self,
        *,
        manifest: AppManifest,
        resolution: CargoResolution,
        platforms: tuple[str, ...],
        profile: str,
        apple_targets: tuple[str, ...],
        host_target: str | None,
    ) -> Mapping[str, Any]:
        toolchains: dict[str, str] = {
            "cargo": self.runner.run(["cargo", "-V"], cwd=self.repo_root, capture=True).stdout.strip(),
            "rustc": self.runner.run(["rustc", "-vV"], cwd=self.repo_root, capture=True).stdout.strip(),
            "python": sys.version,
        }
        if "apple" in platforms:
            toolchains["xcodebuild"] = self.runner.run(
                ["xcodebuild", "-version"], cwd=self.repo_root, capture=True
            ).stdout.strip()
        try:
            revision = self.runner.run(
                ["git", "rev-parse", "HEAD"], cwd=self.repo_root, capture=True
            ).stdout.strip()
        except (subprocess.CalledProcessError, NativePrepareError):
            revision = "unversioned"
        source_digest = self._source_digest(resolution)
        fixed_environment_keys = {
                "RUSTFLAGS",
                "CARGO_ENCODED_RUSTFLAGS",
                "RUSTC_WRAPPER",
                "RUSTC_WORKSPACE_WRAPPER",
                "CFLAGS",
                "CXXFLAGS",
                "LDFLAGS",
                "AR",
                "RANLIB",
                "CPATH",
                "LIBRARY_PATH",
                "MACOSX_DEPLOYMENT_TARGET",
                "IPHONEOS_DEPLOYMENT_TARGET",
                "SDKROOT",
                "CC",
                "CXX",
                "SOURCE_DATE_EPOCH",
        }
        dynamic_environment_keys = {
            key
            for key in os.environ
            if key.startswith("CARGO_TARGET_") or key.startswith("CARGO_PROFILE_")
        }
        environment = {
            key: os.environ.get(key, "")
            for key in sorted(fixed_environment_keys | dynamic_environment_keys)
        }
        return {
            "schema": SCHEMA_VERSION,
            "manifest_schema": SCHEMA_VERSION,
            "catalog_sha256": _sha256_file(self.catalog.path),
            "source_revision": revision,
            "source_sha256": source_digest,
            "toolchains": toolchains,
            "host": {"system": self.system, "machine": self.machine},
            "environment": environment,
            "ffi_package": self.catalog.artifact.ffi_package,
            "requested_features": list(manifest.features),
            "resolved_features": [feature.key for feature in resolution.resolved_features],
            "active_ffi_features": list(resolution.active_ffi_features),
            "resolved_packages": list(resolution.packages),
            "platforms": list(platforms),
            "profile": profile,
            "apple_targets": list(apple_targets),
            "host_target": host_target,
        }

    def _source_digest(self, resolution: CargoResolution) -> str:
        paths: set[Path] = set()
        for relative in (
            "Cargo.toml",
            "Cargo.lock",
            "rust-toolchain.toml",
            ".cargo/config.toml",
        ):
            candidate = self.repo_root / relative
            if candidate.is_file():
                paths.add(candidate.resolve())
        paths.add(self.catalog.path)
        paths.add(Path(__file__).resolve())

        resolved_ids = {
            node.get("id")
            for node in (resolution.metadata.get("resolve") or {}).get("nodes", [])
        }
        for package in resolution.metadata.get("packages", []):
            if package.get("id") not in resolved_ids:
                continue
            manifest_path = Path(package.get("manifest_path", "")).resolve()
            try:
                package_root = manifest_path.parent.relative_to(self.repo_root)
            except ValueError:
                continue
            absolute_root = self.repo_root / package_root
            for candidate in absolute_root.rglob("*"):
                if not candidate.is_file() or "target" in candidate.relative_to(absolute_root).parts:
                    continue
                paths.add(candidate.resolve())

        selected_sources: list[str] = list(self.catalog.core_ffi_sources)
        selected_sources.extend(source.path for source in self.catalog.core_swift_sources)
        selected_sources.extend(source.path for source in self.catalog.core_kotlin_sources)
        for feature in resolution.resolved_features:
            selected_sources.extend(feature.ffi_sources)
            selected_sources.extend(source.path for source in feature.swift_sources)
            selected_sources.extend(source.path for source in feature.kotlin_sources)
        for relative in selected_sources:
            paths.add(_resolve_repo_path(self.repo_root, relative, "catalog source"))

        digest = hashlib.sha256()
        for path in sorted(paths, key=lambda item: item.as_posix()):
            try:
                relative = path.relative_to(self.repo_root).as_posix()
            except ValueError:
                relative = path.as_posix()
            digest.update(relative.encode("utf-8"))
            digest.update(b"\0")
            digest.update(path.read_bytes())
            digest.update(b"\0")
        return digest.hexdigest()

    def _cargo_feature_args(self, resolution: CargoResolution) -> list[str]:
        features = [feature.cargo_feature for feature in resolution.resolved_features]
        return ["--features", ",".join(features)] if features else []

    @staticmethod
    def _profile_args(profile: str) -> list[str]:
        if profile == "dev":
            return []
        return ["--profile", profile]

    @staticmethod
    def _profile_dir(profile: str) -> str:
        return "debug" if profile == "dev" else profile

    def _cargo_env(self) -> dict[str, str]:
        target_dir = self.cache_dir / "cargo-target"
        target_dir.mkdir(parents=True, exist_ok=True)
        return {"CARGO_TARGET_DIR": str(target_dir)}

    def _build_apple(
        self,
        *,
        output: Path,
        build_root: Path,
        resolution: CargoResolution,
        selected_keys: set[str],
        profile: str,
        targets: tuple[str, ...],
    ) -> None:
        output.mkdir(parents=True)
        build_root.mkdir(parents=True)
        installed = set(
            self.runner.run(
                ["rustup", "target", "list", "--installed"],
                cwd=self.repo_root,
                capture=True,
            ).stdout.splitlines()
        )
        missing = [target for target in targets if target not in installed]
        if missing:
            self.runner.run(
                ["rustup", "target", "add", *missing], cwd=self.repo_root
            )
        cargo_env = self._cargo_env()
        target_dir = Path(cargo_env["CARGO_TARGET_DIR"])
        profile_dir = self._profile_dir(profile)
        library_name = f"lib{self.catalog.artifact.library_stem}.a"
        built: dict[str, Path] = {}
        host_target = self._host_target()
        for target in targets:
            args = [
                "cargo",
                "build",
                "--locked",
                "--package",
                self.catalog.artifact.ffi_package,
                "--no-default-features",
                *self._cargo_feature_args(resolution),
                *self._profile_args(profile),
                "--target",
                target,
                "--lib",
            ]
            if target == host_target:
                args.extend(["--bin", self.catalog.artifact.bindgen_bin])
            target_env = self._target_cargo_env(cargo_env, target)
            self.runner.run(args, cwd=self.repo_root, env=target_env)
            library = target_dir / target / profile_dir / library_name
            if not library.is_file():
                raise NativePrepareError(f"Cargo did not produce expected Apple library {library}")
            built[target] = library

        slice_libraries: list[Path] = []
        for apple_slice in self.catalog.apple.slices:
            selected_targets = [target for target in apple_slice.targets if target in built]
            if not selected_targets:
                continue
            if len(selected_targets) == 1:
                slice_libraries.append(built[selected_targets[0]])
                continue
            slice_dir = build_root / "slices" / apple_slice.name
            slice_dir.mkdir(parents=True)
            merged = slice_dir / library_name
            self.runner.run(
                ["lipo", "-create", *[str(built[target]) for target in selected_targets], "-output", str(merged)],
                cwd=self.repo_root,
            )
            if not merged.is_file():
                raise NativePrepareError(f"lipo did not produce expected slice {merged}")
            slice_libraries.append(merged)
        if not slice_libraries:
            raise NativePrepareError("the selected Apple targets produced no catalog slice")

        generated = build_root / "generated"
        generated.mkdir()
        if host_target in targets:
            bindgen = (
                target_dir
                / host_target
                / profile_dir
                / self.catalog.artifact.bindgen_bin
            )
        else:
            bindgen_args = [
                "cargo",
                "build",
                "--locked",
                "--package",
                self.catalog.artifact.ffi_package,
                "--bin",
                self.catalog.artifact.bindgen_bin,
                "--no-default-features",
                *self._cargo_feature_args(resolution),
                *self._profile_args(profile),
            ]
            self.runner.run(bindgen_args, cwd=self.repo_root, env=cargo_env)
            bindgen = target_dir / profile_dir / self.catalog.artifact.bindgen_bin
        if not bindgen.is_file():
            raise NativePrepareError(f"Cargo did not produce expected bindgen tool {bindgen}")
        bindgen_args = [
            str(bindgen),
            "generate",
            "--library",
            str(next(iter(built.values()))),
            "--language",
            "swift",
            "--out-dir",
            str(generated),
            "--no-format",
        ]
        self.runner.run(bindgen_args, cwd=self.repo_root, env=cargo_env)
        stem = self.catalog.artifact.library_stem
        generated_swift = generated / f"{stem}.swift"
        generated_header = generated / f"{stem}FFI.h"
        generated_modulemap = generated / f"{stem}FFI.modulemap"
        for expected in (generated_swift, generated_header, generated_modulemap):
            if not expected.is_file():
                raise NativePrepareError(f"UniFFI did not produce expected file {expected}")
        headers = build_root / "headers"
        headers.mkdir()
        shutil.copy2(generated_header, headers / generated_header.name)
        shutil.copy2(generated_modulemap, headers / "module.modulemap")

        xcframework = output / self.catalog.apple.xcframework_name
        xcode_args = ["xcodebuild", "-create-xcframework"]
        for library in slice_libraries:
            xcode_args.extend(["-library", str(library), "-headers", str(headers)])
        xcode_args.extend(["-output", str(xcframework)])
        self.runner.run(xcode_args, cwd=self.repo_root)
        if not xcframework.is_dir():
            raise NativePrepareError(f"xcodebuild did not produce expected {xcframework}")

        ffi_sources = output / "Sources" / self.catalog.apple.ffi_target
        ffi_sources.mkdir(parents=True)
        shutil.copy2(generated_swift, ffi_sources / generated_swift.name)
        sources = self._selected_swift_sources(resolution)
        self._materialize_sources(
            sources=sources,
            output=output,
            platform_name="Swift",
            selected_keys=selected_keys,
            swift=True,
        )
        included_targets = {source.target for source in sources}
        (output / "Package.swift").write_text(
            self._swift_package(included_targets), encoding="utf-8"
        )

    def _build_kotlin(
        self,
        *,
        output: Path,
        build_root: Path,
        resolution: CargoResolution,
        selected_keys: set[str],
        profile: str,
        host_target: str,
    ) -> None:
        if self.system not in {"Darwin", "Linux"}:
            raise NativePrepareError(
                f"Kotlin/JVM native packaging supports Darwin and Linux, not {self.system}"
            )
        output.mkdir(parents=True)
        build_root.mkdir(parents=True)
        cargo_env = self._cargo_env()
        args = [
            "cargo",
            "build",
            "--locked",
            "--package",
            self.catalog.artifact.ffi_package,
            "--no-default-features",
            *self._cargo_feature_args(resolution),
            *self._profile_args(profile),
            "--target",
            host_target,
            "--lib",
            "--bin",
            self.catalog.artifact.bindgen_bin,
        ]
        target_env = self._target_cargo_env(cargo_env, host_target)
        self.runner.run(args, cwd=self.repo_root, env=target_env)
        extension = "dylib" if self.system == "Darwin" else "so"
        native_name = f"lib{self.catalog.artifact.library_stem}.{extension}"
        native_library = (
            Path(cargo_env["CARGO_TARGET_DIR"])
            / host_target
            / self._profile_dir(profile)
            / native_name
        )
        if not native_library.is_file():
            raise NativePrepareError(
                f"Cargo did not produce expected Kotlin/JVM library {native_library}"
            )

        generated = build_root / "generated"
        generated.mkdir()
        bindgen = (
            Path(cargo_env["CARGO_TARGET_DIR"])
            / host_target
            / self._profile_dir(profile)
            / self.catalog.artifact.bindgen_bin
        )
        if not bindgen.is_file():
            raise NativePrepareError(f"Cargo did not produce expected bindgen tool {bindgen}")
        bindgen_args = [
            str(bindgen),
            "generate",
            "--library",
            str(native_library),
            "--language",
            "kotlin",
            "--out-dir",
            str(generated),
            "--no-format",
        ]
        self.runner.run(bindgen_args, cwd=self.repo_root, env=target_env)
        stem = self.catalog.artifact.library_stem
        generated_binding = generated / "uniffi" / stem / f"{stem}.kt"
        if not generated_binding.is_file():
            raise NativePrepareError(
                f"UniFFI did not produce expected Kotlin binding {generated_binding}"
            )
        binding_destination = output / "src" / "main" / "kotlin" / "uniffi" / stem
        binding_destination.mkdir(parents=True)
        shutil.copy2(generated_binding, binding_destination / generated_binding.name)

        self._materialize_sources(
            sources=self._selected_kotlin_sources(resolution),
            output=output,
            platform_name="Kotlin",
            selected_keys=selected_keys,
            swift=False,
        )
        prefix = self._jna_prefix()
        resource = output / "src" / "main" / "resources" / prefix
        resource.mkdir(parents=True)
        shutil.copy2(native_library, resource / native_name)
        if self.system == "Darwin":
            legacy = output / "src" / "main" / "resources" / "darwin"
            legacy.mkdir(parents=True)
            shutil.copy2(native_library, legacy / native_name)
        (output / "settings.gradle.kts").write_text(
            f'rootProject.name = "{self.catalog.kotlin.project_name}"\n', encoding="utf-8"
        )
        (output / "build.gradle.kts").write_text(self._kotlin_gradle(), encoding="utf-8")

    def _jna_prefix(self) -> str:
        os_name = {"Darwin": "darwin", "Linux": "linux"}[self.system]
        arch = {
            "arm64": "aarch64",
            "aarch64": "aarch64",
            "x86_64": "x86-64",
            "amd64": "x86-64",
        }.get(self.machine.lower())
        if arch is None:
            raise NativePrepareError(f"unsupported Kotlin/JVM host architecture {self.machine}")
        return f"{os_name}-{arch}"

    def _target_cargo_env(
        self, cargo_env: Mapping[str, str], target: str
    ) -> dict[str, str]:
        target_env = dict(cargo_env)
        if target.endswith("apple-darwin"):
            minimum = self.catalog.apple.macos_deployment_target
            target_env["MACOSX_DEPLOYMENT_TARGET"] = minimum
            target_env["CFLAGS"] = _append_flag(
                os.environ.get("CFLAGS", ""), f"-mmacosx-version-min={minimum}"
            )
            target_env["CXXFLAGS"] = _append_flag(
                os.environ.get("CXXFLAGS", ""), f"-mmacosx-version-min={minimum}"
            )
        return target_env

    def _selected_swift_sources(
        self, resolution: CargoResolution
    ) -> tuple[SourceSpec, ...]:
        sources = list(self.catalog.core_swift_sources)
        for feature in resolution.resolved_features:
            sources.extend(feature.swift_sources)
        return _dedupe_sources(sources, "Swift")

    def _selected_kotlin_sources(
        self, resolution: CargoResolution
    ) -> tuple[SourceSpec, ...]:
        sources = list(self.catalog.core_kotlin_sources)
        for feature in resolution.resolved_features:
            sources.extend(feature.kotlin_sources)
        return _dedupe_sources(sources, "Kotlin")

    def _materialize_sources(
        self,
        *,
        sources: Sequence[SourceSpec],
        output: Path,
        platform_name: str,
        selected_keys: set[str],
        swift: bool,
    ) -> None:
        known = set(self.catalog.by_key)
        for source in sources:
            source_path = _resolve_repo_path(
                self.repo_root, source.path, f"{platform_name} catalog source"
            )
            if swift:
                destination = output / "Sources" / str(source.target) / source.destination
            else:
                destination = output / "src" / "main" / "kotlin" / source.destination
            destination.parent.mkdir(parents=True, exist_ok=True)
            text = source_path.read_text(encoding="utf-8")
            filtered = filter_source(text, selected_keys, known, source.path)
            destination.write_text(filtered, encoding="utf-8")

    def _swift_package(self, included_targets: set[str | None]) -> str:
        target_by_name = {target.name: target for target in self.catalog.apple.targets}
        names = {name for name in included_targets if name is not None}
        pending = list(names)
        while pending:
            name = pending.pop()
            target = target_by_name[name]
            for dependency in target.dependencies:
                if dependency in target_by_name and dependency not in names:
                    names.add(dependency)
                    pending.append(dependency)
        lines = [
            "// swift-tools-version:5.9",
            "// Generated by nmp-native; edit the checked-in app feature manifest instead.",
            "import PackageDescription",
            "",
            "let package = Package(",
            f'    name: "{self.catalog.apple.package_name}",',
            "    platforms: [",
        ]
        lines.extend(f"        {declaration}," for declaration in self.catalog.apple.platforms)
        lines.extend(["    ],", "    products: ["])
        lines.extend(
            f'        .library(name: "{name}", targets: ["{name}"]),' for name in sorted(names)
        )
        lines.extend(
            [
                "    ],",
                "    targets: [",
                "        .binaryTarget(",
                f'            name: "{self.catalog.apple.binary_target}",',
                f'            path: "{self.catalog.apple.xcframework_name}"',
                "        ),",
                "        .target(",
                f'            name: "{self.catalog.apple.ffi_target}",',
                f'            dependencies: ["{self.catalog.apple.binary_target}"],',
                "            linkerSettings: [",
            ]
        )
        for framework in self.catalog.apple.linked_frameworks:
            lines.append(f'                .linkedFramework("{framework}"),')
        lines.extend(["            ]", "        ),"])
        for name in sorted(names):
            target = target_by_name[name]
            dependencies = ", ".join(f'"{dependency}"' for dependency in target.dependencies)
            lines.extend(
                [
                    "        .target(",
                    f'            name: "{name}",',
                    f"            dependencies: [{dependencies}]",
                    "        ),",
                ]
            )
        lines.extend(["    ]", ")", ""])
        return "\n".join(lines)

    def _kotlin_gradle(self) -> str:
        lines = [
            "// Generated by nmp-native; edit the checked-in app feature manifest instead.",
            "plugins {",
            '    kotlin("jvm") version "2.0.21"',
            "}",
            "",
            f'group = "{self.catalog.kotlin.group}"',
            f'version = "{self.catalog.kotlin.version}"',
            "",
            "repositories {",
            "    mavenCentral()",
            "}",
            "",
            "dependencies {",
        ]
        lines.extend(
            f'    implementation("{dependency}")'
            for dependency in self.catalog.kotlin.dependencies
        )
        lines.extend(
            [
                "}",
                "",
                "kotlin {",
                f"    jvmToolchain({self.catalog.kotlin.jvm_toolchain})",
                "}",
                "",
            ]
        )
        return "\n".join(lines)

    def _verify_cached_output(self, output: Path, identity: str) -> None:
        provenance_path = output / PROVENANCE_FILE
        marker_path = output / GENERATED_MARKER
        try:
            provenance = json.loads(provenance_path.read_text(encoding="utf-8"))
            marker = marker_path.read_text(encoding="utf-8").strip()
        except (OSError, json.JSONDecodeError) as error:
            raise NativePrepareError(f"cache entry {output.parent} is incomplete: {error}") from error
        if provenance.get("identity") != identity or marker != identity:
            raise NativePrepareError(f"cache entry {output.parent} has mismatched provenance")
        expected = provenance.get("contents")
        actual = _content_inventory(output)
        if expected != actual:
            raise NativePrepareError(f"cache entry {output.parent} content hash mismatch")

    def _materialize(self, cache_output: Path, output: Path) -> None:
        if output == self.repo_root or output == Path(output.anchor):
            raise NativePrepareError(f"refusing to replace unsafe output path {output}")
        output.parent.mkdir(parents=True, exist_ok=True)
        if output.exists():
            if not output.is_dir():
                raise NativePrepareError(f"output exists and is not a directory: {output}")
            marker = output / GENERATED_MARKER
            if any(output.iterdir()) and not marker.is_file():
                raise NativePrepareError(
                    f"refusing to replace non-generated output directory {output}"
                )
        staged = Path(tempfile.mkdtemp(prefix=f".{output.name}-", dir=output.parent))
        shutil.rmtree(staged)
        shutil.copytree(cache_output, staged)
        backup: Path | None = None
        try:
            if output.exists():
                backup = Path(
                    tempfile.mkdtemp(prefix=f".{output.name}-old-", dir=output.parent)
                )
                backup.rmdir()
                os.replace(output, backup)
            os.replace(staged, output)
            if backup is not None:
                shutil.rmtree(backup)
        except BaseException:
            if staged.exists():
                shutil.rmtree(staged)
            if backup is not None and backup.exists() and not output.exists():
                os.replace(backup, output)
            raise


def _dedupe_sources(sources: Iterable[SourceSpec], platform_name: str) -> tuple[SourceSpec, ...]:
    by_destination: dict[tuple[str | None, str], SourceSpec] = {}
    for source in sources:
        key = (source.target, source.destination)
        previous = by_destination.get(key)
        if previous is not None and previous != source:
            raise NativePrepareError(
                f"{platform_name} catalog maps both {previous.path} and {source.path} "
                f"to {source.destination}"
            )
        by_destination[key] = source
    return tuple(by_destination[key] for key in sorted(by_destination, key=lambda item: str(item)))


def _canonical_json(value: Any) -> bytes:
    return json.dumps(value, sort_keys=True, separators=(",", ":")).encode("utf-8")


def _append_flag(existing: str, flag: str) -> str:
    return f"{existing} {flag}".strip()


def _sha256_file(path: Path) -> str:
    return hashlib.sha256(path.read_bytes()).hexdigest()


def _content_inventory(root: Path) -> list[Mapping[str, str]]:
    inventory: list[Mapping[str, str]] = []
    for path in sorted(root.rglob("*")):
        if not path.is_file() or path.name in {PROVENANCE_FILE, GENERATED_MARKER}:
            continue
        inventory.append(
            {
                "path": path.relative_to(root).as_posix(),
                "sha256": _sha256_file(path),
            }
        )
    return inventory


def _default_cache_dir() -> Path:
    configured = os.environ.get("XDG_CACHE_HOME")
    if configured:
        return Path(configured) / "nmp-native"
    if platform.system() == "Darwin":
        return Path.home() / "Library" / "Caches" / "nmp-native"
    return Path.home() / ".cache" / "nmp-native"


def _parse_args(argv: Sequence[str]) -> argparse.Namespace:
    parser = argparse.ArgumentParser(prog="nmp-native")
    subparsers = parser.add_subparsers(dest="command", required=True)
    prepare = subparsers.add_parser(
        "prepare", help="build or reuse the exact native artifact selected by an app manifest"
    )
    prepare.add_argument("--manifest", type=Path, required=True)
    prepare.add_argument("--output", type=Path, required=True)
    prepare.add_argument(
        "--platform",
        action="append",
        choices=("apple", "kotlin-jvm"),
        required=True,
    )
    prepare.add_argument("--profile", default="release")
    prepare.add_argument("--apple-target", action="append", default=[])
    prepare.add_argument("--repo", type=Path)
    prepare.add_argument("--catalog", type=Path)
    prepare.add_argument("--cache-dir", type=Path, default=_default_cache_dir())
    prepare.add_argument("--quiet", action="store_true")
    return parser.parse_args(argv)


def main(argv: Sequence[str] | None = None) -> int:
    args = _parse_args(argv or sys.argv[1:])
    script = Path(__file__).resolve()
    repo_root = (args.repo or script.parents[2]).resolve()
    catalog_path = (args.catalog or repo_root / "native" / "features.toml").resolve()
    try:
        manifest = load_manifest(args.manifest.resolve())
        catalog = load_catalog(catalog_path, repo_root)
        result = NativePreparer(
            repo_root=repo_root,
            catalog=catalog,
            runner=CommandRunner(verbose=not args.quiet),
            cache_dir=args.cache_dir,
        ).prepare(
            manifest=manifest,
            output=args.output,
            platforms=tuple(args.platform),
            profile=args.profile,
            apple_targets=tuple(args.apple_target),
        )
    except NativePrepareError as error:
        print(f"nmp-native: error: {error}", file=sys.stderr)
        return 2
    except subprocess.CalledProcessError as error:
        detail = (error.stderr or error.stdout or "").strip()
        suffix = f"\n{detail}" if detail else ""
        print(
            f"nmp-native: error: command failed with exit {error.returncode}: "
            f"{' '.join(map(str, error.cmd))}{suffix}",
            file=sys.stderr,
        )
        return 2
    print(json.dumps(result, sort_keys=True, indent=2))
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
