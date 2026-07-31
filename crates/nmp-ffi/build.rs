#[path = "../nmp-component-interface/component_identity.rs"]
mod component_identity;

use std::env;
use std::fs;
use std::path::{Path, PathBuf};

const IDENTITY_VERSION: &str = "nmp-core-component-v2";
const COMPONENT_KEY: &str = "nmp-core";
const CARGO_PACKAGE: &str = "nmp-ffi";
const LIBRARY_STEM: &str = "nmp_ffi";
const UNIFFI_NAMESPACE: &str = "nmp_ffi";

fn main() {
    for variable in [
        "CARGO_ENCODED_RUSTFLAGS",
        "RUSTFLAGS",
        "CARGO_BUILD_RUSTFLAGS",
        "RUSTC",
        "NMP_COMPONENT_BUILD_AUTH",
        "NMP_COMPONENT_BUILD_ROOT",
        "NMP_COMPONENT_MANIFEST_OUTPUT",
    ] {
        println!("cargo:rerun-if-env-changed={variable}");
    }
    let manifest_dir = PathBuf::from(
        env::var_os("CARGO_MANIFEST_DIR").expect("Cargo supplies CARGO_MANIFEST_DIR"),
    );
    let workspace = manifest_dir
        .parent()
        .and_then(Path::parent)
        .expect("nmp-ffi remains under <workspace>/crates");
    let target = env::var("TARGET").expect("Cargo supplies TARGET");
    let profile = env::var("PROFILE").expect("Cargo supplies PROFILE");
    let cargo = PathBuf::from(env::var_os("CARGO").expect("Cargo supplies CARGO"));
    let rustc = PathBuf::from(env::var_os("RUSTC").expect("Cargo supplies RUSTC"));
    let interface_identity = env::var("DEP_NMP_COMPONENT_INTERFACE_INTERFACE_IDENTITY")
        .expect("component-interface build script supplies its complete identity");
    let computed = component_identity::compute_component_identity(
        workspace,
        &cargo,
        &rustc,
        &component_identity::IdentitySpec {
            version: IDENTITY_VERSION,
            component_key: COMPONENT_KEY,
            cargo_package: CARGO_PACKAGE,
            target: &target,
            profile: &profile,
            interface_identity: Some(&interface_identity),
            required_core_identity: None,
            forbidden_packages: &["nmp-nip46", "nmp-nip46-ffi"],
        },
    )
    .unwrap_or_else(|error| panic!("compute standalone core identity: {error}"));

    println!(
        "cargo:rustc-env=NMP_CORE_COMPONENT_IDENTITY={}",
        computed.identity
    );
    println!("cargo:rustc-check-cfg=cfg(nmp_component_release_attestation)");
    if profile == "release" {
        validate_release_context(&target);
        write_manifest(&target, &interface_identity, &computed);
        write_attestation(&target, &interface_identity, &computed);
        println!("cargo:rustc-cfg=nmp_component_release_attestation");
    }
}

fn validate_release_context(target: &str) {
    let out_dir = PathBuf::from(env::var_os("OUT_DIR").expect("Cargo supplies OUT_DIR"))
        .canonicalize()
        .expect("Cargo OUT_DIR must exist");
    let root = PathBuf::from(
        env::var_os("NMP_COMPONENT_BUILD_ROOT")
            .expect("release native components require the managed builder"),
    )
    .canonicalize()
    .expect("managed component root must exist");
    assert!(
        root.file_name().and_then(|name| name.to_str()) == Some(COMPONENT_KEY)
            && root
                .parent()
                .and_then(Path::file_name)
                .and_then(|name| name.to_str())
                == Some("nmp-component-build-v2"),
        "release component root is not the standalone core target"
    );
    component_identity::validate_release_out_dir(&root, &out_dir, target, CARGO_PACKAGE)
        .unwrap_or_else(|error| panic!("{error}"));
    let marker_dir = root.join(".nmp-component-build-v2");
    let marker = marker_dir.join(target);
    let expected = format!(
        "nmp-component-build-v2\ncomponent-key={COMPONENT_KEY}\ncargo-package={CARGO_PACKAGE}\ntarget={target}\nprofile=release\n"
    );
    let actual = fs::read_to_string(&marker)
        .unwrap_or_else(|_| panic!("release component target has no managed marker"));
    assert_eq!(actual, expected, "release component marker disagrees");
    let authorization = marker_dir.join(".authorization");
    let expected_auth = fs::read_to_string(&authorization)
        .unwrap_or_else(|_| panic!("release component target has no live authorization"));
    assert_eq!(
        env::var("NMP_COMPONENT_BUILD_AUTH").as_deref(),
        Ok(expected_auth.trim()),
        "release component authorization does not match its isolated target"
    );
}

