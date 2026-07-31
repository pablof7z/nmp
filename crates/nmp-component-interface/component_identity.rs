use std::collections::{BTreeMap, BTreeSet};
use std::env;
use std::fs;
use std::path::{Component, Path, PathBuf};
use std::process::Command;

pub struct IdentitySpec<'a> {
    pub version: &'a str,
    pub component_key: &'a str,
    pub cargo_package: &'a str,
    pub target: &'a str,
    pub profile: &'a str,
    pub interface_identity: Option<&'a str>,
    pub required_core_identity: Option<&'a str>,
    pub forbidden_packages: &'a [&'a str],
}

pub struct ComputedIdentity {
    pub identity: String,
    pub rustc_digest: String,
    pub flags_digest: String,
    pub graph_digest: String,
}

pub fn compute_component_identity(
    workspace: &Path,
    cargo: &Path,
    rustc: &Path,
    spec: &IdentitySpec<'_>,
) -> Result<ComputedIdentity, String> {
    let mut graph = cargo_unit_graph(workspace, cargo, spec)?;
    let package_roots = validate_and_collect_local_packages(&graph, workspace, cargo, spec)?;
    canonicalize_unit_graph(&mut graph, workspace);

    let rustc_output = Command::new(rustc)
        .args(["--version", "--verbose"])
        .output()
        .map_err(|error| format!("run rustc --version --verbose: {error}"))?;
    if !rustc_output.status.success() {
        return Err("rustc --version --verbose failed".to_owned());
    }
    let rustc_digest = blake3::hash(&rustc_output.stdout).to_hex().to_string();

    let mut flags_hasher = blake3::Hasher::new();
    for variable in [
        "CARGO_ENCODED_RUSTFLAGS",
        "RUSTFLAGS",
        "CARGO_BUILD_RUSTFLAGS",
    ] {
        let value = env::var(variable).unwrap_or_default();
        add_field(
            &mut flags_hasher,
            variable,
            normalize_build_text(&value, workspace).as_bytes(),
        );
    }
    let flags_digest = flags_hasher.finalize().to_hex().to_string();

    let canonical_graph = serde_json::to_vec(&graph)
        .map_err(|error| format!("serialize canonical Cargo unit graph: {error}"))?;
    let graph_digest = blake3::hash(&canonical_graph).to_hex().to_string();

    let mut hasher = blake3::Hasher::new();
    add_field(&mut hasher, "identity-version", spec.version.as_bytes());
    add_field(&mut hasher, "component-key", spec.component_key.as_bytes());
    add_field(&mut hasher, "cargo-package", spec.cargo_package.as_bytes());
    add_field(&mut hasher, "target", spec.target.as_bytes());
    add_field(&mut hasher, "profile", spec.profile.as_bytes());
    add_field(&mut hasher, "rustc", &rustc_output.stdout);
    add_field(&mut hasher, "build-flags", flags_digest.as_bytes());
    add_field(&mut hasher, "cargo-unit-graph", &canonical_graph);
    if let Some(interface) = spec.interface_identity {
        add_field(&mut hasher, "component-interface", interface.as_bytes());
    }
    if let Some(core) = spec.required_core_identity {
        add_field(&mut hasher, "required-core", core.as_bytes());
    }

    for package_root in package_roots {
        hash_source_tree(workspace, &package_root, &mut hasher)?;
    }
    hash_reachable_lock_entries(workspace, &graph, &mut hasher)?;

    Ok(ComputedIdentity {
        identity: format!("{}-{}", spec.version, hasher.finalize().to_hex()),
        rustc_digest,
        flags_digest,
        graph_digest,
    })
}

fn cargo_unit_graph(
    workspace: &Path,
    cargo: &Path,
    spec: &IdentitySpec<'_>,
) -> Result<serde_json::Value, String> {
    let mut command = Command::new(cargo);
    command.current_dir(workspace).args([
        "build",
        "-Z",
        "unstable-options",
        "--unit-graph",
        "--frozen",
        "-p",
        spec.cargo_package,
        "--lib",
        "--target",
        spec.target,
    ]);
    match spec.profile {
        "release" => {
            command.arg("--release");
        }
        "debug" => {}
        other => return Err(format!("unsupported component profile {other}")),
    }
    let output = command
        .output()
        .map_err(|error| format!("run Cargo no-build unit graph: {error}"))?;
    if !output.status.success() {
        return Err(format!(
            "Cargo no-build unit graph failed: {}",
            String::from_utf8_lossy(&output.stderr)
        ));
    }
    serde_json::from_slice(&output.stdout)
        .map_err(|error| format!("parse Cargo no-build unit graph: {error}"))
}

