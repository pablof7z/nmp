#[path = "component_identity.rs"]
#[allow(dead_code)]
mod component_identity;

use std::env;
use std::path::{Path, PathBuf};

const IDENTITY_VERSION: &str = "nmp-component-interface-v2";

fn main() {
    for variable in [
        "CARGO_ENCODED_RUSTFLAGS",
        "RUSTFLAGS",
        "CARGO_BUILD_RUSTFLAGS",
        "RUSTC",
    ] {
        println!("cargo:rerun-if-env-changed={variable}");
    }
    let manifest_dir = PathBuf::from(
        env::var_os("CARGO_MANIFEST_DIR").expect("Cargo supplies CARGO_MANIFEST_DIR"),
    );
    let workspace = manifest_dir
        .parent()
        .and_then(Path::parent)
        .expect("component interface remains under <workspace>/crates");
    let cargo = PathBuf::from(env::var_os("CARGO").expect("Cargo supplies CARGO"));
    let rustc = PathBuf::from(env::var_os("RUSTC").expect("Cargo supplies RUSTC"));
    let target = env::var("TARGET").expect("Cargo supplies TARGET");
    let profile = env::var("PROFILE").expect("Cargo supplies PROFILE");
    let computed = component_identity::compute_component_identity(
        workspace,
        &cargo,
        &rustc,
        &component_identity::IdentitySpec {
            version: IDENTITY_VERSION,
            component_key: "nmp-component-interface",
            cargo_package: "nmp-component-interface",
            target: &target,
            profile: &profile,
            interface_identity: None,
            required_core_identity: None,
            forbidden_packages: &["nmp", "nmp-ffi", "nmp-nip46", "nmp-nip46-ffi"],
        },
    )
    .unwrap_or_else(|error| panic!("compute complete component-interface identity: {error}"));

    rerun_local_sources(workspace, &manifest_dir);
    println!(
        "cargo:rustc-env=NMP_COMPONENT_INTERFACE_IDENTITY={}",
        computed.identity
    );
    println!("cargo:interface_identity={}", computed.identity);
}

fn rerun_local_sources(workspace: &Path, manifest_dir: &Path) {
    println!("cargo:rerun-if-changed={}", manifest_dir.display());
    println!(
        "cargo:rerun-if-changed={}",
        workspace.join("Cargo.lock").display()
    );
}
