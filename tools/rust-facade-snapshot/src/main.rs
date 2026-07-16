//! Resolve dependency-owned items explicitly re-exported by the `nmp` facade.
//!
//! Discovery and shapes both come from compiler-produced rustdoc JSON. The
//! source tree is never scanned and there is no hand-maintained type list.

use std::{
    collections::{BTreeMap, BTreeSet},
    env, fs,
    path::{Path, PathBuf},
    process::Command,
    sync::Arc,
};

use anyhow::{bail, Context, Result};
use cargo_metadata::{Metadata, MetadataCommand, Package, PackageId, Target};
use serde_json::{json, Map, Value};

fn main() -> Result<()> {
    let mut args = env::args().skip(1);
    let root = PathBuf::from(
        args.next()
            .context("usage: nmp-rust-facade-snapshot <workspace> <toolchain> <target-dir>")?,
    );
    let toolchain = args.next().context("missing toolchain")?;
    let target_dir = PathBuf::from(args.next().context("missing target-dir")?);
    if args.next().is_some() {
        bail!("too many arguments");
    }
    print!("{}", generate(&root, &toolchain, &target_dir)?);
    Ok(())
}

fn generate(root: &Path, toolchain: &str, target_dir: &Path) -> Result<String> {
    generate_for_package(root, toolchain, target_dir, "nmp")
}

fn generate_for_package(
    root: &Path,
    toolchain: &str,
    target_dir: &Path,
    facade_package: &str,
) -> Result<String> {
    let manifest = root.join("Cargo.toml");
    let metadata = MetadataCommand::new()
        .manifest_path(&manifest)
        .other_options(vec!["--locked".to_owned()])
        .exec()
        .context("locked cargo metadata for facade workspace")?;
    let facade = package(&metadata, facade_package)?.id.clone();
    let mut graph = CargoDocGraph::new(root, toolchain, target_dir, manifest, metadata)?;
    graph.ensure_doc(&facade)?;
    let facade_doc = graph.doc(&facade)?;
    let exports = discover_external_reexports(&facade_doc, graph.crate_name(&facade)?)?;
    render_exports(exports, &facade, &mut graph)
}

#[derive(Debug, Eq, Ord, PartialEq, PartialOrd)]
struct Export {
    public_name: String,
    extern_alias: String,
    target_path: Vec<String>,
}

#[derive(Clone)]
struct PackageInfo {
    package: Package,
    crate_name: String,
    dependencies: BTreeMap<String, PackageId>,
}

struct CargoDocGraph {
    root: PathBuf,
    toolchain: String,
    target_dir: PathBuf,
    manifest: PathBuf,
    packages: BTreeMap<PackageId, PackageInfo>,
    docs: BTreeMap<PackageId, Arc<Value>>,
}

trait DocGraph {
    type Key: Clone + Ord;

    fn ensure_doc(&mut self, package: &Self::Key) -> Result<()>;
    fn doc(&self, package: &Self::Key) -> Result<Arc<Value>>;
    fn crate_name(&self, package: &Self::Key) -> Result<&str>;
    fn dependency(&self, package: &Self::Key, alias: &str) -> Option<Self::Key>;
    fn dependency_by_crate_name(
        &self,
        package: &Self::Key,
        crate_name: &str,
    ) -> Result<Option<Self::Key>>;
}

impl CargoDocGraph {
    fn new(
        root: &Path,
        toolchain: &str,
        target_dir: &Path,
        manifest: PathBuf,
        metadata: Metadata,
    ) -> Result<Self> {
        let resolve = metadata
            .resolve
            .as_ref()
            .context("cargo metadata has no resolve graph")?;
        let mut packages = BTreeMap::new();
        for package in &metadata.packages {
            let Ok(target) = lib_target(package) else {
                continue;
            };
            let crate_name = target.name.clone();
            let dependencies = resolve
                .nodes
                .iter()
                .find(|node| node.id == package.id)
                .map(|node| {
                    node.deps
                        .iter()
                        .map(|dependency| (dependency.name.clone(), dependency.pkg.clone()))
                        .collect()
                })
                .unwrap_or_default();
            packages.insert(
                package.id.clone(),
                PackageInfo {
                    package: package.clone(),
                    crate_name,
                    dependencies,
                },
            );
        }
        Ok(Self {
            root: root.to_owned(),
            toolchain: toolchain.to_owned(),
            target_dir: target_dir.to_owned(),
            manifest,
            packages,
            docs: BTreeMap::new(),
        })
    }
}

