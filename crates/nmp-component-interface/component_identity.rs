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

/// Package owning the Rust types that cross from the core artifact into a
/// separately linked component.
pub const INTERFACE_PACKAGE: &str = "nmp-component-interface";

pub struct ComputedIdentity {
    pub identity: String,
    pub rustc_digest: String,
    pub flags_digest: String,
    pub graph_digest: String,
    /// Digest of how every package the crossing contract itself declares
    /// resolved *in this build*, not in the contract's isolated graph.
    pub interface_dependency_digest: String,
    /// Human-readable form of exactly what that digest covers, for the
    /// refusal message when two components disagree.
    pub interface_dependency_summary: String,
}

pub fn compute_component_identity(
    workspace: &Path,
    cargo: &Path,
    rustc: &Path,
    spec: &IdentitySpec<'_>,
) -> Result<ComputedIdentity, String> {
    let metadata = cargo_metadata(workspace, cargo)?;
    let mut graph = cargo_unit_graph(workspace, cargo, spec)?;
    let package_roots = validate_and_collect_local_packages(&graph, &metadata, workspace, spec)?;
    canonicalize_unit_graph(&mut graph, workspace);
    let (interface_dependency_digest, interface_dependency_summary) =
        digest_interface_dependencies(&graph, &interface_dependency_packages(&metadata)?, spec)?;

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
        interface_dependency_digest,
        interface_dependency_summary,
    })
}

/// Every package the crossing contract names in its own manifest as a normal
/// dependency, plus the contract itself.
///
/// These are the only crates whose types the contract can spell in the API
/// that moves values between two independently resolved builds, so these are
/// the crates whose resolution both builds must agree on. Anything deeper is
/// private to one of them.
fn interface_dependency_packages(metadata: &serde_json::Value) -> Result<BTreeSet<String>, String> {
    let packages = metadata
        .get("packages")
        .and_then(serde_json::Value::as_array)
        .ok_or_else(|| "Cargo metadata has no packages".to_owned())?;
    let mut matches = packages.iter().filter(|package| {
        package.get("name").and_then(serde_json::Value::as_str) == Some(INTERFACE_PACKAGE)
    });
    let interface = matches
        .next()
        .ok_or_else(|| format!("Cargo metadata has no {INTERFACE_PACKAGE} package"))?;
    if matches.next().is_some() {
        return Err(format!(
            "Cargo metadata resolved more than one {INTERFACE_PACKAGE} package"
        ));
    }
    let mut names = BTreeSet::from([INTERFACE_PACKAGE.to_owned()]);
    for dependency in interface
        .get("dependencies")
        .and_then(serde_json::Value::as_array)
        .ok_or_else(|| format!("{INTERFACE_PACKAGE} has no dependencies array"))?
    {
        // `kind` is null for a normal dependency; build and dev dependencies
        // never contribute a type to the crossing.
        if !dependency
            .get("kind")
            .is_none_or(serde_json::Value::is_null)
        {
            continue;
        }
        let name = dependency
            .get("name")
            .and_then(serde_json::Value::as_str)
            .ok_or_else(|| format!("{INTERFACE_PACKAGE} dependency has no name"))?;
        // A dependency the contract does not always link cannot be required
        // to resolve identically on both sides, and silently skipping it
        // would hide exactly the divergence this digest exists to catch.
        for field in ["optional", "target"] {
            let conditional = match field {
                "optional" => {
                    dependency.get(field).and_then(serde_json::Value::as_bool) == Some(true)
                }
                _ => dependency.get(field).is_some_and(|value| !value.is_null()),
            };
            if conditional {
                return Err(format!(
                    "{INTERFACE_PACKAGE} dependency {name} is conditional ({field}); the \
                     crossing contract must link one unconditional dependency set"
                ));
            }
        }
        names.insert(name.to_owned());
    }
    Ok(names)
}

/// Digest the resolved units of those packages *as this component resolved
/// them*. Feature unification is a property of the whole graph being built,
/// so two components that link their own compilation of the same crate can
/// resolve it differently; the digest is what makes that visible.
fn digest_interface_dependencies(
    graph: &serde_json::Value,
    packages: &BTreeSet<String>,
    spec: &IdentitySpec<'_>,
) -> Result<(String, String), String> {
    let units = graph
        .get("units")
        .and_then(serde_json::Value::as_array)
        .ok_or_else(|| "Cargo unit graph has no units array".to_owned())?;
    let mut seen = BTreeSet::new();
    let mut records = Vec::new();
    let mut summary = Vec::new();
    for unit in units {
        let pkg_id = unit
            .get("pkg_id")
            .and_then(serde_json::Value::as_str)
            .ok_or_else(|| "Cargo unit has no pkg_id".to_owned())?;
        let (name, version) = package_coordinate(pkg_id);
        if !packages.contains(&name) {
            continue;
        }
        let mut record = unit
            .as_object()
            .ok_or_else(|| "Cargo unit is not an object".to_owned())?
            .clone();
        // Dependency edges are positions in this graph's unit array; they say
        // nothing comparable across two separately resolved graphs. Every
        // other field is already canonical (`canonicalize_unit_graph` dropped
        // `src_path` and normalized workspace paths).
        record.remove("dependencies");
        let features = unit
            .get("features")
            .and_then(serde_json::Value::as_array)
            .map(|features| {
                features
                    .iter()
                    .filter_map(serde_json::Value::as_str)
                    .collect::<Vec<_>>()
                    .join(",")
            })
            .unwrap_or_default();
        let mode = unit
            .get("mode")
            .and_then(serde_json::Value::as_str)
            .unwrap_or("");
        summary.push(format!("{name} {version} {mode} [{features}]"));
        records.push(
            serde_json::to_string(&serde_json::Value::Object(record))
                .map_err(|error| format!("serialize interface dependency unit: {error}"))?,
        );
        seen.insert(name);
    }
    let missing = packages.difference(&seen).cloned().collect::<Vec<_>>();
    if !missing.is_empty() {
        return Err(format!(
            "component {} resolved no unit for interface dependencies {}",
            spec.component_key,
            missing.join(", ")
        ));
    }
    records.sort();
    records.dedup();
    summary.sort();
    summary.dedup();
    let mut hasher = blake3::Hasher::new();
    add_field(
        &mut hasher,
        "interface-package",
        INTERFACE_PACKAGE.as_bytes(),
    );
    for record in &records {
        add_field(&mut hasher, "interface-dependency-unit", record.as_bytes());
    }
    Ok((hasher.finalize().to_hex().to_string(), summary.join("\n")))
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

fn cargo_metadata(workspace: &Path, cargo: &Path) -> Result<serde_json::Value, String> {
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
    serde_json::from_slice(&output.stdout).map_err(|error| format!("parse Cargo metadata: {error}"))
}

fn validate_and_collect_local_packages(
    graph: &serde_json::Value,
    metadata: &serde_json::Value,
    workspace: &Path,
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

/// Exactly the shape every digest recorded in a component manifest has.
pub fn is_digest(value: &str) -> bool {
    value.len() == 64
        && value
            .bytes()
            .all(|byte| byte.is_ascii_hexdigit() && !byte.is_ascii_uppercase())
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
