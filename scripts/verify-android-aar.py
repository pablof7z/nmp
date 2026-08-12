#!/usr/bin/env python3
"""Verify one feature-selected NMP Android output or compare two AARs."""

from __future__ import annotations

import argparse
import hashlib
import io
import json
import os
import re
import subprocess
import sys
import tomllib
import zipfile
from pathlib import Path
from typing import Any, Mapping


class VerificationError(RuntimeError):
    pass


def sha256(data: bytes) -> str:
    return hashlib.sha256(data).hexdigest()


def semantic_zip_inventory(path: Path) -> list[Mapping[str, Any]]:
    with zipfile.ZipFile(path) as archive:
        return [
            {
                "path": info.filename,
                "sha256": sha256(archive.read(info)),
                "mode": (info.external_attr >> 16) & 0o777,
            }
            for info in sorted(archive.infolist(), key=lambda item: item.filename)
            if not info.is_dir()
        ]


def catalog(root: Path) -> Mapping[str, Any]:
    return tomllib.loads((root / "native" / "features.toml").read_text(encoding="utf-8"))


def llvm_tools(ndk: Path) -> tuple[Path, Path]:
    candidates = sorted((ndk / "toolchains" / "llvm" / "prebuilt").glob("*/bin/llvm-nm"))
    if len(candidates) != 1:
        raise VerificationError(
            f"expected exactly one Android NDK llvm-nm, found {len(candidates)}"
        )
    nm = candidates[0]
    readelf = nm.with_name("llvm-readelf")
    if not readelf.is_file():
        raise VerificationError(f"llvm-readelf is missing beside {nm}")
    return nm, readelf


def checksum_symbols(binding: str) -> list[str]:
    return sorted(
        set(
            re.findall(
                r"^\s*fun\s+([A-Za-z0-9_]*checksum_[A-Za-z0-9_]*)",
                binding,
                flags=re.MULTILINE,
            )
        )
    )