impl DocGraph for CargoDocGraph {
    type Key = PackageId;

    fn ensure_doc(&mut self, package: &PackageId) -> Result<()> {
        if self.docs.contains_key(package) {
            return Ok(());
        }
        let info = self
            .packages
            .get(package)
            .with_context(|| format!("package metadata missing for {package}"))?;
        rustdoc(
            &self.root,
            &self.toolchain,
            &self.target_dir,
            &self.manifest,
            &info.package,
        )?;
        let doc = read_doc(&self.target_dir, &info.crate_name)?;
        self.docs.insert(package.clone(), Arc::new(doc));
        Ok(())
    }

    fn doc(&self, package: &PackageId) -> Result<Arc<Value>> {
        self.docs
            .get(package)
            .cloned()
            .with_context(|| format!("rustdoc was not loaded for {package}"))
    }

    fn crate_name(&self, package: &PackageId) -> Result<&str> {
        self.packages
            .get(package)
            .map(|info| info.crate_name.as_str())
            .with_context(|| format!("package metadata missing for {package}"))
    }

    fn dependency(&self, package: &PackageId, alias: &str) -> Option<PackageId> {
        self.packages.get(package)?.dependencies.get(alias).cloned()
    }

    fn dependency_by_crate_name(
        &self,
        package: &PackageId,
        crate_name: &str,
    ) -> Result<Option<PackageId>> {
        let info = self
            .packages
            .get(package)
            .with_context(|| format!("package metadata missing for {package}"))?;
        let matches = info
            .dependencies
            .values()
            .filter(|dependency| {
                self.packages
                    .get(*dependency)
                    .is_some_and(|candidate| candidate.crate_name == crate_name)
            })
            .cloned()
            .collect::<BTreeSet<_>>();
        match matches.len() {
            0 => Ok(None),
            1 => Ok(matches.into_iter().next()),
            _ => bail!(
                "crate root {crate_name} resolves to multiple exact Cargo package IDs from {package}"
            ),
        }
    }
}

fn package<'a>(metadata: &'a Metadata, name: &str) -> Result<&'a Package> {
    metadata
        .packages
        .iter()
        .find(|package| package.name == name && package.source.is_none())
        .with_context(|| format!("workspace package {name} not found"))
}

fn lib_target(package: &Package) -> Result<&Target> {
    package
        .targets
        .iter()
        .find(|target| target.is_lib() || target.is_rlib() || target.is_proc_macro())
        .with_context(|| format!("package {} has no library target", package.id))
}

fn rustdoc(
    root: &Path,
    toolchain: &str,
    target_dir: &Path,
    manifest: &Path,
    package: &Package,
) -> Result<()> {
    let exact_package_id = &package.id.repr;
    let status = Command::new("cargo")
        .arg(format!("+{toolchain}"))
        .args(["rustdoc", "--locked", "--manifest-path"])
        .arg(manifest)
        .args(["-p", exact_package_id, "--lib", "--"])
        .args(["-Z", "unstable-options", "--output-format", "json"])
        .env("CARGO_TARGET_DIR", target_dir)
        .current_dir(root)
        .status()
        .with_context(|| format!("running locked rustdoc for {exact_package_id}"))?;
    if !status.success() {
        bail!("rustdoc failed for exact package {exact_package_id}");
    }
    Ok(())
}

fn read_doc(target_dir: &Path, crate_name: &str) -> Result<Value> {
    let path = target_dir.join("doc").join(format!("{crate_name}.json"));
    let bytes = fs::read(&path).with_context(|| format!("reading {}", path.display()))?;
    let doc: Value =
        serde_json::from_slice(&bytes).with_context(|| format!("parsing {}", path.display()))?;
    if doc["format_version"] != 60 {
        bail!(
            "unsupported rustdoc JSON format for {crate_name}: {}",
            doc["format_version"]
        );
    }
    Ok(doc)
}

