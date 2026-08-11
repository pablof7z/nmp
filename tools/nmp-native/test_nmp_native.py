from __future__ import annotations

import ast
import json
import re
import tempfile
import textwrap
import tomllib
import unittest
from pathlib import Path
from typing import Mapping, Sequence

import nmp_native


class FakeRunner(nmp_native.CommandRunner):
    def __init__(self, repo: Path, *, dependencies: Mapping[str, set[str]] | None = None) -> None:
        super().__init__(verbose=False)
        self.repo = repo
        self.dependencies = dict(dependencies or {})
        self.commands: list[tuple[str, ...]] = []

    def run(
        self,
        args: Sequence[str],
        *,
        cwd: Path,
        env: Mapping[str, str] | None = None,
        capture: bool = False,
    ) -> nmp_native.CommandResult:
        command = tuple(str(arg) for arg in args)
        self.commands.append(command)
        if command[:2] == ("cargo", "metadata"):
            resolver_manifest = Path(command[command.index("--manifest-path") + 1])
            resolver_data = tomllib.loads(resolver_manifest.read_text(encoding="utf-8"))
            selected = set(
                resolver_data["dependencies"]["selected-nmp-ffi"].get("features", [])
            )
            changed = True
            while changed:
                changed = False
                for feature in tuple(selected):
                    expanded = self.dependencies.get(feature, set()) - selected
                    if expanded:
                        selected.update(expanded)
                        changed = True
            manifest = (self.repo / "crates" / "sample-ffi" / "Cargo.toml").resolve()
            metadata = {
                "packages": [
                    {
                        "name": "sample-ffi",
                        "version": "0.1.0",
                        "id": "path+sample-ffi#0.1.0",
                        "manifest_path": str(manifest),
                    }
                ],
                "resolve": {
                    "nodes": [
                        {
                            "id": "path+sample-ffi#0.1.0",
                            "features": sorted(selected),
                        }
                    ]
                },
            }
            return nmp_native.CommandResult(json.dumps(metadata), "")
        if command == ("cargo", "-V"):
            return nmp_native.CommandResult("cargo 1.88.0\n", "")
        if command == ("rustc", "-vV"):
            return nmp_native.CommandResult(
                "rustc 1.88.0\nhost: aarch64-apple-darwin\nrelease: 1.88.0\n", ""
            )
        if command == ("xcodebuild", "-version"):
            return nmp_native.CommandResult("Xcode 16.4\nBuild version 16F6\n", "")
        if command == ("git", "rev-parse", "HEAD"):
            return nmp_native.CommandResult("0123456789abcdef\n", "")
        if command == ("rustup", "target", "list", "--installed"):
            return nmp_native.CommandResult("aarch64-apple-darwin\n", "")
        if command[:2] == ("rustup", "target"):
            return nmp_native.CommandResult()
        if command[:2] == ("cargo", "build"):
            assert env is not None
            target = command[command.index("--target") + 1]
            profile = "release"
            if "--profile" in command:
                profile = command[command.index("--profile") + 1]
            target_dir = Path(env["CARGO_TARGET_DIR"]) / target / profile
            target_dir.mkdir(parents=True, exist_ok=True)
            (target_dir / "libsample_ffi.a").write_bytes(b"static-library")
            (target_dir / "libsample_ffi.dylib").write_bytes(b"dynamic-library")
            bindgen = target_dir / "uniffi-bindgen"
            bindgen.write_text("fixture executable\n")
            bindgen.chmod(0o755)
            return nmp_native.CommandResult()
        if Path(command[0]).name == "uniffi-bindgen":
            language = command[command.index("--language") + 1]
            out = Path(command[command.index("--out-dir") + 1])
            if language == "swift":
                out.mkdir(parents=True, exist_ok=True)
                (out / "sample_ffi.swift").write_text("// generated swift\n")
                (out / "sample_ffiFFI.h").write_text("// generated header\n")
                (out / "sample_ffiFFI.modulemap").write_text("module sample {}\n")
            else:
                generated = out / "uniffi" / "sample_ffi"
                generated.mkdir(parents=True, exist_ok=True)
                (generated / "sample_ffi.kt").write_text("// generated kotlin\n")
            return nmp_native.CommandResult()
        if command[0] == "lipo":
            output = Path(command[command.index("-output") + 1])
            output.parent.mkdir(parents=True, exist_ok=True)
            output.write_bytes(b"fat-static-library")
            return nmp_native.CommandResult()
        if command[:2] == ("xcodebuild", "-create-xcframework"):
            output = Path(command[command.index("-output") + 1])
            output.mkdir(parents=True, exist_ok=True)
            (output / "Info.plist").write_text("fixture\n")
            return nmp_native.CommandResult()
        raise AssertionError(f"unexpected fake command: {command}")


