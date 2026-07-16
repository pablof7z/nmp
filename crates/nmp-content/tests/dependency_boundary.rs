use std::path::Path;
use std::process::Command;

#[test]
fn normal_dependency_tree_contains_no_engine_or_mechanism_crate() {
    let manifest_dir = Path::new(env!("CARGO_MANIFEST_DIR"));
    let workspace = manifest_dir
        .parent()
        .and_then(Path::parent)
        .expect("nmp-content lives under the workspace crates directory");
    let output = Command::new(env!("CARGO"))
        .current_dir(workspace)
        .args([
            "tree",
            "-p",
            "nmp-content",
            "-e",
            "normal",
            "--prefix",
            "none",
        ])
        .output()
        .expect("cargo tree must run for the dependency-boundary falsifier");
    assert!(
        output.status.success(),
        "cargo tree failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );

    let tree = String::from_utf8(output.stdout).expect("cargo tree output is UTF-8");
    for forbidden in [
        "nmp",
        "nmp-engine",
        "nmp-store",
        "nmp-router",
        "nmp-resolver",
        "nmp-transport",
    ] {
        assert!(
            !tree
                .lines()
                .any(|line| line.starts_with(&format!("{forbidden} v"))),
            "nmp-content normal dependency tree contains forbidden crate {forbidden}:\n{tree}"
        );
    }
}