def verify(
    root: Path,
    output: Path,
    *,
    aar_override: Path | None = None,
    binding_override: Path | None = None,
) -> None:
    android = output / "android"
    catalog_data = catalog(root)
    android_spec = catalog_data["android"]
    artifact = catalog_data["artifact"]
    kotlin = catalog_data["kotlin"]
    aar = aar_override or (
        android / "artifacts" / f"{android_spec['artifact_id']}-{kotlin['version']}.aar"
    )
    for path in (
        aar,
        output / "nmp-native-provenance.json",
        android / "nmp-native-selection.json",
        android / "nmp-kotlin-sources.json",
    ):
        if not path.is_file():
            raise VerificationError(f"required Android output is missing: {path}")

    provenance = json.loads(
        (output / "nmp-native-provenance.json").read_text(encoding="utf-8")
    )
    selection_bytes = (android / "nmp-native-selection.json").read_bytes()
    selection = json.loads(selection_bytes)
    if provenance.get("identity") != selection.get("identity"):
        raise VerificationError("Android selection identity differs from output provenance")
    expected_abis = [item["name"] for item in android_spec["abis"]]
    if selection.get("android", {}).get("abis") != android_spec["abis"]:
        raise VerificationError("Android selection provenance differs from the catalog ABI matrix")

    source_inventory = json.loads(
        (android / "nmp-kotlin-sources.json").read_text(encoding="utf-8")
    )
    actual_destinations = {item["destination"] for item in source_inventory}
    selected_keys = set(selection["resolved_features"])
    expected_sources = list(catalog_data["core"]["kotlin_sources"])
    for feature in catalog_data["features"]:
        if feature["key"] in selected_keys:
            expected_sources.extend(feature["kotlin_sources"])
    expected_destinations = {
        item["destination"]
        for item in expected_sources
        if not item.get("platforms") or "android" in item["platforms"]
    }
    if actual_destinations != expected_destinations:
        raise VerificationError(
            "Android Kotlin source inventory differs from the resolved catalog selection"
        )
    for feature in catalog_data["features"]:
        if feature["key"] in selected_keys:
            continue
        for source in feature["kotlin_sources"]:
            destination = source["destination"]
            if destination in expected_destinations:
                continue
            if (android / "src" / "main" / "kotlin" / destination).exists():
                raise VerificationError(
                    f"unselected feature source was materialized: {destination}"
                )

    with zipfile.ZipFile(aar) as archive:
        entries = {info.filename for info in archive.infolist() if not info.is_dir()}
        expected_libraries = {
            f"jni/{abi}/lib{artifact['library_stem']}.so" for abi in expected_abis
        }
        actual_libraries = {
            entry
            for entry in entries
            if entry.startswith("jni/") and entry.endswith(".so")
        }
        if actual_libraries != expected_libraries:
            raise VerificationError(
                "AAR native library inventory differs from the exact ABI matrix: "
                f"expected={sorted(expected_libraries)} actual={sorted(actual_libraries)}"
            )
        asset = "assets/nmp/selection.json"
        if asset not in entries or archive.read(asset) != selection_bytes:
            raise VerificationError("AAR does not embed its exact feature/toolchain selection")
        try:
            classes_bytes = archive.read("classes.jar")
        except KeyError as error:
            raise VerificationError("AAR is missing classes.jar") from error
        with zipfile.ZipFile(io.BytesIO(classes_bytes)) as classes:
            class_entries = {info.filename for info in classes.infolist() if not info.is_dir()}
        for required in (
            "com/nmp/sdk/NMPEngine.class",
            "com/nmp/sdk/NMPConfig.class",
            "uniffi/nmp_ffi/NmpEngine.class",
        ):
            if required not in class_entries:
                raise VerificationError(f"AAR classes.jar is missing {required}")
        if "com/nmp/sdk/NMPSecureKeyStoreAccountStore.class" in class_entries:
            raise VerificationError("desktop JCEKS checkpoint provider leaked into Android")

        binding_path = binding_override or (
            android
            / "src"
            / "main"
            / "kotlin"
            / "uniffi"
            / artifact["library_stem"]
            / f"{artifact['library_stem']}.kt"
        )
        binding = binding_path.read_text(encoding="utf-8")
        checksums = checksum_symbols(binding)
        if "uniffiCheckApiChecksums" not in binding or not checksums:
            raise VerificationError("generated binding lacks UniFFI API checksum checks")

        sdk = os.environ.get("ANDROID_HOME") or os.environ.get("ANDROID_SDK_ROOT")
        if not sdk:
            raise VerificationError("ANDROID_HOME or ANDROID_SDK_ROOT is required")
        ndk = Path(
            os.environ.get("NMP_ANDROID_NDK_HOME")
            or os.environ.get("ANDROID_NDK_HOME")
            or Path(sdk) / "ndk" / android_spec["ndk_version"]
        )
        nm, readelf = llvm_tools(ndk)
        for abi in expected_abis:
            entry = f"jni/{abi}/lib{artifact['library_stem']}.so"
            native_bytes = archive.read(entry)
            source_library = android / "src" / "main" / "jniLibs" / abi / entry.rsplit("/", 1)[1]
            if not source_library.is_file():
                raise VerificationError(f"selected build output lacks {abi} native library")
            temporary = android / f".verify-{abi}.so"
            try:
                temporary.write_bytes(native_bytes)
                symbols = subprocess.run(
                    [str(nm), "-D", "--defined-only", str(temporary)],
                    check=True,
                    capture_output=True,
                    text=True,
                ).stdout
                names = {line.split()[-1] for line in symbols.splitlines() if line.split()}
                for required in [
                    f"ffi_{artifact['library_stem']}_uniffi_contract_version",
                    *checksums,
                ]:
                    if required not in names:
                        raise VerificationError(
                            f"{abi} native library lacks generated contract symbol {required}"
                        )
                header = subprocess.run(
                    [str(readelf), "--file-header", str(temporary)],
                    check=True,
                    capture_output=True,
                    text=True,
                ).stdout
            finally:
                temporary.unlink(missing_ok=True)
            machine_match = re.search(r"^\s*Machine:\s*(.+)$", header, re.MULTILINE)
            machine = machine_match.group(1) if machine_match else "unknown"
            if abi == "arm64-v8a" and "AArch64" not in machine:
                raise VerificationError(f"arm64-v8a contains wrong ELF machine: {machine}")
            if abi == "x86_64" and "X86-64" not in machine and "x86-64" not in machine:
                raise VerificationError(f"x86_64 contains wrong ELF machine: {machine}")

    repo_files = [path for path in (android / "repository").rglob("*") if path.is_file()]
    if not any(path.suffix == ".aar" for path in repo_files) or not any(
        path.suffix == ".pom" for path in repo_files
    ):
        raise VerificationError("generated Maven repository lacks AAR or POM metadata")

    print(
        json.dumps(
            {
                "aar": str(aar),
                "aar_sha256": sha256(aar.read_bytes()),
                "identity": selection["identity"],
                "requested_features": selection["requested_features"],
                "resolved_features": selection["resolved_features"],
                "abis": expected_abis,
                "semantic_entries": semantic_zip_inventory(aar),
            },
            sort_keys=True,
        )
    )