def write(path: Path, text: str) -> None:
    path.parent.mkdir(parents=True, exist_ok=True)
    path.write_text(textwrap.dedent(text).lstrip(), encoding="utf-8")


def fixture_repo(root: Path) -> Path:
    write(root / "Cargo.toml", "[workspace]\nmembers = [\"crates/sample-ffi\"]\n")
    write(root / "Cargo.lock", "version = 4\n")
    write(root / "rust-toolchain.toml", "[toolchain]\nchannel = \"stable\"\n")
    write(
        root / "crates" / "sample-ffi" / "Cargo.toml",
        """
        [package]
        name = "sample-ffi"
        version = "0.1.0"
        edition = "2021"
        """,
    )
    write(root / "crates" / "sample-ffi" / "src" / "lib.rs", "pub fn core() {}\n")
    write(root / "crates" / "sample-ffi" / "src" / "alpha.rs", "pub fn alpha() {}\n")
    write(root / "crates" / "sample-ffi" / "src" / "beta.rs", "pub fn beta() {}\n")
    write(
        root / "sdk" / "Core.swift",
        """
        public let core = true
        // nmp-native:if alpha
        public let alphaInCore = true
        // nmp-native:endif
        // nmp-native:if beta
        public let betaInCore = true
        // nmp-native:endif
        """,
    )
    write(root / "sdk" / "Alpha.swift", "public let alpha = true\n")
    write(root / "sdk" / "Beta.swift", "public let beta = true\n")
    write(
        root / "sdk" / "Core.kt",
        """
        package test
        val core = true
        // nmp-native:if alpha
        val alphaInCore = true
        // nmp-native:endif
        // nmp-native:if beta
        val betaInCore = true
        // nmp-native:endif
        """,
    )
    write(root / "sdk" / "Alpha.kt", "package test\nval alpha = true\n")
    write(root / "sdk" / "Beta.kt", "package test\nval beta = true\n")
    catalog = root / "native" / "features.toml"
    write(
        catalog,
        """
        schema = 1

        [artifact]
        ffi_package = "sample-ffi"
        ffi_manifest = "crates/sample-ffi/Cargo.toml"
        library_stem = "sample_ffi"
        bindgen_bin = "uniffi-bindgen"

        [apple]
        package_name = "Sample"
        xcframework_name = "Sample.xcframework"
        binary_target = "sample_ffiFFI"
        ffi_target = "SampleFFI"
        macos_deployment_target = "13.0"
        platforms = [".macOS(.v13)"]
        linked_frameworks = []
        targets = [{ name = "Sample", dependencies = ["SampleFFI"] }]
        slices = [{ name = "macos", targets = ["aarch64-apple-darwin"] }]

        [kotlin]
        project_name = "sample-kotlin"
        group = "test.sample"
        version = "0.0.0"
        jvm_toolchain = 17
        dependencies = ["net.java.dev.jna:jna:5.14.0"]

        [core]
        ffi_sources = ["crates/sample-ffi/src/lib.rs"]
        swift_sources = [
          { path = "sdk/Core.swift", target = "Sample", destination = "Core.swift" },
        ]
        kotlin_sources = [
          { path = "sdk/Core.kt", destination = "test/Core.kt" },
        ]

        [[features]]
        key = "alpha"
        cargo_feature = "alpha"
        ffi_sources = ["crates/sample-ffi/src/alpha.rs"]
        swift_sources = [
          { path = "sdk/Alpha.swift", target = "Sample", destination = "Alpha.swift" },
        ]
        kotlin_sources = [
          { path = "sdk/Alpha.kt", destination = "test/Alpha.kt" },
        ]

        [[features]]
        key = "beta"
        cargo_feature = "beta"
        ffi_sources = ["crates/sample-ffi/src/beta.rs"]
        swift_sources = [
          { path = "sdk/Beta.swift", target = "Sample", destination = "Beta.swift" },
        ]
        kotlin_sources = [
          { path = "sdk/Beta.kt", destination = "test/Beta.kt" },
        ]
        """,
    )
    return catalog


