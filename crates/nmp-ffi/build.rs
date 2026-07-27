mod component_identity;

use std::env;
use std::ffi::OsStr;
use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;

const IDENTITY_VERSION: &str = "nmp-core-component-v1";
const PROVIDER_ONLY_CRATES: &[&str] = &["nmp-nip46", "nmp-nip46-ffi"];

fn main() {
    let manifest_dir = PathBuf::from(
        env::var_os("CARGO_MANIFEST_DIR").expect("Cargo supplies CARGO_MANIFEST_DIR"),
    );
    let workspace = manifest_dir
        .parent()
        .and_then(Path::parent)
        .expect("nmp-ffi remains under <workspace>/crates/nmp-ffi");

    let mut hasher = blake3::Hasher::new();
    add_field(&mut hasher, "identity-version", IDENTITY_VERSION.as_bytes());
    add_field(
        &mut hasher,
        "target",
        env::var("TARGET")
            .expect("Cargo supplies TARGET")
            .as_bytes(),
    );
    add_field(
        &mut hasher,
        "profile",
        env::var("PROFILE")
            .expect("Cargo supplies PROFILE")
            .as_bytes(),
    );
    add_field(
        &mut hasher,
        "cargo-packages",
        normalized_component_packages().as_bytes(),
    );
    hash_cargo_unit_graph(workspace, &mut hasher);
    for variable in ["CARGO_ENCODED_RUSTFLAGS", "RUSTFLAGS"] {
        println!("cargo:rerun-if-env-changed={variable}");
        if let Ok(value) = env::var(variable) {
            add_field(&mut hasher, variable, value.as_bytes());
        }
    }

    let rustc = env::var_os("RUSTC").expect("Cargo supplies RUSTC");
    let rustc_version = Command::new(rustc)
        .args(["--version", "--verbose"])
        .output()
        .expect("rustc --version --verbose must run");
    if !rustc_version.status.success() {
        panic!("rustc --version --verbose failed");
    }
    add_field(&mut hasher, "rustc", &rustc_version.stdout);

    let mut cargo_features = env::vars()
        .filter(|(key, _)| key.starts_with("CARGO_FEATURE_"))
        .collect::<Vec<_>>();
    cargo_features.sort();
    for (key, value) in cargo_features {
        add_field(&mut hasher, &key, value.as_bytes());
    }

    for relative in ["Cargo.toml", "Cargo.lock", "rust-toolchain.toml"] {
        hash_file(workspace, &workspace.join(relative), &mut hasher);
    }

    let crates_dir = workspace.join("crates");
    println!("cargo:rerun-if-changed={}", crates_dir.display());
    let mut crate_dirs = fs::read_dir(&crates_dir)
        .expect("workspace crates directory exists")
        .map(|entry| entry.expect("crate directory entry is readable").path())
        .filter(|path| path.is_dir())
        .filter(|path| {
            path.file_name()
                .and_then(OsStr::to_str)
                .is_some_and(|name| !PROVIDER_ONLY_CRATES.contains(&name))
        })
        .collect::<Vec<_>>();
    crate_dirs.sort();
    for crate_dir in crate_dirs {
        hash_source_tree(workspace, &crate_dir, &mut hasher);
    }

    println!("cargo:rerun-if-env-changed=RUSTC");
    println!(
        "cargo:rustc-env=NMP_CORE_COMPONENT_IDENTITY={IDENTITY_VERSION}-{}",
        hasher.finalize().to_hex()
    );
}

fn normalized_component_packages() -> String {
    println!("cargo:rerun-if-env-changed=NMP_FFI_CARGO_PACKAGES");
    let mut packages = env::var("NMP_FFI_CARGO_PACKAGES")
        .unwrap_or_else(|_| "nmp-ffi".to_owned())
        .split_whitespace()
        .map(str::to_owned)
        .collect::<Vec<_>>();
    packages.sort();
    packages.dedup();
    assert!(
        packages.iter().any(|package| package == "nmp-ffi"),
        "NMP_FFI_CARGO_PACKAGES must include nmp-ffi"
    );
    packages.join(" ")
}

fn hash_cargo_unit_graph(workspace: &Path, hasher: &mut blake3::Hasher) {
    println!("cargo:rerun-if-env-changed=NMP_FFI_CARGO_UNIT_GRAPH");
    let Ok(path) = env::var("NMP_FFI_CARGO_UNIT_GRAPH") else {
        add_field(hasher, "cargo-unit-graph", b"development-unresolved");
        return;
    };
    let path = PathBuf::from(path);
    println!("cargo:rerun-if-changed={}", path.display());
    let bytes = fs::read(&path).unwrap_or_else(|error| panic!("read {}: {error}", path.display()));
    let mut graph: serde_json::Value = serde_json::from_slice(&bytes)
        .unwrap_or_else(|error| panic!("parse {}: {error}", path.display()));
    component_identity::canonicalize_unit_graph(&mut graph, workspace);
    let canonical = serde_json::to_vec(&graph)
        .unwrap_or_else(|error| panic!("serialize canonical Cargo unit graph: {error}"));
    add_field(hasher, "cargo-unit-graph", &canonical);
}

fn hash_source_tree(workspace: &Path, directory: &Path, hasher: &mut blake3::Hasher) {
    println!("cargo:rerun-if-changed={}", directory.display());
    let mut entries = fs::read_dir(directory)
        .unwrap_or_else(|error| panic!("read {}: {error}", directory.display()))
        .map(|entry| {
            entry
                .unwrap_or_else(|error| panic!("read entry in {}: {error}", directory.display()))
                .path()
        })
        .collect::<Vec<_>>();
    entries.sort();

    for path in entries {
        if path.is_dir() {
            hash_source_tree(workspace, &path, hasher);
            continue;
        }
        let include = path
            .file_name()
            .and_then(OsStr::to_str)
            .is_some_and(|name| name == "Cargo.toml" || name == "build.rs")
            || path
                .extension()
                .and_then(OsStr::to_str)
                .is_some_and(|extension| extension == "rs");
        if include {
            hash_file(workspace, &path, hasher);
        }
    }
}

fn hash_file(workspace: &Path, path: &Path, hasher: &mut blake3::Hasher) {
    println!("cargo:rerun-if-changed={}", path.display());
    let relative = path
        .strip_prefix(workspace)
        .unwrap_or_else(|_| panic!("{} is inside {}", path.display(), workspace.display()))
        .to_string_lossy()
        .replace('\\', "/");
    let bytes = fs::read(path).unwrap_or_else(|error| panic!("read {}: {error}", path.display()));
    add_field(hasher, &relative, &bytes);
}

fn add_field(hasher: &mut blake3::Hasher, name: &str, value: &[u8]) {
    hasher.update(&(name.len() as u64).to_le_bytes());
    hasher.update(name.as_bytes());
    hasher.update(&(value.len() as u64).to_le_bytes());
    hasher.update(value);
}
