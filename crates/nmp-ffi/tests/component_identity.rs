#[path = "../component_identity.rs"]
mod component_identity;

use serde_json::json;
use std::path::Path;

fn graph(workspace: &str, feature: &str) -> serde_json::Value {
    json!({
        "version": 1,
        "units": [{
            "pkg_id": format!("path+file://{workspace}/crates/nmp-ffi#0.1.0"),
            "target": {
                "name": "nmp_ffi",
                "src_path": format!("{workspace}/crates/nmp-ffi/src/lib.rs"),
            },
            "profile": {
                "name": "release",
                "panic": "unwind",
            },
            "platform": "aarch64-linux-android",
            "mode": "build",
            "features": [feature],
            "dependencies": [],
        }],
        "roots": [0],
    })
}

#[test]
fn unit_graph_identity_ignores_absolute_workspace_paths() {
    let mut first = graph("/checkout/one/nmp", "feature-a");
    let mut second = graph("/different/host/nmp", "feature-a");

    component_identity::canonicalize_unit_graph(&mut first, Path::new("/checkout/one/nmp"));
    component_identity::canonicalize_unit_graph(&mut second, Path::new("/different/host/nmp"));

    assert_eq!(first, second);
}

#[test]
fn unit_graph_identity_preserves_resolved_feature_changes() {
    let mut first = graph("/checkout/one/nmp", "feature-a");
    let mut second = graph("/checkout/one/nmp", "feature-b");

    component_identity::canonicalize_unit_graph(&mut first, Path::new("/checkout/one/nmp"));
    component_identity::canonicalize_unit_graph(&mut second, Path::new("/checkout/one/nmp"));

    assert_ne!(first, second);
}