fn discover_external_reexports(doc: &Value, facade_crate: &str) -> Result<BTreeSet<Export>> {
    let root = id_string(doc.get("root").context("rustdoc root missing")?)?;
    let index = doc["index"].as_object().context("rustdoc index missing")?;
    let root_item = index.get(&root).context("root module item missing")?;
    let root_items = root_item["inner"]["module"]["items"]
        .as_array()
        .context("root module children missing")?;
    let mut exports = BTreeSet::new();
    for child in root_items {
        let id = id_string(child)?;
        let item = index
            .get(&id)
            .with_context(|| format!("root child {id} missing"))?;
        if item["visibility"] != "public" {
            continue;
        }
        let Some(import) = item["inner"].get("use") else {
            continue;
        };
        if import["is_glob"] == true {
            bail!("public glob reexports require explicit support");
        }
        let target_id = id_string(&import["id"])?;
        let path = rustdoc_path(doc, &target_id)
            .with_context(|| format!("path missing for reexport target {target_id}"))?;
        let source = import["source"]
            .as_str()
            .context("reexport source missing")?;
        let extern_alias = source
            .split("::")
            .next()
            .filter(|part| !part.is_empty())
            .context("reexport source has no extern alias")?;
        let Some(target_crate) = path.first() else {
            bail!("reexport target {target_id} has an empty path");
        };
        if target_crate == facade_crate || matches!(extern_alias, "crate" | "self" | "super") {
            continue;
        }
        exports.insert(Export {
            public_name: import["name"]
                .as_str()
                .context("reexport name missing")?
                .to_owned(),
            extern_alias: extern_alias.to_owned(),
            target_path: path,
        });
    }
    Ok(exports)
}

fn render_exports<G: DocGraph>(
    exports: BTreeSet<Export>,
    facade: &G::Key,
    graph: &mut G,
) -> Result<String> {
    let mut output =
        String::from("# resolved dependency-owned reexports (rustdoc JSON format 60)\n");
    let mut state = CanonicalState::default();
    let mut resolved = Vec::new();
    for export in exports {
        let package = graph
            .dependency(facade, &export.extern_alias)
            .with_context(|| {
                format!(
                    "{} is not an exact direct facade dependency alias",
                    export.extern_alias
                )
            })?;
        graph.ensure_doc(&package)?;
        let mut canonical_path = export.target_path.clone();
        canonical_path[0] = graph.crate_name(&package)?.to_owned();
        let doc = graph.doc(&package)?;
        let target_id = find_path_id(&doc, &canonical_path).with_context(|| {
            format!(
                "rustdoc item missing for renamed target {}",
                canonical_path.join("::")
            )
        })?;
        state.root_items.insert(ItemKey {
            package: package.clone(),
            id: target_id.clone(),
        });
        resolved.push((export, package, canonical_path, target_id));
    }
    for (export, package, canonical_path, target_id) in resolved {
        let item = canonical_item(graph, &package, &target_id, &mut state)?;
        output.push_str(&format!(
            "reexport nmp::{} => {}\n",
            export.public_name,
            canonical_path.join("::")
        ));
        output.push_str(&serde_json::to_string_pretty(&item)?);
        output.push('\n');
    }
    Ok(output)
}

fn rustdoc_path(doc: &Value, id: &str) -> Option<Vec<String>> {
    doc["paths"][id]["path"].as_array().map(|parts| {
        parts
            .iter()
            .filter_map(Value::as_str)
            .map(str::to_owned)
            .collect()
    })
}

fn find_path_id(doc: &Value, wanted: &[String]) -> Option<String> {
    doc["paths"].as_object()?.iter().find_map(|(id, summary)| {
        let path = summary["path"].as_array()?;
        let matches = path.len() == wanted.len()
            && path
                .iter()
                .zip(wanted)
                .all(|(actual, expected)| actual.as_str() == Some(expected));
        matches.then(|| id.clone())
    })
}

#[derive(Clone, Eq, Ord, PartialEq, PartialOrd)]
struct ItemKey<K> {
    package: K,
    id: String,
}

struct CanonicalState<K> {
    active: BTreeSet<ItemKey<K>>,
    expanded: BTreeSet<ItemKey<K>>,
    root_items: BTreeSet<ItemKey<K>>,
}

impl<K> Default for CanonicalState<K> {
    fn default() -> Self {
        Self {
            active: BTreeSet::new(),
            expanded: BTreeSet::new(),
            root_items: BTreeSet::new(),
        }
    }
}