class ManifestAndCatalogTests(unittest.TestCase):
    def test_manifest_is_canonical_and_rejects_runtime_fields(self) -> None:
        with tempfile.TemporaryDirectory() as temporary:
            root = Path(temporary)
            manifest = root / "app.toml"
            write(manifest, 'schema = 1\nfeatures = ["beta", "alpha"]\n')
            self.assertEqual(nmp_native.load_manifest(manifest).features, ("alpha", "beta"))
            write(
                manifest,
                'schema = 1\nfeatures = []\nindexer_relays = ["wss://example.test"]\n',
            )
            with self.assertRaisesRegex(nmp_native.NativePrepareError, "unknown field"):
                nmp_native.load_manifest(manifest)

    def test_checked_catalog_is_valid_and_canonically_ordered(self) -> None:
        repo = Path(__file__).resolve().parents[2]
        catalog = nmp_native.load_catalog(repo / "native" / "features.toml", repo)
        self.assertEqual(
            [feature.key for feature in catalog.features],
            sorted(feature.key for feature in catalog.features),
        )

    def test_generic_tool_has_no_literal_catalog_feature_keys(self) -> None:
        repo = Path(__file__).resolve().parents[2]
        catalog = nmp_native.load_catalog(repo / "native" / "features.toml", repo)
        tree = ast.parse(Path(nmp_native.__file__).read_text(encoding="utf-8"))
        literals = {
            node.value
            for node in ast.walk(tree)
            if isinstance(node, ast.Constant) and isinstance(node.value, str)
        }
        self.assertEqual(set(catalog.by_key) & literals, set())

    def test_checked_all_manifest_tracks_the_catalog_exactly(self) -> None:
        repo = Path(__file__).resolve().parents[2]
        catalog = nmp_native.load_catalog(repo / "native" / "features.toml", repo)
        selected = nmp_native.load_manifest(repo / "native" / "examples" / "all.toml")
        self.assertEqual(selected.features, tuple(feature.key for feature in catalog.features))

    def test_every_checked_sdk_marker_is_consumed_for_core_nip65_and_all(self) -> None:
        repo = Path(__file__).resolve().parents[2]
        catalog = nmp_native.load_catalog(repo / "native" / "features.toml", repo)
        known = set(catalog.by_key)
        sources = list(catalog.core_swift_sources) + list(catalog.core_kotlin_sources)
        for feature in catalog.features:
            sources.extend(feature.swift_sources)
            sources.extend(feature.kotlin_sources)
        for source in nmp_native._dedupe_sources(sources, "SDK"):
            text = (repo / source.path).read_text(encoding="utf-8")
            for selected in (set(), {"nip65"}, known):
                filtered = nmp_native.filter_source(text, selected, known, source.path)
                self.assertNotIn("nmp-native:", filtered, source.path)

    def test_checked_nip65_blocks_remove_auto_and_config_from_core(self) -> None:
        repo = Path(__file__).resolve().parents[2]
        catalog = nmp_native.load_catalog(repo / "native" / "features.toml", repo)
        known = set(catalog.by_key)
        cases = (
            (
                repo / "Packages" / "NMP" / "Sources" / "NMP" / "WriteIntent.swift",
                re.compile(r"^\s*case\s+(?:\.)?auto\b", re.IGNORECASE | re.MULTILINE),
            ),
            (
                repo
                / "Packages"
                / "NMPKotlin"
                / "src"
                / "main"
                / "kotlin"
                / "com"
                / "nmp"
                / "sdk"
                / "WriteIntent.kt",
                re.compile(r"^\s*object\s+auto\b", re.IGNORECASE | re.MULTILINE),
            ),
        )
        for path, auto_declaration in cases:
            source = path.read_text(encoding="utf-8")
            core = nmp_native.filter_source(source, set(), known, path.as_posix())
            nip65 = nmp_native.filter_source(source, {"nip65"}, known, path.as_posix())
            self.assertIsNone(auto_declaration.search(core), path.as_posix())
            self.assertIsNotNone(auto_declaration.search(nip65), path.as_posix())

        engine_paths = (
            repo / "Packages" / "NMP" / "Sources" / "NMP" / "Engine.swift",
            repo
            / "Packages"
            / "NMPKotlin"
            / "src"
            / "main"
            / "kotlin"
            / "com"
            / "nmp"
            / "sdk"
            / "Engine.kt",
        )
        for path in engine_paths:
            source = path.read_text(encoding="utf-8")
            core = nmp_native.filter_source(source, set(), known, path.as_posix())
            spelling_neutral = re.sub(r"[^a-z0-9]", "", core.lower())
            self.assertNotIn("nip65config", spelling_neutral, path.as_posix())


