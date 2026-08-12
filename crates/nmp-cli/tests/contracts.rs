use nmp_cli::{
    filter_source, verify_prepared_product, AppManifest, Catalog, CommandOutput, CommandRunner,
    Error, PrepareOptions, Preparer, Product,
};
use sha2::Digest;
use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};
use std::process::Command;

fn repository() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .and_then(Path::parent)
        .unwrap()
        .to_path_buf()
}

fn catalog() -> Catalog {
    let root = repository();
    Catalog::load(&root.join("native/features.toml"), &root).unwrap()
}

#[test]
fn manifest_is_canonical_and_runtime_fields_are_refused() {
    let temp = tempfile::tempdir().unwrap();
    let path = temp.path().join(".nmp.toml");
    std::fs::write(
        &path,
        "schema = 1\ncapabilities = [\"nip65\", \"nip29\"]\nproducts = [\"apple\"]\n",
    )
    .unwrap();
    let manifest = AppManifest::load(&path).unwrap();
    assert_eq!(manifest.capabilities, ["nip29", "nip65"]);
    std::fs::write(
        &path,
        "schema = 1\ncapabilities = []\nproducts = [\"apple\"]\nrelays = [\"wss://example.test\"]\n",
    )
    .unwrap();
    assert!(AppManifest::load(&path)
        .unwrap_err()
        .to_string()
        .contains("unknown field"));
    std::fs::write(&path, "schema = 1\nfeatures = []\nproducts = [\"apple\"]\n").unwrap();
    assert!(AppManifest::load(&path)
        .unwrap_err()
        .to_string()
        .contains("unknown field"));
    std::fs::write(
        &path,
        "schema = 1\ncapabilities = [\"not-a-capability\"]\nproducts = [\"apple\"]\n",
    )
    .unwrap();
    assert!(AppManifest::load(&path)
        .unwrap()
        .validate_capabilities(&catalog())
        .unwrap_err()
        .to_string()
        .contains("unknown or internal-only capabilities"));
    std::fs::write(
        &path,
        "schema = 999\ncapabilities = []\nproducts = [\"apple\"]\n",
    )
    .unwrap();
    assert!(AppManifest::load(&path)
        .unwrap_err()
        .to_string()
        .contains("unsupported schema"));
    std::fs::write(&path, "schema = [\n").unwrap();
    assert!(AppManifest::load(&path)
        .unwrap_err()
        .to_string()
        .contains("invalid app manifest"));
    std::fs::remove_file(&path).unwrap();
    assert!(AppManifest::load(&path)
        .unwrap_err()
        .to_string()
        .contains("cannot read"));
}

#[test]
fn capability_language_resolves_to_catalog_keys_without_builder_branches() {
    let catalog = catalog();
    assert_eq!(
        AppManifest::resolve_capability(&catalog, "groups").unwrap(),
        "nip29"
    );
    assert_eq!(
        AppManifest::resolve_capability(&catalog, "outbox routing").unwrap(),
        "nip65"
    );
    let root = repository();
    let source = std::fs::read_to_string(root.join("crates/nmp-cli/src/prepare.rs")).unwrap();
    for feature in &catalog.features {
        assert!(
            !source.contains(&format!("\"{}\"", feature.key)),
            "builder hard-codes {}",
            feature.key
        );
    }
}

#[test]
fn source_filter_keeps_only_selected_capability_blocks() {
    let selected = BTreeSet::from(["alpha".to_owned()]);
    let known = BTreeSet::from(["alpha".to_owned(), "beta".to_owned()]);
    let source = "core\n// nmp-native:if alpha\nalpha\n// nmp-native:if beta\nboth\n// nmp-native:endif\n// nmp-native:endif\n// nmp-native:if beta\nbeta\n// nmp-native:endif\n";
    assert_eq!(
        filter_source(source, &selected, &known, "fixture").unwrap(),
        "core\nalpha\n"
    );
}

#[test]
fn init_and_capability_edit_use_dot_nmp_toml() {
    let temp = tempfile::tempdir().unwrap();
    let manifest = temp.path().join(".nmp.toml");
    let binary = env!("CARGO_BIN_EXE_nmp");
    let root = repository();
    let run = |args: &[&str]| {
        let status = Command::new(binary)
            .arg("--source")
            .arg(&root)
            .arg("--manifest")
            .arg(&manifest)
            .args(args)
            .status()
            .unwrap();
        assert!(status.success(), "{args:?}");
    };
    run(&["init", "--product", "apple", "--capability", "groups"]);
    run(&["capability", "add", "outbox routing"]);
    let loaded = AppManifest::load(&manifest).unwrap();
    assert_eq!(loaded.capabilities, ["nip29", "nip65"]);
    assert_eq!(loaded.products, [Product::Apple]);
    run(&["capability", "remove", "groups"]);
    assert_eq!(
        AppManifest::load(&manifest).unwrap().capabilities,
        ["nip65"]
    );
}

struct MissingCargo;

impl CommandRunner for MissingCargo {
    fn run(
        &self,
        args: &[String],
        _: &Path,
        _: &BTreeMap<String, String>,
        _: bool,
    ) -> nmp_cli::Result<CommandOutput> {
        Err(Error::Refusal(format!(
            "{} is unavailable; install the Rust toolchain",
            args[0]
        )))
    }
}

#[test]
fn missing_tool_refuses_before_partial_output() {
    let temp = tempfile::tempdir().unwrap();
    let manifest_path = temp.path().join(".nmp.toml");
    let manifest = AppManifest::new(manifest_path, vec![Product::Apple], vec![]).unwrap();
    let output = temp.path().join("Generated/NMP");
    let result = Preparer::new(
        repository(),
        catalog(),
        &MissingCargo,
        PrepareOptions {
            output: output.clone(),
            cache_dir: temp.path().join("cache"),
        },
    )
    .prepare(&manifest);
    assert!(result
        .unwrap_err()
        .to_string()
        .contains("install the Rust toolchain"));
    assert!(!output.exists());
}

#[test]
fn provenance_refuses_a_modified_prepared_product() {
    let temp = tempfile::tempdir().unwrap();
    let product = temp.path();
    let wrapper = product.join("apple/Sources/NMP/Engine.swift");
    std::fs::create_dir_all(wrapper.parent().unwrap()).unwrap();
    std::fs::write(&wrapper, "original\n").unwrap();
    let digest = format!("{:x}", sha2::Sha256::digest(b"original\n"));
    std::fs::write(
        product.join("nmp-native-provenance.json"),
        format!(
            "{{\"identity\":\"fixture\",\"contents\":[{{\"path\":\"apple/Sources/NMP/Engine.swift\",\"sha256\":\"{digest}\"}}]}}"
        ),
    )
    .unwrap();
    std::fs::write(product.join(".nmp-native-generated"), "fixture\n").unwrap();
    verify_prepared_product(product).unwrap();
    let status = Command::new(env!("CARGO_BIN_EXE_nmp"))
        .args(["--source", "/does/not/exist", "verify", "--output"])
        .arg(product)
        .status()
        .unwrap();
    assert!(
        status.success(),
        "verify must not require an NMP source checkout"
    );
    std::fs::write(&wrapper, "modified\n").unwrap();
    assert!(verify_prepared_product(product)
        .unwrap_err()
        .to_string()
        .contains("content hash mismatch"));
}
