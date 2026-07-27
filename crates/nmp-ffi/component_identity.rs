use std::path::{Component, Path};

const PROVIDER_COMPONENT_FEATURE: &str = "nip46-provider-component";

pub fn normalize_build_text(text: &str, workspace: &Path) -> String {
    text.replace(workspace.to_string_lossy().as_ref(), "<workspace>")
}

pub fn validate_release_out_dir(
    component_root: &Path,
    out_dir: &Path,
    target: &str,
) -> Result<(), String> {
    let relative = out_dir.strip_prefix(component_root).map_err(|_| {
        format!(
            "Cargo OUT_DIR {} is outside component root {}",
            out_dir.display(),
            component_root.display()
        )
    })?;
    let components = relative
        .components()
        .map(|component| match component {
            Component::Normal(value) => value.to_string_lossy().into_owned(),
            _ => String::new(),
        })
        .collect::<Vec<_>>();
    let valid = components.len() == 5
        && components[0] == target
        && components[1] == "release"
        && components[2] == "build"
        && components[3].starts_with("nmp-ffi-")
        && components[4] == "out"
        && components.iter().all(|component| !component.is_empty());
    if !valid {
        return Err(format!(
            "Cargo OUT_DIR {} is not the exact {target}/release nmp-ffi build under {}",
            out_dir.display(),
            component_root.display()
        ));
    }
    Ok(())
}

pub fn canonicalize_unit_graph(value: &mut serde_json::Value, workspace: &Path) {
    match value {
        serde_json::Value::Object(fields) => {
            // Cargo reports absolute source paths for every unit. Package IDs,
            // target metadata, features, profiles, platforms, modes, roots,
            // and dependency edges fully identify the resolution; source
            // bytes are separately pinned by Cargo.lock and the governed
            // workspace hash.
            fields.remove("src_path");
            for value in fields.values_mut() {
                canonicalize_unit_graph(value, workspace);
            }
        }
        serde_json::Value::Array(values) => {
            for value in values {
                canonicalize_unit_graph(value, workspace);
            }
        }
        serde_json::Value::String(text) => {
            *text = normalize_build_text(text, workspace);
        }
        _ => {}
    }
}

pub fn validate_unit_graph_against_cargo(
    value: &serde_json::Value,
    workspace: &Path,
    cargo_has_provider_component: bool,
) -> Result<(), String> {
    let units = value
        .get("units")
        .and_then(serde_json::Value::as_array)
        .ok_or_else(|| "Cargo unit graph has no units array".to_owned())?;
    let mut observed = Vec::new();
    let workspace_package_prefix = format!(
        "path+file://{}/",
        workspace
            .to_string_lossy()
            .replace('\\', "/")
            .trim_end_matches('/')
    );

    for unit in units {
        let pkg_id = unit
            .get("pkg_id")
            .and_then(serde_json::Value::as_str)
            .ok_or_else(|| "Cargo unit graph unit has no package id".to_owned())?;
        if pkg_id.starts_with("path+file://") && !pkg_id.starts_with(&workspace_package_prefix) {
            return Err(format!(
                "external path override is not a reproducible component input: {pkg_id}"
            ));
        }
        let is_nmp_ffi = unit
            .get("pkg_id")
            .and_then(serde_json::Value::as_str)
            .is_some_and(|pkg_id| pkg_id.contains("/crates/nmp-ffi#"));
        let is_library = unit
            .pointer("/target/name")
            .and_then(serde_json::Value::as_str)
            == Some("nmp_ffi")
            && unit
                .pointer("/target/kind")
                .and_then(serde_json::Value::as_array)
                .is_some_and(|kinds| {
                    kinds
                        .iter()
                        .any(|kind| kind.as_str().is_some_and(|kind| kind == "lib"))
                });
        if !is_nmp_ffi
            || !is_library
            || unit.get("mode").and_then(serde_json::Value::as_str) != Some("build")
        {
            continue;
        }

        let graph_has_provider_component = unit
            .get("features")
            .and_then(serde_json::Value::as_array)
            .ok_or_else(|| "nmp-ffi library unit has no features array".to_owned())?
            .iter()
            .any(|feature| feature.as_str() == Some(PROVIDER_COMPONENT_FEATURE));
        observed.push(graph_has_provider_component);
    }

    if observed.is_empty() {
        return Err("Cargo unit graph has no nmp-ffi library build unit".to_owned());
    }
    if observed.iter().any(|graph_has_provider_component| {
        *graph_has_provider_component != cargo_has_provider_component
    }) {
        return Err(format!(
            "derived Cargo unit graph disagrees with Cargo-resolved \
             {PROVIDER_COMPONENT_FEATURE}: graph={observed:?}, cargo={cargo_has_provider_component}"
        ));
    }
    Ok(())
}