class ConditionalSourceTests(unittest.TestCase):
    def test_generic_nested_filter_keeps_only_active_blocks(self) -> None:
        source = textwrap.dedent(
            """
            core
            // nmp-native:if alpha
            alpha
            // nmp-native:if beta
            both
            // nmp-native:endif
            // nmp-native:endif
            // nmp-native:if beta
            beta
            // nmp-native:endif
            """
        ).lstrip()
        filtered = nmp_native.filter_source(source, {"alpha"}, {"alpha", "beta"}, "x")
        self.assertEqual(filtered, "core\nalpha\n")

    def test_filter_rejects_unknown_or_unbalanced_markers(self) -> None:
        with self.assertRaisesRegex(nmp_native.NativePrepareError, "unknown feature"):
            nmp_native.filter_source(
                "// nmp-native:if missing\nvalue\n// nmp-native:endif\n",
                set(),
                {"alpha"},
                "x",
            )
        with self.assertRaisesRegex(nmp_native.NativePrepareError, "unterminated"):
            nmp_native.filter_source(
                "// nmp-native:if alpha\nvalue\n", {"alpha"}, {"alpha"}, "x"
            )

    def test_filter_preserves_a_block_comment_closer_on_marker_line(self) -> None:
        source = textwrap.dedent(
            """
            /** core
             * nmp-native:if alpha
             * optional detail
             * nmp-native:endif */
            declaration
            """
        ).lstrip()
        filtered = nmp_native.filter_source(source, set(), {"alpha"}, "x")
        self.assertEqual(filtered, "/** core\n * */\ndeclaration\n")


