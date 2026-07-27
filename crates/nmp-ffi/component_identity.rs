use std::path::Path;

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
            let workspace = workspace.to_string_lossy();
            if text.contains(workspace.as_ref()) {
                *text = text.replace(workspace.as_ref(), "<workspace>");
            }
        }
        _ => {}
    }
}