fn canonical_item<G: DocGraph>(
    graph: &mut G,
    package: &G::Key,
    id: &str,
    state: &mut CanonicalState<G::Key>,
) -> Result<Value> {
    let key = ItemKey {
        package: package.clone(),
        id: id.to_owned(),
    };
    let doc = graph.doc(package)?;
    if state.active.contains(&key) {
        return Ok(json!({"recursive_item": semantic_label(&doc, id)?}));
    }
    if !state.expanded.insert(key.clone()) {
        return Ok(json!({"previously_defined": semantic_label(&doc, id)?}));
    }
    state.active.insert(key.clone());
    let label = semantic_label(&doc, id)?;
    let item = doc["index"]
        .get(id)
        .cloned()
        .with_context(|| format!("rustdoc item {label} missing"))?;
    let mut out = Map::new();
    out.insert("name".into(), item["name"].clone());
    out.insert(
        "visibility".into(),
        normalize(graph, package, &item["visibility"], None, state)?,
    );
    let attrs = canonical_attrs(&item)?;
    out.insert("attrs".into(), Value::Array(attrs));
    if !item["deprecation"].is_null() {
        out.insert("deprecation".into(), item["deprecation"].clone());
    }
    out.insert(
        "inner".into(),
        normalize(graph, package, &item["inner"], None, state)?,
    );
    if state.root_items.contains(&key) {
        let inherent_impls = canonical_inherent_impls(graph, package, &item, state)?;
        if !inherent_impls.is_empty() {
            out.insert("inherent_impls".into(), Value::Array(inherent_impls));
        }
    }
    state.active.remove(&key);
    Ok(Value::Object(out))
}

fn canonical_attrs(item: &Value) -> Result<Vec<Value>> {
    const STABLE_ATTRS: &[&str] = &[
        "export_name",
        "link_section",
        "macro_export",
        "must_use",
        "no_mangle",
        "non_exhaustive",
        "repr",
        "target_feature",
    ];
    let attrs = item["attrs"].as_array().context("rustdoc attrs missing")?;
    let mut output = Vec::new();
    for attr in attrs {
        if let Some(text) = attr.as_str() {
            if text.starts_with("#[doc")
                || text.contains("CfgTrace")
                || text.contains("CfgAttrTrace")
                || text.contains("Inline(")
                || text.contains("/.cargo/")
            {
                continue;
            }
            output.push(attr.clone());
            continue;
        }
        let Some(object) = attr.as_object() else {
            bail!("rustdoc attribute is neither a string nor an object");
        };
        if object.len() != 1 {
            bail!("rustdoc attribute has a non-canonical object shape");
        }
        let key = object.keys().next().expect("checked one key");
        if STABLE_ATTRS.contains(&key.as_str()) {
            output.push(attr.clone());
        }
    }
    output.sort_by_key(Value::to_string);
    Ok(output)
}

fn canonical_inherent_impls<G: DocGraph>(
    graph: &mut G,
    package: &G::Key,
    item: &Value,
    state: &mut CanonicalState<G::Key>,
) -> Result<Vec<Value>> {
    let Some(kind) = item["inner"]
        .as_object()
        .and_then(|inner| inner.values().next())
    else {
        return Ok(Vec::new());
    };
    let Some(impl_ids) = kind.get("impls").and_then(Value::as_array) else {
        return Ok(Vec::new());
    };
    let doc = graph.doc(package)?;
    let mut output = Vec::new();
    for impl_id in impl_ids {
        let impl_id = id_string(impl_id)?;
        let impl_item = doc["index"]
            .get(&impl_id)
            .with_context(|| "type impl list contains an unindexed rustdoc id")?;
        let Some(implementation) = impl_item["inner"].get("impl") else {
            bail!("type impl list points to a non-impl rustdoc item");
        };
        if !implementation["trait"].is_null()
            || implementation["is_synthetic"] == true
            || !implementation["blanket_impl"].is_null()
            || implementation["is_negative"] == true
        {
            continue;
        }
        let items = implementation["items"]
            .as_array()
            .context("inherent impl items missing")?;
        let mut public_items = Vec::new();
        for associated_id in items {
            let associated_id = id_string(associated_id)?;
            let associated = doc["index"]
                .get(&associated_id)
                .with_context(|| "inherent impl contains an unindexed associated item")?;
            if associated["visibility"] != "public" || !is_inherent_associated_item(associated) {
                continue;
            }
            public_items.push(canonical_item(graph, package, &associated_id, state)?);
        }
        if public_items.is_empty() {
            continue;
        }
        let mut canonical = Map::new();
        canonical.insert(
            "generics".into(),
            normalize(
                graph,
                package,
                &implementation["generics"],
                Some("generics"),
                state,
            )?,
        );
        canonical.insert("items".into(), Value::Array(public_items));
        output.push(Value::Object(canonical));
    }
    Ok(output)
}