class PreparationTests(unittest.TestCase):
    def setUp(self) -> None:
        self.temporary = tempfile.TemporaryDirectory()
        self.root = Path(self.temporary.name)
        self.catalog_path = fixture_repo(self.root)
        self.catalog = nmp_native.load_catalog(self.catalog_path, self.root)
        self.runner = FakeRunner(self.root, dependencies={"beta": {"alpha"}})
        self.cache = self.root / "cache"
        self.preparer = nmp_native.NativePreparer(
            repo_root=self.root,
            catalog=self.catalog,
            runner=self.runner,
            cache_dir=self.cache,
            system="Darwin",
            machine="arm64",
        )

    def tearDown(self) -> None:
        self.temporary.cleanup()

    def manifest(self, features: Sequence[str], name: str = "app.toml") -> nmp_native.AppManifest:
        path = self.root / name
        rendered = ", ".join(json.dumps(feature) for feature in features)
        write(path, f"schema = 1\nfeatures = [{rendered}]\n")
        return nmp_native.load_manifest(path)

    def test_cargo_dependency_activation_materializes_both_platform_surfaces(self) -> None:
        output = self.root / "generated"
        result = self.preparer.prepare(
            manifest=self.manifest(["beta"]),
            output=output,
            platforms=("apple", "kotlin-jvm"),
            profile="release",
        )
        self.assertEqual(result["requested_features"], ["beta"])
        self.assertEqual(result["resolved_features"], ["alpha", "beta"])
        self.assertTrue((output / "apple" / "Sample.xcframework" / "Info.plist").is_file())
        self.assertTrue((output / "apple" / "Sources" / "Sample" / "Alpha.swift").is_file())
        self.assertTrue((output / "apple" / "Sources" / "Sample" / "Beta.swift").is_file())
        core_swift = (output / "apple" / "Sources" / "Sample" / "Core.swift").read_text()
        self.assertIn("alphaInCore", core_swift)
        self.assertIn("betaInCore", core_swift)
        self.assertTrue(
            (
                output
                / "kotlin-jvm"
                / "src"
                / "main"
                / "resources"
                / "darwin-aarch64"
                / "libsample_ffi.dylib"
            ).is_file()
        )
        provenance = json.loads((output / nmp_native.PROVENANCE_FILE).read_text())
        self.assertEqual(provenance["identity"], result["identity"])
        self.assertEqual(
            provenance["identity_inputs"]["resolved_features"], ["alpha", "beta"]
        )
        gradle = (output / "kotlin-jvm" / "build.gradle.kts").read_text()
        self.assertIn('group = "test.sample"', gradle)
        self.assertIn('version = "0.0.0"', gradle)
        bindgen_commands = [
            command for command in self.runner.commands if Path(command[0]).name == "uniffi-bindgen"
        ]
        self.assertEqual(len(bindgen_commands), 2)
        self.assertTrue(all("--no-format" in command for command in bindgen_commands))
        self.assertFalse(any(command[:2] == ("cargo", "run") for command in self.runner.commands))

    def test_canonical_order_is_same_identity_and_second_prepare_is_cache_hit(self) -> None:
        first = self.preparer.prepare(
            manifest=self.manifest(["beta", "alpha"], "first.toml"),
            output=self.root / "first-output",
            platforms=("apple",),
            profile="release",
        )
        build_count = sum(command[:2] == ("cargo", "build") for command in self.runner.commands)
        second = self.preparer.prepare(
            manifest=self.manifest(["alpha", "beta"], "second.toml"),
            output=self.root / "second-output",
            platforms=("apple",),
            profile="release",
        )
        self.assertEqual(first["identity"], second["identity"])
        self.assertTrue(second["cache_hit"])
        self.assertEqual(
            sum(command[:2] == ("cargo", "build") for command in self.runner.commands),
            build_count,
        )
        self.assertEqual(first["contents"], second["contents"])

    def test_changed_feature_set_changes_identity_and_omits_unselected_sources(self) -> None:
        both = self.preparer.prepare(
            manifest=self.manifest(["beta"], "both.toml"),
            output=self.root / "both-output",
            platforms=("kotlin-jvm",),
            profile="release",
        )
        alpha = self.preparer.prepare(
            manifest=self.manifest(["alpha"], "alpha.toml"),
            output=self.root / "alpha-output",
            platforms=("kotlin-jvm",),
            profile="release",
        )
        self.assertNotEqual(both["identity"], alpha["identity"])
        self.assertTrue(
            (
                self.root
                / "alpha-output"
                / "kotlin-jvm"
                / "src"
                / "main"
                / "kotlin"
                / "test"
                / "Alpha.kt"
            ).is_file()
        )
        self.assertFalse(
            (
                self.root
                / "alpha-output"
                / "kotlin-jvm"
                / "src"
                / "main"
                / "kotlin"
                / "test"
                / "Beta.kt"
            ).exists()
        )
        core = (
            self.root
            / "alpha-output"
            / "kotlin-jvm"
            / "src"
            / "main"
            / "kotlin"
            / "test"
            / "Core.kt"
        ).read_text()
        self.assertIn("alphaInCore", core)
        self.assertNotIn("betaInCore", core)

    def test_unknown_and_unregistered_active_features_fail_before_build(self) -> None:
        with self.assertRaisesRegex(nmp_native.NativePrepareError, "unknown or internal-only"):
            self.preparer.prepare(
                manifest=self.manifest(["internal"], "unknown.toml"),
                output=self.root / "unknown-output",
                platforms=("apple",),
                profile="release",
            )
        unregistered_runner = FakeRunner(self.root, dependencies={"alpha": {"internal"}})
        preparer = nmp_native.NativePreparer(
            repo_root=self.root,
            catalog=self.catalog,
            runner=unregistered_runner,
            cache_dir=self.root / "other-cache",
            system="Darwin",
            machine="arm64",
        )
        with self.assertRaisesRegex(nmp_native.NativePrepareError, "no catalog metadata"):
            preparer.prepare(
                manifest=self.manifest(["alpha"], "unregistered.toml"),
                output=self.root / "unregistered-output",
                platforms=("apple",),
                profile="release",
            )
        self.assertFalse(
            any(command[:2] == ("cargo", "build") for command in unregistered_runner.commands)
        )

    def test_missing_feature_source_breaks_only_selections_that_resolve_it(self) -> None:
        (self.root / "sdk" / "Beta.swift").unlink()
        catalog = nmp_native.load_catalog(self.catalog_path, self.root)
        runner = FakeRunner(self.root, dependencies={"beta": {"alpha"}})
        preparer = nmp_native.NativePreparer(
            repo_root=self.root,
            catalog=catalog,
            runner=runner,
            cache_dir=self.root / "deletion-cache",
            system="Darwin",
            machine="arm64",
        )

        unrelated = preparer.prepare(
            manifest=self.manifest(["alpha"], "unrelated.toml"),
            output=self.root / "unrelated-output",
            platforms=("apple",),
            profile="release",
        )
        self.assertEqual(unrelated["resolved_features"], ["alpha"])

        build_count = sum(
            command[:2] == ("cargo", "build") for command in runner.commands
        )
        with self.assertRaisesRegex(
            nmp_native.NativePrepareError, "selected feature beta source does not exist"
        ):
            preparer.prepare(
                manifest=self.manifest(["beta"], "selected.toml"),
                output=self.root / "selected-output",
                platforms=("apple",),
                profile="release",
            )
        self.assertEqual(
            sum(command[:2] == ("cargo", "build") for command in runner.commands),
            build_count,
        )

    def test_refuses_to_replace_unowned_output(self) -> None:
        output = self.root / "owned-by-user"
        write(output / "important.txt", "keep me\n")
        with self.assertRaisesRegex(nmp_native.NativePrepareError, "non-generated"):
            self.preparer.prepare(
                manifest=self.manifest([], "core.toml"),
                output=output,
                platforms=("kotlin-jvm",),
                profile="release",
            )
        self.assertEqual((output / "important.txt").read_text(), "keep me\n")


