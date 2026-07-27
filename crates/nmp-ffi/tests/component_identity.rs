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
                "kind": ["lib"],
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

fn provider_graph(workspace: &str, provider_enabled: bool) -> serde_json::Value {
    let mut graph = graph(workspace, "feature-a");
    graph["units"][0]["features"] = if provider_enabled {
        json!(["feature-a", "nip46-provider-component"])
    } else {
        json!(["feature-a"])
    };
    graph
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
fn build_flags_ignore_absolute_workspace_paths() {
    let first = component_identity::normalize_build_text(
        "--remap-path-prefix=/checkout/one/nmp=/source",
        Path::new("/checkout/one/nmp"),
    );
    let second = component_identity::normalize_build_text(
        "--remap-path-prefix=/different/host/nmp=/source",
        Path::new("/different/host/nmp"),
    );

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

#[test]
fn unit_graph_must_match_cargo_observed_provider_resolution() {
    let core_only = provider_graph("/checkout/one/nmp", false);
    let matched = provider_graph("/checkout/one/nmp", true);

    assert!(component_identity::validate_unit_graph_against_cargo(&core_only, false).is_ok());
    assert!(component_identity::validate_unit_graph_against_cargo(&matched, true).is_ok());
    assert!(component_identity::validate_unit_graph_against_cargo(&core_only, true).is_err());
    assert!(component_identity::validate_unit_graph_against_cargo(&matched, false).is_err());
}