fn is_inherent_associated_item(item: &Value) -> bool {
    item["inner"]
        .as_object()
        .and_then(|inner| inner.keys().next())
        .is_some_and(|kind| {
            matches!(
                kind.as_str(),
                "function" | "assoc_const" | "assoc_type" | "type_alias"
            )
        })
}

fn normalize<G: DocGraph>(
    graph: &mut G,
    package: &G::Key,
    value: &Value,
    parent_key: Option<&str>,
    state: &mut CanonicalState<G::Key>,
) -> Result<Value> {
    let doc = graph.doc(package)?;
    match value {
        Value::Array(values)
            if matches!(parent_key, Some("variants" | "fields" | "items" | "tuple")) =>
        {
            let kind = parent_key.expect("matched array kind");
            values
                .iter()
                .map(|value| {
                    if value.is_null() {
                        if kind == "tuple" {
                            return Ok(Value::Null);
                        }
                        bail!("unexpected null rustdoc item in {kind} array");
                    }
                    if value.is_number() || value.is_string() {
                        let id = id_string(value)?;
                        if doc["index"].get(&id).is_none() {
                            bail!("unexpected unindexed rustdoc id in {kind} array");
                        }
                        return canonical_item(graph, package, &id, state);
                    }
                    normalize(graph, package, value, None, state)
                })
                .collect::<Result<Vec<_>>>()
                .map(Value::Array)
        }
        Value::Array(values) => values
            .iter()
            .map(|value| normalize(graph, package, value, None, state))
            .collect::<Result<Vec<_>>>()
            .map(Value::Array),
        Value::Object(object) => {
            let mut out = Map::new();
            for (key, value) in object {
                if matches!(
                    key.as_str(),
                    "span" | "docs" | "links" | "crate_id" | "impls" | "implementations"
                ) {
                    continue;
                }
                if key == "id" {
                    if !value.is_null() {
                        let id = id_string(value)?;
                        out.insert(
                            "resolved_item".into(),
                            canonical_reference(graph, package, &id, state)?,
                        );
                    }
                    continue;
                }
                out.insert(
                    key.clone(),
                    normalize(graph, package, value, Some(key), state)?,
                );
            }
            Ok(Value::Object(out))
        }
        _ => Ok(value.clone()),
    }
}

fn canonical_reference<G: DocGraph>(
    graph: &mut G,
    package: &G::Key,
    id: &str,
    state: &mut CanonicalState<G::Key>,
) -> Result<Value> {
    let doc = graph.doc(package)?;
    if let Some(path) = rustdoc_path(&doc, id) {
        let alias = path.first().context("rustdoc reference has empty path")?;
        let owner = if alias == graph.crate_name(package)? {
            Some(package.clone())
        } else {
            graph
                .dependency(package, alias)
                .or(graph.dependency_by_crate_name(package, alias)?)
        };
        let Some(owner) = owner else {
            return Ok(json!({"path": path.join("::")}));
        };
        graph.ensure_doc(&owner)?;
        let mut canonical_path = path;
        canonical_path[0] = graph.crate_name(&owner)?.to_owned();
        let owner_doc = graph.doc(&owner)?;
        let target_id = find_path_id(&owner_doc, &canonical_path).with_context(|| {
            format!(
                "dependency-owned rustdoc path has no exact definition: {}",
                canonical_path.join("::")
            )
        })?;
        let mut result = Map::new();
        result.insert("path".into(), Value::String(canonical_path.join("::")));
        if is_definition(&owner_doc, &target_id) {
            result.insert(
                "definition".into(),
                canonical_item(graph, &owner, &target_id, state)?,
            );
        }
        return Ok(Value::Object(result));
    }

    if doc["index"].get(id).is_some() {
        return Ok(json!({
            "anonymous_definition": canonical_item(graph, package, id, state)?
        }));
    }
    bail!(
        "rustdoc reference has neither a semantic path nor an indexed definition; refusing unstable numeric fallback"
    )
}