class RealRepositoryResolverTests(unittest.TestCase):
    def test_cargo_metadata_resolves_checked_acceptance_manifests_exactly(self) -> None:
        repo = Path(__file__).resolve().parents[2]
        catalog = nmp_native.load_catalog(repo / "native" / "features.toml", repo)
        with tempfile.TemporaryDirectory() as temporary:
            preparer = nmp_native.NativePreparer(
                repo_root=repo,
                catalog=catalog,
                runner=nmp_native.CommandRunner(verbose=False),
                cache_dir=Path(temporary) / "cache",
            )
            core = nmp_native.load_manifest(repo / "native" / "examples" / "core.toml")
            normal = nmp_native.load_manifest(
                repo / "native" / "examples" / "normal-client.toml"
            )
            representative = nmp_native.load_manifest(
                repo / "native" / "examples" / "representative-mix.toml"
            )
            core_resolution = preparer.resolve(core)
            normal_resolution = preparer.resolve(normal)
            representative_resolution = preparer.resolve(representative)

        self.assertEqual(core_resolution.active_ffi_features, ())
        self.assertEqual(core_resolution.resolved_features, ())
        self.assertEqual(
            tuple(feature.key for feature in normal_resolution.resolved_features),
            normal.features,
        )
        self.assertEqual(normal_resolution.active_ffi_features, normal.features)
        self.assertEqual(
            tuple(feature.key for feature in representative_resolution.resolved_features),
            ("asset",) + representative.features,
        )
        self.assertEqual(
            representative_resolution.active_ffi_features,
            ("asset",) + representative.features,
        )


if __name__ == "__main__":
    unittest.main()