fn validate_and_collect_local_packages(
    graph: &serde_json::Value,
    workspace: &Path,
    cargo: &Path,
    spec: &IdentitySpec<'_>,
) -> Result<BTreeSet<PathBuf>, String> {
    let units = graph
        .get("units")
        .and_then(serde_json::Value::as_array)
        .ok_or_else(|| "Cargo unit graph has no units array".to_owned())?;
    let reachable = units
        .iter()
        .filter_map(|unit| unit.get("pkg_id").and_then(serde_json::Value::as_str))
        .collect::<BTreeSet<_>>();

    let output = Command::new(cargo)
        .current_dir(workspace)
        .args(["metadata", "--frozen", "--format-version", "1"])
        .output()
        .map_err(|error| format!("run Cargo metadata: {error}"))?;
    if !output.status.success() {
        return Err(format!(
            "Cargo metadata failed: {}",
            String::from_utf8_lossy(&output.stderr)
        ));
    }
    let metadata: serde_json::Value = serde_json::from_slice(&output.stdout)
        .map_err(|error| format!("parse Cargo metadata: {error}"))?;
    let packages = metadata
        .get("packages")
        .and_then(serde_json::Value::as_array)
        .ok_or_else(|| "Cargo metadata has no packages".to_owned())?;
    let workspace = workspace
        .canonicalize()
        .map_err(|error| format!("canonicalize workspace: {error}"))?;
    let mut roots = 0usize;
    let mut local = BTreeSet::new();

    for package in packages {
        let id = package
            .get("id")
            .and_then(serde_json::Value::as_str)
            .ok_or_else(|| "Cargo package has no id".to_owned())?;
        if !reachable.contains(id) {
            continue;
        }
        let name = package
            .get("name")
            .and_then(serde_json::Value::as_str)
            .ok_or_else(|| "Cargo package has no name".to_owned())?;
        if spec.forbidden_packages.contains(&name) {
            return Err(format!(
                "component {} resolved forbidden package {name}",
                spec.component_key
            ));
        }
        if name == spec.cargo_package {
            roots += 1;
        }
        if package.get("source").is_none_or(serde_json::Value::is_null) {
            let manifest = PathBuf::from(
                package
                    .get("manifest_path")
                    .and_then(serde_json::Value::as_str)
                    .ok_or_else(|| format!("local package {name} has no manifest path"))?,
            );
            let root = manifest
                .parent()
                .ok_or_else(|| format!("local package {name} manifest has no parent"))?
                .canonicalize()
                .map_err(|error| format!("canonicalize local package {name}: {error}"))?;
            if !root.starts_with(&workspace) {
                return Err(format!(
                    "external path override is not reproducible: {}",
                    root.display()
                ));
            }
            local.insert(root);
        }
    }
    if roots != 1 {
        return Err(format!(
            "resolved graph must contain exactly one {} package; found {roots}",
            spec.cargo_package
        ));
    }
    Ok(local)
}

fn hash_reachable_lock_entries(
    workspace: &Path,
    graph: &serde_json::Value,
    hasher: &mut blake3::Hasher,
) -> Result<(), String> {
    let reachable = graph
        .get("units")
        .and_then(serde_json::Value::as_array)
        .ok_or_else(|| "Cargo unit graph has no units array".to_owned())?
        .iter()
        .filter_map(|unit| unit.get("pkg_id").and_then(serde_json::Value::as_str))
        .map(package_coordinate)
        .collect::<BTreeSet<_>>();
    let lock_path = workspace.join("Cargo.lock");
    let lock: toml::Value = fs::read_to_string(&lock_path)
        .map_err(|error| format!("read {}: {error}", lock_path.display()))?
        .parse()
        .map_err(|error| format!("parse {}: {error}", lock_path.display()))?;
    let mut entries = Vec::new();
    for package in lock
        .get("package")
        .and_then(toml::Value::as_array)
        .ok_or_else(|| "Cargo.lock has no package array".to_owned())?
    {
        let name = package
            .get("name")
            .and_then(toml::Value::as_str)
            .ok_or_else(|| "Cargo.lock package has no name".to_owned())?;
        let version = package
            .get("version")
            .and_then(toml::Value::as_str)
            .ok_or_else(|| "Cargo.lock package has no version".to_owned())?;
        if !reachable.contains(&(name.to_owned(), version.to_owned())) {
            continue;
        }
        let mut entry = BTreeMap::new();
        entry.insert("name", name.to_owned());
        entry.insert("version", version.to_owned());
        entry.insert(
            "source",
            package
                .get("source")
                .and_then(toml::Value::as_str)
                .unwrap_or("")
                .to_owned(),
        );
        entry.insert(
            "checksum",
            package
                .get("checksum")
                .and_then(toml::Value::as_str)
                .unwrap_or("")
                .to_owned(),
        );
        entries.push(entry);
    }
    let bytes = serde_json::to_vec(&entries)
        .map_err(|error| format!("serialize reachable Cargo.lock subset: {error}"))?;
    add_field(hasher, "reachable-lock", &bytes);
    Ok(())
}

