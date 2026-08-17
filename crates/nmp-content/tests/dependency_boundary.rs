use std::path::Path;
use std::process::Command;

#[test]
fn normal_dependency_tree_contains_no_engine_or_mechanism_crate() {
    // `cargo tree` needs the real workspace, and under `bazel test` the
    // process starts in a runfiles tree rather than the source checkout, so
    // CARGO_MANIFEST_DIR is a relative path into that tree. Bazel stages
    // source FILES as symlinks back to the checkout (the directories are
    // real), so canonicalizing this crate's own manifest is what recovers the
    // checkout. Under Cargo the path is already absolute and real, and
    // canonicalizing it changes nothing -- one spelling, both build systems.
    let manifest_dir =
        std::fs::canonicalize(Path::new(env!("CARGO_MANIFEST_DIR")).join("Cargo.toml"))
            .expect("nmp-content's own manifest must resolve")
            .parent()
            .expect("a manifest has a directory")
            .to_owned();
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