def compare(first: Path, second: Path) -> None:
    first_inventory = semantic_zip_inventory(first)
    second_inventory = semantic_zip_inventory(second)
    if first_inventory != second_inventory:
        raise VerificationError("AAR semantic contents are not reproducible")
    print(json.dumps({"semantic_aar_match": True, "entries": first_inventory}, sort_keys=True))


def parity(root: Path, output: Path) -> None:
    catalog_data = catalog(root)
    android_inventory = json.loads(
        (output / "android" / "nmp-kotlin-sources.json").read_text(encoding="utf-8")
    )
    jvm_inventory = json.loads(
        (output / "kotlin-jvm" / "nmp-kotlin-sources.json").read_text(encoding="utf-8")
    )
    android_destinations = {item["destination"] for item in android_inventory}
    jvm_destinations = {item["destination"] for item in jvm_inventory}
    for feature in catalog_data["features"]:
        expected = {item["destination"] for item in feature["kotlin_sources"]}
        if expected & android_destinations != expected & jvm_destinations:
            raise VerificationError(
                f"Kotlin/JVM and Android feature-wrapper inventory differs for {feature['key']}"
            )
    platform_specific = {
        item["destination"]
        for item in catalog_data["core"]["kotlin_sources"]
        if item.get("platforms")
    }
    if (android_destinations ^ jvm_destinations) - platform_specific:
        raise VerificationError(
            "Kotlin/JVM and Android differ outside declared platform-specific core sources"
        )
    print(
        json.dumps(
            {
                "feature_wrapper_parity": True,
                "android_sources": sorted(android_destinations),
                "kotlin_jvm_sources": sorted(jvm_destinations),
                "declared_platform_specific": sorted(platform_specific),
            },
            sort_keys=True,
        )
    )


def main() -> int:
    parser = argparse.ArgumentParser()
    subparsers = parser.add_subparsers(dest="command", required=True)
    verify_parser = subparsers.add_parser("verify")
    verify_parser.add_argument("--repo", type=Path, required=True)
    verify_parser.add_argument("--output", type=Path, required=True)
    verify_parser.add_argument("--aar", type=Path)
    verify_parser.add_argument("--binding", type=Path)
    compare_parser = subparsers.add_parser("compare")
    compare_parser.add_argument("first", type=Path)
    compare_parser.add_argument("second", type=Path)
    parity_parser = subparsers.add_parser("parity")
    parity_parser.add_argument("--repo", type=Path, required=True)
    parity_parser.add_argument("--output", type=Path, required=True)
    args = parser.parse_args()
    try:
        if args.command == "verify":
            verify(
                args.repo.resolve(),
                args.output.resolve(),
                aar_override=args.aar.resolve() if args.aar else None,
                binding_override=args.binding.resolve() if args.binding else None,
            )
        elif args.command == "compare":
            compare(args.first.resolve(), args.second.resolve())
        else:
            parity(args.repo.resolve(), args.output.resolve())
    except (OSError, KeyError, ValueError, subprocess.CalledProcessError, VerificationError) as error:
        print(f"verify-android-aar: error: {error}", file=sys.stderr)
        return 1
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