fn is_definition(doc: &Value, id: &str) -> bool {
    doc["index"][id]["inner"]
        .as_object()
        .and_then(|inner| inner.keys().next())
        .is_some_and(|kind| {
            matches!(
                kind.as_str(),
                "struct"
                    | "enum"
                    | "union"
                    | "type_alias"
                    | "trait"
                    | "trait_alias"
                    | "function"
                    | "constant"
                    | "static"
                    | "primitive"
                    | "foreign_type"
                    | "macro"
                    | "proc_macro"
                    | "assoc_type"
                    | "assoc_const"
            )
        })
}

fn semantic_label(doc: &Value, id: &str) -> Result<String> {
    if let Some(path) = rustdoc_path(doc, id) {
        return Ok(path.join("::"));
    }
    let item = doc["index"]
        .get(id)
        .context("rustdoc item lacks both path and indexed semantic shape")?;
    let kind = item["inner"]
        .as_object()
        .and_then(|inner| inner.keys().next())
        .context("path-free rustdoc item has no kind")?;
    let name = item["name"].as_str().unwrap_or("<anonymous>");
    Ok(format!("{kind}:{name}"))
}

fn id_string(value: &Value) -> Result<String> {
    match value {
        Value::String(id) => Ok(id.clone()),
        Value::Number(id) => Ok(id.to_string()),
        Value::Null => bail!("rustdoc id is null"),
        _ => bail!("rustdoc id is not scalar: {value}"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::{SystemTime, UNIX_EPOCH};

    fn toolchain() -> String {
        env::var("RUSTUP_TOOLCHAIN").expect("tests must run through the pinned nightly toolchain")
    }

    fn fixture() -> PathBuf {
        Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/renamed-workspace")
    }

    fn temp_dir(label: &str) -> PathBuf {
        let nonce = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let path = env::temp_dir().join(format!(
            "nmp-rust-facade-{label}-{}-{nonce}",
            std::process::id()
        ));
        fs::create_dir_all(&path).unwrap();
        path
    }

    fn copy_tree(source: &Path, destination: &Path) {
        fs::create_dir_all(destination).unwrap();
        for entry in fs::read_dir(source).unwrap() {
            let entry = entry.unwrap();
            let target = destination.join(entry.file_name());
            if entry.file_type().unwrap().is_dir() {
                copy_tree(&entry.path(), &target);
            } else {
                fs::copy(entry.path(), target).unwrap();
            }
        }
    }

    fn render_fixture(root: &Path, label: &str) -> Result<String> {
        generate_for_package(
            root,
            &toolchain(),
            &temp_dir(label).join("target"),
            "nmp-rust-facade-fixture",
        )
    }

    fn mutate_fixture(label: &str, relative: &str, from: &str, to: &str) -> PathBuf {
        let root = temp_dir(label).join("workspace");
        copy_tree(&fixture(), &root);
        let path = root.join(relative);
        let source = fs::read_to_string(&path).unwrap();
        assert!(source.contains(from), "fixture mutation needle missing");
        fs::write(path, source.replacen(from, to, 1)).unwrap();
        root
    }

    #[test]
    fn compiler_fixture_covers_renames_shapes_cycles_and_determinism() {
        let before = render_fixture(&fixture(), "fixture-a").unwrap();
        let repeated = render_fixture(&fixture(), "fixture-b").unwrap();
        assert_eq!(before, repeated);
        let copied_root = temp_dir("copied-root").join("workspace");
        copy_tree(&fixture(), &copied_root);
        assert_eq!(
            before,
            render_fixture(&copied_root, "copied-root-target").unwrap()
        );
        assert!(before.lines().count() < 5_000);
        assert!(before.len() < 200_000);
        assert!(before.contains("nmp::GroupedOuter => actual_lib_root::Outer"));
        assert!(before.contains("nmp::DuplicateOuter => actual_lib_root::Outer"));
        assert!(before.contains("deep_actual_root::Nested"));
        assert!(!before.contains("PrivateRoute"));
        assert!(before.contains("RouteAlias"));
        assert!(before.contains("RouteState"));
        // Compiler JSON represents fields as indexed, path-free item IDs.
        assert!(before.contains("\"name\": \"revision\""));
        assert!(before.contains("\"name\": \"Mixed\""));
        let mixed_name = before.find("\"name\": \"Mixed\"").unwrap();
        let mixed_shape = &before[mixed_name.saturating_sub(1_200)..mixed_name];
        assert!(mixed_shape.contains("\"name\": \"0\""));
        assert!(mixed_shape.contains("null"));
        assert!(before.contains("inherent_impls"));
        assert!(before.contains("\"name\": \"new\""));
        assert!(before.contains("\"name\": \"map\""));
        assert!(before.contains("\"name\": \"REVISION\""));
        assert!(before.contains("fixture method deprecation must be captured"));
        assert!(!before.contains("helper_only"));
        assert!(before.contains("generic_transform"));
        assert!(before.contains("where_predicates"));
        assert!(before.contains("fixture deprecation must be captured"));
        assert!(before.contains("\"must_use\""));
        assert!(before.contains("recursive_item"));
        assert!(!before.contains("unrelated_dependency_api"));
        assert!(!before.contains("local-item:"));
        assert!(!before.contains("\"id\":"));
        assert!(!before.contains("\"impls\""));
        for forbidden in [
            "/Users/",
            "/home/",
            "/private/var/",
            ".cargo/registry",
            "CfgTrace",
            "CfgAttrTrace",
            "#[attr = Inline",
        ] {
            assert!(!before.contains(forbidden), "found unstable {forbidden}");
        }

        let unrelated_change = mutate_fixture(
            "unrelated-change",
            "renamed-package/src/lib.rs",
            "pub fn unrelated_dependency_api() -> bool {",
            "pub fn another_unrelated_api() {}\n\npub fn unrelated_dependency_api() -> bool {",
        );
        assert_eq!(
            before,
            render_fixture(&unrelated_change, "unrelated-target").unwrap()
        );

        let struct_change = mutate_fixture(
            "struct-change",
            "nested-package/src/lib.rs",
            "pub revision: u32",
            "pub revision: u64",
        );
        assert_ne!(
            before,
            render_fixture(&struct_change, "struct-target").unwrap()
        );

        let enum_change = mutate_fixture(
            "enum-change",
            "nested-package/src/lib.rs",
            "Ready,",
            "Ready,\n    Deferred,",
        );
        assert_ne!(before, render_fixture(&enum_change, "enum-target").unwrap());

        let alias_change = mutate_fixture(
            "alias-change",
            "nested-package/src/lib.rs",
            "pub type RouteAlias = RouteState;",
            "pub type RouteAlias = RouteStateAlias;\npub type RouteStateAlias = RouteState;",
        );
        assert_ne!(
            before,
            render_fixture(&alias_change, "alias-target").unwrap()
        );

        let constructor_change = mutate_fixture(
            "constructor-change",
            "renamed-package/src/lib.rs",
            "pub fn new(nested: deep_alias::Nested<T>) -> Self {",
            "pub fn new<'a>(nested: deep_alias::Nested<T>) -> Self {",
        );
        assert_ne!(
            before,
            render_fixture(&constructor_change, "constructor-target").unwrap()
        );

        let method_generic_change = mutate_fixture(
            "method-generic-change",
            "renamed-package/src/lib.rs",
            "U: Clone,",
            "U: Clone + Send,",
        );
        assert_ne!(
            before,
            render_fixture(&method_generic_change, "method-generic-target").unwrap()
        );

        let associated_const_change = mutate_fixture(
            "associated-const-change",
            "renamed-package/src/lib.rs",
            "pub const REVISION: u32 = 0;",
            "pub const REVISION: u64 = 0;",
        );
        assert_ne!(
            before,
            render_fixture(&associated_const_change, "associated-const-target").unwrap()
        );

        let nested_method_change = mutate_fixture(
            "nested-method-change",
            "nested-package/src/lib.rs",
            "pub fn helper_only(&self) {}",
            "pub fn helper_only<'a>(&self) {}",
        );
        assert_eq!(
            before,
            render_fixture(&nested_method_change, "nested-method-target").unwrap()
        );
    }

    #[test]
    fn compiler_fixture_rejects_public_globs_and_lock_drift() {
        let glob = mutate_fixture(
            "glob",
            "facade/src/lib.rs",
            "pub use first_alias::Outer as DuplicateOuter;",
            "pub use first_alias::Outer as DuplicateOuter;\npub use first_alias::*;",
        );
        let error = render_fixture(&glob, "glob-target")
            .unwrap_err()
            .to_string();
        assert!(error.contains("public glob reexports"), "{error}");

        let drift = mutate_fixture(
            "lock-drift",
            "facade/Cargo.toml",
            "first_alias = { package = \"renamed-package\", path = \"../renamed-package\" }",
            "first_alias = { package = \"renamed-package\", path = \"../renamed-package\" }\ndeep_direct = { package = \"nested-package\", path = \"../nested-package\" }",
        );
        let error = render_fixture(&drift, "drift-target")
            .unwrap_err()
            .to_string();
        assert!(error.contains("locked cargo metadata"), "{error}");
    }

    #[test]
    fn path_free_reference_never_falls_back_to_numeric_identity() {
        let mut docs = BTreeMap::new();
        docs.insert(
            "dep".to_owned(),
            json!({
                "format_version":60,
                "index":{
                    "1":{"name":"Visible","visibility":"public","attrs":[],"deprecation":null,
                        "inner":{"struct":{"generics":{"params":[],"where_predicates":[]},
                        "kind":{"tuple":[2]},"impls":[]}}},
                    "2":{"name":null,"visibility":"default","attrs":[],"deprecation":null,
                        "inner":{"struct_field":{"resolved_path":{"name":"Missing","id":99,"args":null}}}}
                },
                "paths":{"1":{"path":["dep","Visible"],"kind":"struct"}}
            }),
        );
        let mut graph = MockGraph::new(docs);
        let error = canonical_item(
            &mut graph,
            &"dep".to_owned(),
            "1",
            &mut CanonicalState::default(),
        )
        .unwrap_err()
        .to_string();
        assert!(
            error.contains("refusing unstable numeric fallback"),
            "{error}"
        );
    }

    #[test]
    fn unindexed_numeric_array_item_is_a_typed_failure() {
        let mut docs = BTreeMap::new();
        docs.insert(
            "dep".to_owned(),
            json!({
                "format_version":60,
                "index":{
                    "1":{"name":"Mixed","visibility":"public","attrs":[],"deprecation":null,
                        "inner":{"struct":{"generics":{"params":[],"where_predicates":[]},
                        "kind":{"tuple":[2,99]},"impls":[]}}},
                    "2":{"name":"0","visibility":"public","attrs":[],"deprecation":null,
                        "inner":{"struct_field":{"primitive":"u8"}}}
                },
                "paths":{"1":{"path":["dep","Mixed"],"kind":"struct"}}
            }),
        );
        let mut graph = MockGraph::new(docs);
        let error = canonical_item(
            &mut graph,
            &"dep".to_owned(),
            "1",
            &mut CanonicalState::default(),
        )
        .unwrap_err()
        .to_string();
        assert!(
            error.contains("unexpected unindexed rustdoc id in tuple array"),
            "{error}"
        );
    }

    struct MockGraph {
        docs: BTreeMap<String, Arc<Value>>,
    }

    impl MockGraph {
        fn new(docs: BTreeMap<String, Value>) -> Self {
            Self {
                docs: docs
                    .into_iter()
                    .map(|(key, value)| (key, Arc::new(value)))
                    .collect(),
            }
        }
    }

    impl DocGraph for MockGraph {
        type Key = String;

        fn ensure_doc(&mut self, package: &String) -> Result<()> {
            if self.docs.contains_key(package) {
                Ok(())
            } else {
                bail!("missing mock doc")
            }
        }

        fn doc(&self, package: &String) -> Result<Arc<Value>> {
            self.docs.get(package).cloned().context("missing mock doc")
        }

        fn crate_name(&self, package: &String) -> Result<&str> {
            self.docs
                .get_key_value(package)
                .map(|(key, _)| key.as_str())
                .context("missing mock package")
        }

        fn dependency(&self, _package: &String, _alias: &str) -> Option<String> {
            None
        }

        fn dependency_by_crate_name(
            &self,
            _package: &String,
            _crate_name: &str,
        ) -> Result<Option<String>> {
            Ok(None)
        }
    }
}