fn package_coordinate(pkg_id: &str) -> (String, String) {
    let tail = pkg_id.rsplit('#').next().unwrap_or(pkg_id);
    if let Some((name, version)) = tail.rsplit_once('@') {
        return (name.to_owned(), version.to_owned());
    }
    let name = pkg_id
        .split('#')
        .next()
        .unwrap_or(pkg_id)
        .rsplit('/')
        .next()
        .unwrap_or(pkg_id);
    (name.to_owned(), tail.to_owned())
}

pub fn normalize_build_text(text: &str, workspace: &Path) -> String {
    text.replace(workspace.to_string_lossy().as_ref(), "<workspace>")
}

pub fn canonicalize_unit_graph(value: &mut serde_json::Value, workspace: &Path) {
    match value {
        serde_json::Value::Object(fields) => {
            fields.remove("src_path");
            for value in fields.values_mut() {
                canonicalize_unit_graph(value, workspace);
            }
        }
        serde_json::Value::Array(values) => {
            for value in values.iter_mut() {
                canonicalize_unit_graph(value, workspace);
            }
            values.sort_by(|left, right| {
                serde_json::to_vec(left)
                    .expect("serialize normalized unit graph")
                    .cmp(&serde_json::to_vec(right).expect("serialize normalized unit graph"))
            });
        }
        serde_json::Value::String(text) => {
            *text = normalize_build_text(text, workspace);
        }
        _ => {}
    }
}

pub fn validate_release_out_dir(
    component_root: &Path,
    out_dir: &Path,
    target: &str,
    package: &str,
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
        && components[3].starts_with(&format!("{package}-"))
        && components[4] == "out"
        && components.iter().all(|component| !component.is_empty());
    if !valid {
        return Err(format!(
            "Cargo OUT_DIR {} is not the exact {target}/release {package} build under {}",
            out_dir.display(),
            component_root.display()
        ));
    }
    Ok(())
}

fn hash_source_tree(
    workspace: &Path,
    directory: &Path,
    hasher: &mut blake3::Hasher,
) -> Result<(), String> {
    let mut entries = fs::read_dir(directory)
        .map_err(|error| format!("read {}: {error}", directory.display()))?
        .map(|entry| {
            entry
                .map(|entry| entry.path())
                .map_err(|error| format!("read entry in {}: {error}", directory.display()))
        })
        .collect::<Result<Vec<_>, _>>()?;
    entries.sort();
    for path in entries {
        if path
            .file_name()
            .and_then(|name| name.to_str())
            .is_some_and(|name| name == "target" || name == ".git")
        {
            continue;
        }
        if path.is_dir() {
            hash_source_tree(workspace, &path, hasher)?;
        } else {
            hash_file(workspace, &path, hasher)?;
        }
    }
    Ok(())
}

fn hash_file(workspace: &Path, path: &Path, hasher: &mut blake3::Hasher) -> Result<(), String> {
    let relative = path
        .strip_prefix(workspace)
        .map_err(|_| format!("{} is outside {}", path.display(), workspace.display()))?
        .to_string_lossy()
        .replace('\\', "/");
    let bytes = fs::read(path).map_err(|error| format!("read {}: {error}", path.display()))?;
    add_field(hasher, &relative, &bytes);
    Ok(())
}

fn add_field(hasher: &mut blake3::Hasher, name: &str, value: &[u8]) {
    hasher.update(&(name.len() as u64).to_le_bytes());
    hasher.update(name.as_bytes());
    hasher.update(&(value.len() as u64).to_le_bytes());
    hasher.update(value);
}