fn write_manifest(
    target: &str,
    interface_identity: &str,
    computed: &component_identity::ComputedIdentity,
) {
    let output = PathBuf::from(
        env::var_os("NMP_COMPONENT_MANIFEST_OUTPUT")
            .expect("managed release build supplies manifest output"),
    );
    let value = serde_json::json!({
        "attestation_symbol": "NMP_CORE_COMPONENT_ATTESTATION_V2",
        "binding_identity": computed.identity,
        "build_flags_digest": computed.flags_digest,
        "cargo_package": CARGO_PACKAGE,
        "component_key": COMPONENT_KEY,
        "graph_digest": computed.graph_digest,
        "identity": computed.identity,
        "interface_identity": interface_identity,
        "kind": "core",
        "library_stem": LIBRARY_STEM,
        "native_identity": computed.identity,
        "profile": "release",
        "rustc_digest": computed.rustc_digest,
        "schema": 2,
        "target": target,
        "uniffi_namespace": UNIFFI_NAMESPACE,
    });
    let mut bytes = serde_json::to_vec(&value).expect("serialize canonical core manifest");
    bytes.push(b'\n');
    let temporary = output.with_extension("json.tmp");
    fs::write(&temporary, bytes).expect("write temporary core manifest");
    fs::rename(&temporary, &output).expect("publish core manifest atomically");
}

fn write_attestation(
    target: &str,
    interface_identity: &str,
    computed: &component_identity::ComputedIdentity,
) {
    let value = serde_json::json!({
        "build_flags_digest": computed.flags_digest,
        "cargo_package": CARGO_PACKAGE,
        "component_key": COMPONENT_KEY,
        "graph_digest": computed.graph_digest,
        "identity": computed.identity,
        "interface_identity": interface_identity,
        "kind": "core",
        "library_stem": LIBRARY_STEM,
        "profile": "release",
        "rustc_digest": computed.rustc_digest,
        "schema": 1,
        "target": target,
        "uniffi_namespace": UNIFFI_NAMESPACE,
    });
    let payload = serde_json::to_vec(&value).expect("serialize canonical core attestation");
    let payload_length =
        u32::try_from(payload.len()).expect("core attestation payload fits in a u32");
    let mut record = b"NMPATT01".to_vec();
    record.extend_from_slice(&payload_length.to_le_bytes());
    record.extend_from_slice(&payload);
    write_attestation_source("NMP_CORE_COMPONENT_ATTESTATION_V2", &record);
}

fn write_attestation_source(symbol: &str, record: &[u8]) {
    let output = PathBuf::from(env::var_os("OUT_DIR").expect("Cargo supplies OUT_DIR"))
        .join("component_attestation.rs");
    let bytes = record
        .iter()
        .map(|byte| format!("0x{byte:02x}"))
        .collect::<Vec<_>>()
        .join(",");
    let source = format!(
        "#[used]\n#[no_mangle]\npub static {symbol}: [u8; {}] = [{bytes}];\n",
        record.len()
    );
    let temporary = output.with_extension("rs.tmp");
    fs::write(&temporary, source).expect("write temporary core attestation source");
    fs::rename(&temporary, &output).expect("publish core attestation source atomically");
}
