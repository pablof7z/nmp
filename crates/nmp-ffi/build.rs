use std::fs;
use std::path::{Path, PathBuf};

fn numeric_rhs(line: &str) -> bool {
    line.split_once('=')
        .is_some_and(|(_, rhs)| rhs.chars().any(|character| character.is_ascii_digit()))
}

fn assert_no_numeric_owner(path: &Path, source: &str) {
    for (index, line) in source.lines().enumerate() {
        let trimmed = line.trim();
        let rust_constant = (trimmed.starts_with("pub const DEFAULT_MAX_RELAYS")
            || trimmed.starts_with("pub const DEFAULT_MAX_AUTH_CAPABILITIES"))
            && numeric_rhs(trimmed);
        let uniffi_literal = trimmed.starts_with("#[uniffi(default =")
            && trimmed.chars().any(|character| character.is_ascii_digit());
        let swift_default = (trimmed.contains("maxRelays: UInt32 =")
            || trimmed.contains("maxAuthCapabilities: UInt32 ="))
            && numeric_rhs(trimmed);
        let kotlin_default = (trimmed.contains("val maxRelays: UInt =")
            || trimmed.contains("val maxAuthCapabilities: UInt ="))
            && numeric_rhs(trimmed);

        assert!(
            !(rust_constant || uniffi_literal || swift_default || kotlin_default),
            "{}:{} introduces a second numeric engine-config default; project it from \
             crates/nmp-engine-config-defaults.inc.rs instead",
            path.display(),
            index + 1
        );
    }
}

fn main() {
    let repository = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../..");
    let guarded = [
        "crates/nmp-transport/src/pool.rs",
        "crates/nmp-engine/src/runtime/mod.rs",
        "crates/nmp-ffi/src/facade.rs",
        "Packages/NMP/Sources/NMP/Engine.swift",
        "Packages/NMPKotlin/src/main/kotlin/com/nmp/sdk/Engine.kt",
    ];

    println!("cargo:rerun-if-changed=../nmp-engine-config-defaults.inc.rs");
    for relative in guarded {
        let path = repository.join(relative);
        println!("cargo:rerun-if-changed={}", path.display());
        let source = fs::read_to_string(&path)
            .unwrap_or_else(|error| panic!("failed to read {}: {error}", path.display()));
        assert_no_numeric_owner(&path, &source);
    }

    println!("cargo:rerun-if-env-changed=NMP_CONFIG_DEFAULT_OWNERSHIP_FALSIFIER");
    if let Ok(source) = std::env::var("NMP_CONFIG_DEFAULT_OWNERSHIP_FALSIFIER") {
        assert_no_numeric_owner(Path::new("<ownership-falsifier>"), &source);
    }
}
