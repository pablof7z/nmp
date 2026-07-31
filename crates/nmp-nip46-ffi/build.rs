#[path = "../nmp-component-interface/component_identity.rs"]
mod component_identity;

use std::env;
use std::fs;
use std::path::{Path, PathBuf};

const IDENTITY_VERSION: &str = "nmp-nip46-component-v2";
const COMPONENT_KEY: &str = "nmp-nip46";
const CARGO_PACKAGE: &str = "nmp-nip46-ffi";
const LIBRARY_STEM: &str = "nmp_nip46_ffi";
const UNIFFI_NAMESPACE: &str = "nmp_nip46_ffi";

struct CoreManifest {
    identity: String,
    rustc_digest: String,
    flags_digest: String,
    interface_dependency_digest: String,
    artifact_blake3: Option<String>,
    manifest_blake3: Option<String>,
}

fn main() {
    for variable in [
        "CARGO_ENCODED_RUSTFLAGS",
        "RUSTFLAGS",
        "CARGO_BUILD_RUSTFLAGS",
        "RUSTC",
        "NMP_COMPONENT_BUILD_AUTH",
        "NMP_COMPONENT_BUILD_ROOT",
        "NMP_COMPONENT_CORE_ARTIFACT",
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
        .expect("provider remains under <workspace>/crates");
    let target = env::var("TARGET").expect("Cargo supplies TARGET");
    let profile = env::var("PROFILE").expect("Cargo supplies PROFILE");
    let cargo = PathBuf::from(env::var_os("CARGO").expect("Cargo supplies CARGO"));
    let rustc = PathBuf::from(env::var_os("RUSTC").expect("Cargo supplies RUSTC"));
    let interface_identity = env::var("DEP_NMP_COMPONENT_INTERFACE_INTERFACE_IDENTITY")
        .expect("component-interface supplies its complete identity");

    if target.contains("-linux-") {
        // The optional ELF provider links the shared interface as an rlib, but
        // core alone owns that interface's public UniFFI namespace. Hide every
        // archive-owned dependency symbol at link time; the provider crate's
        // own direct objects (including its NIP-46 UniFFI surface and
        // attestation) remain exportable. The final artifact witness derives
        // the exact forbidden interface set from the paired core and refuses
        // the package if any member still escapes.
        println!("cargo:rustc-link-arg-cdylib=-Wl,--exclude-libs,ALL");
    }

    let core = if profile == "release" {
        validate_release_context(&target);
        required_core_from_artifact(&target, &interface_identity)
    } else {
        let computed = component_identity::compute_component_identity(
            workspace,
            &cargo,
            &rustc,
            &component_identity::IdentitySpec {
                version: "nmp-core-component-v2",
                component_key: "nmp-core",
                cargo_package: "nmp-ffi",
                target: &target,
                profile: &profile,
                interface_identity: Some(&interface_identity),
                required_core_identity: None,
                forbidden_packages: &["nmp-nip46", "nmp-nip46-ffi"],
            },
        )
        .unwrap_or_else(|error| panic!("compute debug core identity: {error}"));
        CoreManifest {
            identity: computed.identity,
            rustc_digest: computed.rustc_digest,
            flags_digest: computed.flags_digest,
            interface_dependency_digest: computed.interface_dependency_digest,
            artifact_blake3: None,
            manifest_blake3: None,
        }
    };

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
            required_core_identity: Some(&core.identity),
            forbidden_packages: &["nmp-ffi", "nmp"],
        },
    )
    .unwrap_or_else(|error| panic!("compute NIP-46 component identity: {error}"));
    assert_eq!(
        computed.rustc_digest, core.rustc_digest,
        "optional component compiler identity disagrees with sealed core"
    );
    assert_eq!(
        computed.flags_digest, core.flags_digest,
        "optional component flags disagree with sealed core"
    );
    // The core and this provider are separate Cargo resolutions under separate
    // target directories, so each links its own compilation of Tokio and of
    // every other crate the shared interface declares. Their identities are
    // computed from isolated graphs and can therefore agree while the two
    // builds disagree about, say, Tokio's feature set -- which changes the
    // layout of the `tokio::runtime::Handle` and `tokio::sync` channels the
    // interface moves across the seam. Nothing else in this build compares
    // that, so this is the only place it can be caught.
    assert!(
        component_identity::is_digest(&core.interface_dependency_digest),
        "sealed core has no usable interface dependency digest: {:?}",
        core.interface_dependency_digest
    );
    assert_eq!(
        computed.interface_dependency_digest,
        core.interface_dependency_digest,
        "the shared component interface resolved differently in this provider than in the \
         core it is paired with: interface_dependency_digest {} (provider) != {} (core). \
         Values crossing the seam would have two layouts. This provider resolved:\n{}\n\
         Rebuild the core and compare, or add the missing feature to the tokio dependency \
         in crates/nmp-component-interface/Cargo.toml so both graphs resolve one Tokio.",
        computed.interface_dependency_digest,
        core.interface_dependency_digest,
        computed.interface_dependency_summary
    );

    println!(
        "cargo:rustc-env=NMP_NIP46_COMPONENT_IDENTITY={}",
        computed.identity
    );
    println!(
        "cargo:rustc-env=NMP_NIP46_REQUIRED_CORE_IDENTITY={}",
        core.identity
    );
    println!("cargo:rustc-check-cfg=cfg(nmp_component_release_attestation)");
    if profile == "release" {
        write_manifest(&target, &interface_identity, &core, &computed);
        write_attestation(&target, &interface_identity, &core, &computed);
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
        "release component root is not the standalone NIP-46 target"
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

fn required_core_from_artifact(target: &str, interface_identity: &str) -> CoreManifest {
    let path = PathBuf::from(
        env::var_os("NMP_COMPONENT_CORE_ARTIFACT")
            .expect("optional release build requires a sealed core artifact"),
    )
    .canonicalize()
    .expect("sealed core artifact must exist");
    println!("cargo:rerun-if-changed={}", path.display());
    assert!(
        fs::metadata(&path)
            .expect("stat sealed core artifact")
            .is_file(),
        "optional release build requires a regular sealed core artifact"
    );
    assert!(
        fs::metadata(&path)
            .expect("stat sealed core artifact")
            .permissions()
            .readonly(),
        "optional release build requires a read-only sealed core artifact"
    );
    let artifact_bytes = fs::read(&path).expect("read sealed core artifact");
    let artifact_blake3 = blake3::hash(&artifact_bytes).to_hex().to_string();
    let parent = path
        .parent()
        .expect("sealed core artifact must have a parent directory");
    let manifest_path = parent.join("component-manifest.json");
    let artifact_name = path
        .file_name()
        .and_then(|name| name.to_str())
        .expect("sealed core artifact name must be UTF-8");
    let witness_path = parent.join(format!("{artifact_name}.witness.json"));
    println!("cargo:rerun-if-changed={}", manifest_path.display());
    println!("cargo:rerun-if-changed={}", witness_path.display());

    let (manifest_bytes, value) =
        read_canonical_readonly_json(&manifest_path, "sealed core manifest");
    let fields = value.as_object().expect("core manifest must be an object");
    let expected_fields = [
        "attestation_symbol",
        "binding_identity",
        "build_flags_digest",
        "cargo_package",
        "component_key",
        "graph_digest",
        "identity",
        "interface_dependency_digest",
        "interface_identity",
        "kind",
        "library_stem",
        "native_identity",
        "profile",
        "rustc_digest",
        "schema",
        "target",
        "uniffi_namespace",
    ]
    .into_iter()
    .collect::<std::collections::BTreeSet<_>>();
    assert_eq!(
        fields
            .keys()
            .map(String::as_str)
            .collect::<std::collections::BTreeSet<_>>(),
        expected_fields,
        "core manifest has an unknown or missing field"
    );
    let field = |name: &str| {
        value
            .get(name)
            .and_then(serde_json::Value::as_str)
            .unwrap_or_else(|| panic!("core manifest has no string {name}"))
    };
    assert_eq!(
        value.get("schema").and_then(serde_json::Value::as_u64),
        Some(2)
    );
    assert_eq!(field("kind"), "core");
    assert_eq!(field("component_key"), "nmp-core");
    assert_eq!(field("target"), target);
    assert_eq!(field("profile"), "release");
    assert_eq!(
        field("attestation_symbol"),
        "NMP_CORE_COMPONENT_ATTESTATION_V2"
    );
    assert_eq!(field("cargo_package"), "nmp-ffi");
    assert_eq!(field("library_stem"), "nmp_ffi");
    assert_eq!(field("uniffi_namespace"), "nmp_ffi");
    assert_eq!(field("interface_identity"), interface_identity);
    assert_eq!(field("binding_identity"), field("identity"));
    assert_eq!(field("native_identity"), field("identity"));
    let identity = field("identity").to_owned();
    let rustc_digest = field("rustc_digest").to_owned();
    let flags_digest = field("build_flags_digest").to_owned();
    let interface_dependency_digest = field("interface_dependency_digest").to_owned();

    let (_, witness) = read_canonical_readonly_json(&witness_path, "sealed core witness");
    let witness_fields = witness.as_object().expect("core witness must be an object");
    let expected_witness_fields = [
        "architecture",
        "artifact_blake3",
        "artifact_size",
        "attestation",
        "component_key",
        "format",
        "public_symbols",
        "schema",
        "target",
        "uniffi_components",
    ]
    .into_iter()
    .collect::<std::collections::BTreeSet<_>>();
    assert_eq!(
        witness_fields
            .keys()
            .map(String::as_str)
            .collect::<std::collections::BTreeSet<_>>(),
        expected_witness_fields,
        "core witness has an unknown or missing field"
    );
    let witness_field = |name: &str| {
        witness
            .get(name)
            .and_then(serde_json::Value::as_str)
            .unwrap_or_else(|| panic!("core witness has no string {name}"))
    };
    assert_eq!(
        witness.get("schema").and_then(serde_json::Value::as_u64),
        Some(1)
    );
    assert_eq!(witness_field("component_key"), "nmp-core");
    assert_eq!(witness_field("target"), target);
    assert_eq!(witness_field("artifact_blake3"), artifact_blake3);
    assert_eq!(
        witness
            .get("artifact_size")
            .and_then(serde_json::Value::as_u64),
        Some(artifact_bytes.len() as u64),
        "core witness artifact size disagrees with sealed bytes"
    );
    assert!(
        witness
            .get("public_symbols")
            .and_then(serde_json::Value::as_array)
            .is_some(),
        "core witness public_symbols must be an array"
    );
    assert!(
        witness
            .get("uniffi_components")
            .and_then(serde_json::Value::as_array)
            .is_some(),
        "core witness uniffi_components must be an array"
    );

    let attestation = witness
        .get("attestation")
        .and_then(serde_json::Value::as_object)
        .expect("core witness attestation must be an object");
    let expected_attestation_fields = [
        "build_flags_digest",
        "cargo_package",
        "component_key",
        "graph_digest",
        "identity",
        "interface_dependency_digest",
        "interface_identity",
        "kind",
        "library_stem",
        "profile",
        "rustc_digest",
        "schema",
        "target",
        "uniffi_namespace",
    ]
    .into_iter()
    .collect::<std::collections::BTreeSet<_>>();
    assert_eq!(
        attestation
            .keys()
            .map(String::as_str)
            .collect::<std::collections::BTreeSet<_>>(),
        expected_attestation_fields,
        "core witness attestation has an unknown or missing field"
    );
    let attestation_field = |name: &str| {
        attestation
            .get(name)
            .and_then(serde_json::Value::as_str)
            .unwrap_or_else(|| panic!("core witness attestation has no string {name}"))
    };
    assert_eq!(
        attestation
            .get("schema")
            .and_then(serde_json::Value::as_u64),
        Some(1)
    );
    assert_eq!(attestation_field("kind"), "core");
    assert_eq!(attestation_field("component_key"), "nmp-core");
    assert_eq!(attestation_field("identity"), identity);
    assert_eq!(attestation_field("interface_identity"), interface_identity);
    for name in [
        "build_flags_digest",
        "cargo_package",
        "graph_digest",
        "interface_dependency_digest",
        "library_stem",
        "profile",
        "rustc_digest",
        "target",
        "uniffi_namespace",
    ] {
        assert_eq!(
            attestation_field(name),
            field(name),
            "core witness attestation {name} disagrees with manifest"
        );
    }

    CoreManifest {
        identity,
        rustc_digest,
        flags_digest,
        interface_dependency_digest,
        artifact_blake3: Some(artifact_blake3),
        manifest_blake3: Some(blake3::hash(&manifest_bytes).to_hex().to_string()),
    }
}

fn read_canonical_readonly_json(path: &Path, label: &str) -> (Vec<u8>, serde_json::Value) {
    let metadata = fs::metadata(path).unwrap_or_else(|_| panic!("stat {label}"));
    assert!(metadata.is_file(), "{label} must be a regular file");
    assert!(
        metadata.permissions().readonly(),
        "{label} must be read-only"
    );
    let bytes = fs::read(path).unwrap_or_else(|_| panic!("read {label}"));
    let value: serde_json::Value =
        serde_json::from_slice(&bytes).unwrap_or_else(|_| panic!("parse {label}"));
    let mut canonical =
        serde_json::to_vec(&value).unwrap_or_else(|_| panic!("canonicalize {label}"));
    canonical.push(b'\n');
    assert_eq!(
        bytes, canonical,
        "{label} is not canonical JSON with one trailing newline"
    );
    (bytes, value)
}

fn write_manifest(
    target: &str,
    interface_identity: &str,
    core: &CoreManifest,
    computed: &component_identity::ComputedIdentity,
) {
    let output = PathBuf::from(
        env::var_os("NMP_COMPONENT_MANIFEST_OUTPUT")
            .expect("managed release build supplies manifest output"),
    );
    let value = serde_json::json!({
        "attestation_symbol": "NMP_NIP46_COMPONENT_ATTESTATION_V2",
        "binding_identity": computed.identity,
        "build_flags_digest": computed.flags_digest,
        "cargo_package": CARGO_PACKAGE,
        "component_key": COMPONENT_KEY,
        "graph_digest": computed.graph_digest,
        "identity": computed.identity,
        "interface_dependency_digest": computed.interface_dependency_digest,
        "interface_identity": interface_identity,
        "kind": "optional",
        "library_stem": LIBRARY_STEM,
        "native_identity": computed.identity,
        "profile": "release",
        "required_core_artifact_blake3": core.artifact_blake3.as_deref()
            .expect("release core has an artifact digest"),
        "required_core_identity": core.identity,
        "required_core_manifest_blake3": core.manifest_blake3.as_deref()
            .expect("release core has a manifest digest"),
        "rustc_digest": computed.rustc_digest,
        "schema": 2,
        "target": target,
        "uniffi_namespace": UNIFFI_NAMESPACE,
    });
    let mut bytes = serde_json::to_vec(&value).expect("serialize canonical provider manifest");
    bytes.push(b'\n');
    let temporary = output.with_extension("json.tmp");
    fs::write(&temporary, bytes).expect("write temporary provider manifest");
    fs::rename(&temporary, &output).expect("publish provider manifest atomically");
}

fn write_attestation(
    target: &str,
    interface_identity: &str,
    core: &CoreManifest,
    computed: &component_identity::ComputedIdentity,
) {
    let value = serde_json::json!({
        "build_flags_digest": computed.flags_digest,
        "cargo_package": CARGO_PACKAGE,
        "component_key": COMPONENT_KEY,
        "graph_digest": computed.graph_digest,
        "identity": computed.identity,
        "interface_dependency_digest": computed.interface_dependency_digest,
        "interface_identity": interface_identity,
        "kind": "optional",
        "library_stem": LIBRARY_STEM,
        "profile": "release",
        "required_core_artifact_blake3": core.artifact_blake3.as_deref()
            .expect("release core has an artifact digest"),
        "required_core_identity": core.identity,
        "required_core_manifest_blake3": core.manifest_blake3.as_deref()
            .expect("release core has a manifest digest"),
        "rustc_digest": computed.rustc_digest,
        "schema": 1,
        "target": target,
        "uniffi_namespace": UNIFFI_NAMESPACE,
    });
    let payload = serde_json::to_vec(&value).expect("serialize canonical provider attestation");
    let payload_length =
        u32::try_from(payload.len()).expect("provider attestation payload fits in a u32");
    let mut record = b"NMPATT01".to_vec();
    record.extend_from_slice(&payload_length.to_le_bytes());
    record.extend_from_slice(&payload);
    write_attestation_source("NMP_NIP46_COMPONENT_ATTESTATION_V2", &record);
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
    fs::write(&temporary, source).expect("write temporary provider attestation source");
    fs::rename(&temporary, &output).expect("publish provider attestation source atomically");
}
