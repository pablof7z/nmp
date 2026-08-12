use std::path::PathBuf;
use std::process::Command;

/// The app manifest, Cargo-resolution, generic source filtering, exact package
/// materialization, provenance, and cache contract is part of the native FFI
/// product. Keep its deterministic tool suite in the ordinary workspace test
/// lane instead of relying on a developer remembering a separate Python step.
#[test]
fn native_feature_selection_tool_contract() {
    let repository = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .and_then(|path| path.parent())
        .expect("nmp-ffi lives under <repository>/crates")
        .to_path_buf();
    let suite = repository.join("tools/nmp-native/test_nmp_native.py");
    let output = Command::new("python3")
        .arg(&suite)
        .current_dir(&repository)
        .output()
        .expect("python3 must be available in the repository test lane");

    assert!(
        output.status.success(),
        "native feature-selection tool contract failed\nstdout:\n{}\nstderr:\n{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr),
    );
}
