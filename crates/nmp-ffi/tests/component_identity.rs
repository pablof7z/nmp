#[path = "../component_identity.rs"]
mod component_identity;

use serde_json::json;
use std::fs;
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
    let workspace = Path::new("/checkout/one/nmp");

    assert!(
        component_identity::validate_unit_graph_against_cargo(&core_only, workspace, false).is_ok()
    );
    assert!(
        component_identity::validate_unit_graph_against_cargo(&matched, workspace, true).is_ok()
    );
    assert!(
        component_identity::validate_unit_graph_against_cargo(&core_only, workspace, true).is_err()
    );
    assert!(
        component_identity::validate_unit_graph_against_cargo(&matched, workspace, false).is_err()
    );
}

#[test]
fn unit_graph_refuses_external_path_overrides() {
    let mut overridden = provider_graph("/checkout/one/nmp", false);
    overridden["units"].as_array_mut().unwrap().push(json!({
        "pkg_id": "path+file:///tmp/patched-uniffi#0.29.5",
        "target": {
            "name": "uniffi",
            "kind": ["lib"],
            "src_path": "/tmp/patched-uniffi/src/lib.rs",
        },
        "profile": {"name": "release"},
        "platform": null,
        "mode": "build",
        "features": [],
        "dependencies": [],
    }));

    let error = component_identity::validate_unit_graph_against_cargo(
        &overridden,
        Path::new("/checkout/one/nmp"),
        false,
    )
    .expect_err("a user-level path patch is not a reproducible component input");
    assert!(error.contains("external path override"));

    overridden["units"].as_array_mut().unwrap().pop();
    overridden["units"].as_array_mut().unwrap().push(json!({
        "pkg_id": "path+file:///checkout/one/nmp-shadow/crates/uniffi#0.29.5",
        "target": {
            "name": "uniffi",
            "kind": ["lib"],
            "src_path": "/checkout/one/nmp-shadow/crates/uniffi/src/lib.rs",
        },
        "profile": {"name": "release"},
        "platform": null,
        "mode": "build",
        "features": [],
        "dependencies": [],
    }));
    assert!(
        component_identity::validate_unit_graph_against_cargo(
            &overridden,
            Path::new("/checkout/one/nmp"),
            false,
        )
        .is_err(),
        "a sibling checkout with the workspace name as a prefix is still external"
    );
}

#[test]
fn all_literal_rust_includes_stay_inside_hashed_inputs() {
    let workspace = Path::new(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .and_then(Path::parent)
        .unwrap();
    let crates = workspace.join("crates");
    let fixtures = workspace.join("fixtures");
    let mut pending = fs::read_dir(&crates)
        .unwrap()
        .map(|entry| entry.unwrap().path())
        .filter(|path| {
            path.file_name()
                .and_then(|name| name.to_str())
                .is_some_and(|name| name != "nmp-nip46" && name != "nmp-nip46-ffi")
        })
        .collect::<Vec<_>>();

    while let Some(path) = pending.pop() {
        if path.is_dir() {
            pending.extend(
                fs::read_dir(path)
                    .unwrap()
                    .map(|entry| entry.unwrap().path()),
            );
            continue;
        }
        if path.extension().and_then(|extension| extension.to_str()) != Some("rs") {
            continue;
        }
        if path.ends_with("nmp-ffi/tests/component_identity.rs") {
            continue;
        }

        let source = fs::read_to_string(&path).unwrap();
        for macro_name in ["include_str!(\"", "include_bytes!(\""] {
            for suffix in source.split(macro_name).skip(1) {
                let relative = suffix
                    .split_once('"')
                    .map(|(relative, _)| relative)
                    .expect("literal include has a closing quote");
                let included = path
                    .parent()
                    .unwrap()
                    .join(relative)
                    .canonicalize()
                    .unwrap();
                assert!(
                    included.starts_with(&crates) || included.starts_with(&fixtures),
                    "{} includes unhashed input {}",
                    path.display(),
                    included.display()
                );
            }
        }
        let literal_count =
            source.matches("include_str!(\"").count() + source.matches("include_bytes!(\"").count();
        let total_count =
            source.matches("include_str!(").count() + source.matches("include_bytes!(").count();
        assert_eq!(
            literal_count,
            total_count,
            "{} has a non-literal include that the identity cannot enumerate",
            path.display()
        );
    }
}
