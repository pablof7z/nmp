use crate::catalog::{safe_path, FeatureSpec, SourceSpec};
use crate::{
    filter_source, read, refusal, write, AppManifest, Catalog, CommandRunner, Product, Result,
    GENERATED_MARKER, OUTPUT_SCHEMA_VERSION, PROVENANCE_FILE,
};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use sha2::{Digest, Sha256};
use std::collections::{BTreeMap, BTreeSet};
use std::fs;
use std::io::{BufReader, Read};
use std::path::{Path, PathBuf};
use tempfile::TempDir;
use walkdir::WalkDir;

#[derive(Clone, Debug)]
pub struct PrepareOptions {
    pub output: PathBuf,
    pub cache_dir: PathBuf,
}

#[derive(Clone, Debug, Serialize)]
pub struct PrepareResult {
    pub identity: String,
    pub cache_hit: bool,
    pub cache_entry: PathBuf,
    pub output: PathBuf,
    pub requested_capabilities: Vec<String>,
    pub resolved_capabilities: Vec<String>,
    pub android_abis: Vec<String>,
    pub contents: Vec<ContentRecord>,
}

#[derive(Clone, Debug)]
struct CargoResolution {
    metadata: Value,
    active_ffi_features: Vec<String>,
    resolved_features: Vec<FeatureSpec>,
    packages: Vec<String>,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct ContentRecord {
    path: String,
    sha256: String,
}

pub struct Preparer<'a> {
    repo_root: PathBuf,
    catalog: Catalog,
    runner: &'a dyn CommandRunner,
    options: PrepareOptions,
}

impl<'a> Preparer<'a> {
    pub fn new(
        repo_root: PathBuf,
        catalog: Catalog,
        runner: &'a dyn CommandRunner,
        options: PrepareOptions,
    ) -> Self {
        Self {
            repo_root,
            catalog,
            runner,
            options,
        }
    }

    pub fn prepare(&self, manifest: &AppManifest) -> Result<PrepareResult> {
        manifest.validate_capabilities(&self.catalog)?;
        self.validate_platform_inputs(manifest)?;
        let resolution = self.resolve(manifest)?;
        self.validate_selected_sources(&resolution.resolved_features)?;
        let context = self.identity_inputs(manifest, &resolution)?;
        let identity = sha256_bytes(&canonical_json(&context)?);
        let cache_entry = self.options.cache_dir.join(&identity);
        let cache_output = cache_entry.join("output");
        let cache_hit = cache_output.is_dir();
        if cache_hit {
            verify_product(&cache_output, &identity)?;
        } else {
            fs::create_dir_all(&self.options.cache_dir).map_err(|error| {
                refusal(format!(
                    "cannot create cache {}: {error}",
                    self.options.cache_dir.display()
                ))
            })?;
            let staging = tempfile::Builder::new()
                .prefix(".prepare-")
                .tempdir_in(&self.options.cache_dir)
                .map_err(|error| refusal(format!("cannot stage native product: {error}")))?;
            let product = staging.path().join("output");
            fs::create_dir(&product)
                .map_err(|error| refusal(format!("cannot stage product: {error}")))?;
            let selected: BTreeSet<_> = resolution
                .resolved_features
                .iter()
                .map(|feature| feature.key.clone())
                .collect();
            for platform in &manifest.products {
                match platform {
                    Product::Apple => self.build_apple(
                        &product.join("apple"),
                        staging.path(),
                        manifest,
                        &resolution,
                        &selected,
                    )?,
                    Product::KotlinJvm => self.build_kotlin(
                        &product.join("kotlin-jvm"),
                        staging.path(),
                        manifest,
                        &resolution,
                        &selected,
                    )?,
                    Product::Android => self.build_android(
                        &product.join("android"),
                        staging.path(),
                        manifest,
                        &resolution,
                        &selected,
                        (&identity, &context),
                    )?,
                }
            }
            let contents = content_inventory(&product)?;
            write(
                &product.join(PROVENANCE_FILE),
                serde_json::to_vec_pretty(&json!({
                    "schema": OUTPUT_SCHEMA_VERSION,
                    "identity": identity,
                    "identity_inputs": context,
                    "contents": contents,
                }))
                .map_err(|error| refusal(format!("cannot render provenance: {error}")))?,
            )?;
            write(&product.join(GENERATED_MARKER), format!("{identity}\n"))?;
            if let Err(error) = fs::rename(staging.path(), &cache_entry) {
                if cache_output.is_dir() {
                    verify_product(&cache_output, &identity)?;
                } else {
                    return Err(refusal(format!(
                        "cannot publish cache entry {}: {error}",
                        cache_entry.display()
                    )));
                }
            } else {
                std::mem::forget(staging);
            }
        }
        materialize_product(&cache_output, &self.options.output)?;
        let provenance: Value =
            serde_json::from_str(&read(&cache_output.join(PROVENANCE_FILE))?)
                .map_err(|error| refusal(format!("invalid generated provenance: {error}")))?;
        let contents = serde_json::from_value(provenance["contents"].clone())
            .map_err(|error| refusal(format!("invalid generated content inventory: {error}")))?;
        Ok(PrepareResult {
            identity,
            cache_hit,
            cache_entry,
            output: absolute(&self.options.output)?,
            requested_capabilities: manifest.capabilities.clone(),
            resolved_capabilities: resolution
                .resolved_features
                .iter()
                .map(|feature| feature.key.clone())
                .collect(),
            android_abis: if manifest.products.contains(&Product::Android) {
                self.catalog
                    .android
                    .abis
                    .iter()
                    .map(|abi| abi.name.clone())
                    .collect()
            } else {
                Vec::new()
            },
            contents,
        })
    }

    fn resolve(&self, manifest: &AppManifest) -> Result<CargoResolution> {
        let resolver = TempDir::new()
            .map_err(|error| refusal(format!("cannot create Cargo resolver: {error}")))?;
        let dependency_path = safe_path(&self.repo_root, &self.catalog.artifact.ffi_manifest)?
            .parent()
            .ok_or_else(|| refusal("FFI manifest has no parent"))?
            .to_path_buf();
        let features = manifest
            .capabilities
            .iter()
            .map(|key| {
                self.catalog
                    .by_key(key)
                    .expect("validated key")
                    .cargo_feature
                    .clone()
            })
            .collect::<Vec<_>>();
        let quoted = features
            .iter()
            .map(|feature| format!("{feature:?}"))
            .collect::<Vec<_>>()
            .join(", ");
        write(
            &resolver.path().join("Cargo.toml"),
            format!(
                "[package]\nname = \"nmp-native-resolver\"\nversion = \"0.0.0\"\nedition = \"2021\"\npublish = false\n\n[workspace]\n\n[dependencies.selected-nmp-ffi]\npackage = {:?}\npath = {:?}\ndefault-features = false\nfeatures = [{}]\n",
                self.catalog.artifact.ffi_package,
                dependency_path.display().to_string(),
                quoted,
            ),
        )?;
        write(&resolver.path().join("src/lib.rs"), "")?;
        let repository_lock = self.repo_root.join("Cargo.lock");
        fs::copy(&repository_lock, resolver.path().join("Cargo.lock"))
            .map_err(|error| refusal(format!("cannot seed Cargo resolver lockfile: {error}")))?;
        let manifest_path = resolver.path().join("Cargo.toml").display().to_string();
        let base = vec![
            "cargo".into(),
            "metadata".into(),
            "--format-version".into(),
            "1".into(),
            "--manifest-path".into(),
            manifest_path,
            "--no-default-features".into(),
        ];
        self.runner
            .run(&base, resolver.path(), &BTreeMap::new(), true)?;
        verify_resolver_lock(&resolver.path().join("Cargo.lock"), &repository_lock)?;
        let mut locked = base.clone();
        locked.push("--locked".into());
        let output = self
            .runner
            .run(&locked, resolver.path(), &BTreeMap::new(), true)?;
        let metadata: Value = serde_json::from_str(&output.stdout)
            .map_err(|error| refusal(format!("cargo metadata returned invalid JSON: {error}")))?;
        let expected_manifest = absolute(&safe_path(
            &self.repo_root,
            &self.catalog.artifact.ffi_manifest,
        )?)?;
        let packages = metadata["packages"]
            .as_array()
            .ok_or_else(|| refusal("cargo metadata lacks packages"))?;
        let candidates: Vec<_> = packages
            .iter()
            .filter(|package| {
                package["name"].as_str() == Some(&self.catalog.artifact.ffi_package)
                    && package["manifest_path"]
                        .as_str()
                        .and_then(|path| absolute(Path::new(path)).ok())
                        .as_ref()
                        == Some(&expected_manifest)
            })
            .collect();
        if candidates.len() != 1 {
            return Err(refusal(format!(
                "cargo metadata did not resolve exactly one {}",
                self.catalog.artifact.ffi_package
            )));
        }
        let package_id = candidates[0]["id"]
            .as_str()
            .ok_or_else(|| refusal("resolved FFI package has no id"))?;
        let nodes = metadata["resolve"]["nodes"]
            .as_array()
            .ok_or_else(|| refusal("cargo metadata lacks resolve nodes"))?;
        let node = nodes
            .iter()
            .find(|node| node["id"].as_str() == Some(package_id))
            .ok_or_else(|| refusal("cargo metadata lacks the FFI resolve node"))?;
        let mut active = node["features"]
            .as_array()
            .ok_or_else(|| refusal("FFI resolve node lacks features"))?
            .iter()
            .filter_map(Value::as_str)
            .map(str::to_owned)
            .collect::<Vec<_>>();
        active.sort();
        active.dedup();
        let unregistered: Vec<_> = active
            .iter()
            .filter(|feature| {
                feature.as_str() != "default" && self.catalog.by_cargo_feature(feature).is_none()
            })
            .cloned()
            .collect();
        if !unregistered.is_empty() {
            return Err(refusal(format!(
                "Cargo activated app-facing FFI features with no catalog metadata: {}",
                unregistered.join(", ")
            )));
        }
        let resolved_features = self
            .catalog
            .features
            .iter()
            .filter(|feature| active.contains(&feature.cargo_feature))
            .cloned()
            .collect::<Vec<_>>();
        let resolved_keys: BTreeSet<_> = resolved_features
            .iter()
            .map(|feature| feature.key.as_str())
            .collect();
        let missing: Vec<_> = manifest
            .capabilities
            .iter()
            .filter(|key| !resolved_keys.contains(key.as_str()))
            .cloned()
            .collect();
        if !missing.is_empty() {
            return Err(refusal(format!(
                "Cargo did not activate requested capabilities: {}",
                missing.join(", ")
            )));
        }
        let resolved_ids: BTreeSet<_> = nodes
            .iter()
            .filter_map(|node| node["id"].as_str())
            .collect();
        let mut package_names = packages
            .iter()
            .filter_map(|package| {
                let id = package["id"].as_str()?;
                let name = package["name"].as_str()?;
                let version = package["version"].as_str()?;
                resolved_ids
                    .contains(id)
                    .then(|| format!("{name}@{version}"))
            })
            .collect::<Vec<_>>();
        package_names.sort();
        Ok(CargoResolution {
            metadata,
            active_ffi_features: active,
            resolved_features,
            packages: package_names,
        })
    }

    fn validate_selected_sources(&self, features: &[FeatureSpec]) -> Result<()> {
        for feature in features {
            for relative in feature
                .ffi_sources
                .iter()
                .chain(feature.swift_sources.iter().map(|source| &source.path))
                .chain(feature.kotlin_sources.iter().map(|source| &source.path))
            {
                if !safe_path(&self.repo_root, relative)?.is_file() {
                    return Err(refusal(format!(
                        "selected capability {} source does not exist: {relative}",
                        feature.key
                    )));
                }
            }
        }
        Ok(())
    }

    fn validate_platform_inputs(&self, manifest: &AppManifest) -> Result<()> {
        // Run every validation before creating cache/product staging. Missing
        // tools therefore cannot leave an output that looks complete.
        self.runner.run(
            &strings(&["cargo", "-V"]),
            &self.repo_root,
            &BTreeMap::new(),
            true,
        )?;
        self.runner.run(
            &strings(&["rustc", "-vV"]),
            &self.repo_root,
            &BTreeMap::new(),
            true,
        )?;
        self.runner.run(
            &strings(&["rustup", "--version"]),
            &self.repo_root,
            &BTreeMap::new(),
            true,
        )?;
        if manifest.products.contains(&Product::Apple) {
            if std::env::consts::OS != "macos" {
                return Err(refusal("Apple preparation requires macOS with Xcode command-line tools; prepare on a Mac or remove `apple` from products"));
            }
            self.runner.run(
                &strings(&["xcodebuild", "-version"]),
                &self.repo_root,
                &BTreeMap::new(),
                true,
            )?;
            self.runner.run(
                &strings(&["xcrun", "--find", "lipo"]),
                &self.repo_root,
                &BTreeMap::new(),
                true,
            )?;
            let available: BTreeSet<_> = self
                .catalog
                .apple
                .slices
                .iter()
                .flat_map(|slice| &slice.targets)
                .map(String::as_str)
                .collect();
            let unknown: Vec<_> = manifest
                .apple_targets
                .iter()
                .filter(|target| !available.contains(target.as_str()))
                .cloned()
                .collect();
            if !unknown.is_empty() {
                return Err(refusal(format!("unknown Apple target(s): {}; remove them from .nmp.toml or select catalog targets", unknown.join(", "))));
            }
        }
        if manifest.products.contains(&Product::Android) {
            self.android_context()?;
        }
        Ok(())
    }

    fn identity_inputs(
        &self,
        manifest: &AppManifest,
        resolution: &CargoResolution,
    ) -> Result<Value> {
        let cargo = self
            .runner
            .run(
                &strings(&["cargo", "-V"]),
                &self.repo_root,
                &BTreeMap::new(),
                true,
            )?
            .stdout
            .trim()
            .to_owned();
        let rustc = self
            .runner
            .run(
                &strings(&["rustc", "-vV"]),
                &self.repo_root,
                &BTreeMap::new(),
                true,
            )?
            .stdout
            .trim()
            .to_owned();
        let revision = self
            .runner
            .run(
                &strings(&["git", "rev-parse", "HEAD"]),
                &self.repo_root,
                &BTreeMap::new(),
                true,
            )?
            .stdout
            .trim()
            .to_owned();
        let host_target = host_target(&rustc)?;
        let mut toolchains = serde_json::Map::new();
        toolchains.insert("cargo".into(), json!(cargo));
        toolchains.insert("rustc".into(), json!(rustc));
        if manifest.products.contains(&Product::Apple) {
            let xcode = self
                .runner
                .run(
                    &strings(&["xcodebuild", "-version"]),
                    &self.repo_root,
                    &BTreeMap::new(),
                    true,
                )?
                .stdout
                .trim()
                .to_owned();
            toolchains.insert("xcodebuild".into(), json!(xcode));
        }
        let fixed_environment = [
            "RUSTFLAGS",
            "CARGO_ENCODED_RUSTFLAGS",
            "RUSTC_WRAPPER",
            "RUSTC_WORKSPACE_WRAPPER",
            "CFLAGS",
            "CXXFLAGS",
            "LDFLAGS",
            "AR",
            "RANLIB",
            "CPATH",
            "LIBRARY_PATH",
            "MACOSX_DEPLOYMENT_TARGET",
            "IPHONEOS_DEPLOYMENT_TARGET",
            "SDKROOT",
            "CC",
            "CXX",
            "SOURCE_DATE_EPOCH",
        ];
        let mut environment = BTreeMap::new();
        for key in fixed_environment
            .into_iter()
            .chain(std::env::vars().filter_map(|(key, _)| {
                (key.starts_with("CARGO_TARGET_") || key.starts_with("CARGO_PROFILE_"))
                    .then_some(Box::leak(key.into_boxed_str()) as &'static str)
            }))
        {
            environment.insert(key.to_owned(), std::env::var(key).unwrap_or_default());
        }
        let products: Vec<_> = manifest
            .products
            .iter()
            .map(|product| product.as_str())
            .collect();
        let apple_targets = self.apple_targets(manifest)?;
        Ok(json!({
            "schema": OUTPUT_SCHEMA_VERSION,
            "manifest_schema": crate::APP_MANIFEST_SCHEMA_VERSION,
            "catalog_schema": crate::CATALOG_SCHEMA_VERSION,
            "catalog_sha256": sha256_file(&self.catalog.path)?,
            "source_revision": revision,
            "source_sha256": self.source_digest(resolution)?,
            "toolchains": toolchains,
            "host": {"system": std::env::consts::OS, "machine": std::env::consts::ARCH},
            "environment": environment,
            "ffi_package": self.catalog.artifact.ffi_package,
            "requested_features": manifest.capabilities,
            "resolved_features": resolution.resolved_features.iter().map(|feature| feature.key.as_str()).collect::<Vec<_>>(),
            "active_ffi_features": resolution.active_ffi_features,
            "resolved_packages": resolution.packages,
            "platforms": products,
            "profile": manifest.profile,
            "apple_targets": apple_targets,
            "host_target": host_target,
            "android": if manifest.products.contains(&Product::Android) { Some(self.android_context()?) } else { None },
        }))
    }

    fn source_digest(&self, resolution: &CargoResolution) -> Result<String> {
        let mut paths = BTreeSet::new();
        for relative in [
            "Cargo.toml",
            "Cargo.lock",
            "rust-toolchain.toml",
            ".cargo/config.toml",
        ] {
            let path = self.repo_root.join(relative);
            if path.is_file() {
                paths.insert(path);
            }
        }
        paths.insert(self.catalog.path.clone());
        let resolved_ids: BTreeSet<_> = resolution.metadata["resolve"]["nodes"]
            .as_array()
            .into_iter()
            .flatten()
            .filter_map(|node| node["id"].as_str())
            .collect();
        for package in resolution.metadata["packages"]
            .as_array()
            .into_iter()
            .flatten()
        {
            let Some(id) = package["id"].as_str() else {
                continue;
            };
            let Some(manifest) = package["manifest_path"].as_str() else {
                continue;
            };
            if resolved_ids.contains(id) {
                let package_root = Path::new(manifest)
                    .parent()
                    .expect("Cargo manifest has a parent");
                if package_root.starts_with(&self.repo_root) {
                    collect_files(package_root, &mut paths)?;
                }
            }
        }
        collect_files(&self.repo_root.join("crates/nmp-cli"), &mut paths)?;
        collect_files(
            &safe_path(&self.repo_root, &self.catalog.android.project_template)?,
            &mut paths,
        )?;
        let wrapper = safe_path(
            &self.repo_root,
            &self.catalog.android.gradle_wrapper_project,
        )?;
        for relative in [
            "gradlew",
            "gradlew.bat",
            "gradle/wrapper/gradle-wrapper.jar",
            "gradle/wrapper/gradle-wrapper.properties",
        ] {
            paths.insert(wrapper.join(relative));
        }
        let selected_sources = self
            .catalog
            .core
            .ffi_sources
            .iter()
            .cloned()
            .chain(
                self.catalog
                    .core
                    .swift_sources
                    .iter()
                    .map(|source| source.path.clone()),
            )
            .chain(
                self.catalog
                    .core
                    .kotlin_sources
                    .iter()
                    .map(|source| source.path.clone()),
            )
            .chain(
                resolution
                    .resolved_features
                    .iter()
                    .flat_map(|feature| feature.ffi_sources.iter().cloned()),
            )
            .chain(resolution.resolved_features.iter().flat_map(|feature| {
                feature
                    .swift_sources
                    .iter()
                    .map(|source| source.path.clone())
            }))
            .chain(resolution.resolved_features.iter().flat_map(|feature| {
                feature
                    .kotlin_sources
                    .iter()
                    .map(|source| source.path.clone())
            }));
        for relative in selected_sources {
            paths.insert(safe_path(&self.repo_root, &relative)?);
        }
        let mut hasher = Sha256::new();
        for path in paths {
            let relative = path.strip_prefix(&self.repo_root).unwrap_or(&path);
            hasher.update(relative.to_string_lossy().as_bytes());
            hasher.update([0]);
            update_hasher_from_file(&mut hasher, &path)?;
            hasher.update([0]);
        }
        Ok(format!("{:x}", hasher.finalize()))
    }

    fn apple_targets(&self, manifest: &AppManifest) -> Result<Vec<String>> {
        let available = self
            .catalog
            .apple
            .slices
            .iter()
            .flat_map(|slice| &slice.targets)
            .cloned()
            .collect::<Vec<_>>();
        if manifest.apple_targets.is_empty() {
            Ok(available)
        } else {
            Ok(available
                .into_iter()
                .filter(|target| manifest.apple_targets.contains(target))
                .collect())
        }
    }

    fn cargo_feature_args(&self, resolution: &CargoResolution) -> Vec<String> {
        let features = resolution
            .resolved_features
            .iter()
            .map(|feature| feature.cargo_feature.as_str())
            .collect::<Vec<_>>();
        if features.is_empty() {
            Vec::new()
        } else {
            vec!["--features".into(), features.join(",")]
        }
    }

    fn profile_args(profile: &str) -> Vec<String> {
        if profile == "dev" {
            Vec::new()
        } else {
            vec!["--profile".into(), profile.into()]
        }
    }

    fn profile_dir(profile: &str) -> &str {
        if profile == "dev" {
            "debug"
        } else {
            profile
        }
    }

    fn cargo_env(&self) -> Result<BTreeMap<String, String>> {
        let target = self.options.cache_dir.join("cargo-target");
        fs::create_dir_all(&target)
            .map_err(|error| refusal(format!("cannot create Cargo target cache: {error}")))?;
        Ok(BTreeMap::from([(
            "CARGO_TARGET_DIR".into(),
            target.display().to_string(),
        )]))
    }

    fn build_apple(
        &self,
        output: &Path,
        staging: &Path,
        manifest: &AppManifest,
        resolution: &CargoResolution,
        selected: &BTreeSet<String>,
    ) -> Result<()> {
        fs::create_dir_all(output)
            .map_err(|error| refusal(format!("cannot stage Apple product: {error}")))?;
        let targets = self.apple_targets(manifest)?;
        let installed = self
            .runner
            .run(
                &strings(&["rustup", "target", "list", "--installed"]),
                &self.repo_root,
                &BTreeMap::new(),
                true,
            )?
            .stdout;
        let installed: BTreeSet<_> = installed.lines().collect();
        let missing: Vec<_> = targets
            .iter()
            .filter(|target| !installed.contains(target.as_str()))
            .cloned()
            .collect();
        if !missing.is_empty() {
            let mut args = strings(&["rustup", "target", "add"]);
            args.extend(missing);
            self.runner
                .run(&args, &self.repo_root, &BTreeMap::new(), false)?;
        }
        let cargo_env = self.cargo_env()?;
        let target_dir = PathBuf::from(&cargo_env["CARGO_TARGET_DIR"]);
        let host = host_target(
            &self
                .runner
                .run(
                    &strings(&["rustc", "-vV"]),
                    &self.repo_root,
                    &BTreeMap::new(),
                    true,
                )?
                .stdout,
        )?;
        let library_name = format!("lib{}.a", self.catalog.artifact.library_stem);
        let mut built = BTreeMap::new();
        for target in &targets {
            let mut args = strings(&["cargo", "build", "--locked", "--package"]);
            args.push(self.catalog.artifact.ffi_package.clone());
            args.push("--no-default-features".into());
            args.extend(self.cargo_feature_args(resolution));
            args.extend(Self::profile_args(&manifest.profile));
            args.extend(strings(&["--target"]));
            args.push(target.clone());
            args.push("--lib".into());
            if target == &host {
                args.extend(strings(&["--bin"]));
                args.push(self.catalog.artifact.bindgen_bin.clone());
            }
            let mut env = cargo_env.clone();
            if target.ends_with("apple-darwin") {
                env.insert(
                    "MACOSX_DEPLOYMENT_TARGET".into(),
                    self.catalog.apple.macos_deployment_target.clone(),
                );
            }
            self.runner.run(&args, &self.repo_root, &env, false)?;
            let library = target_dir
                .join(target)
                .join(Self::profile_dir(&manifest.profile))
                .join(&library_name);
            if !library.is_file() {
                return Err(refusal(format!(
                    "Cargo did not produce expected Apple library {}",
                    library.display()
                )));
            }
            built.insert(target.clone(), library);
        }
        let slice_root = staging.join("apple-slices");
        fs::create_dir_all(&slice_root)
            .map_err(|error| refusal(format!("cannot stage Apple slices: {error}")))?;
        let mut slices = Vec::new();
        for slice in &self.catalog.apple.slices {
            let libraries: Vec<_> = slice
                .targets
                .iter()
                .filter_map(|target| built.get(target))
                .collect();
            match libraries.as_slice() {
                [] => {}
                [one] => slices.push((*one).clone()),
                _ => {
                    let merged = slice_root.join(format!("{}-{library_name}", slice.name));
                    let mut args = strings(&["lipo", "-create"]);
                    args.extend(libraries.iter().map(|path| path.display().to_string()));
                    args.extend(strings(&["-output"]));
                    args.push(merged.display().to_string());
                    self.runner
                        .run(&args, &self.repo_root, &BTreeMap::new(), false)?;
                    if !merged.is_file() {
                        return Err(refusal(format!(
                            "lipo did not produce {}",
                            merged.display()
                        )));
                    }
                    slices.push(merged);
                }
            }
        }
        if slices.is_empty() {
            return Err(refusal("selected Apple targets produced no catalog slice"));
        }
        let bindgen = if targets.contains(&host) {
            target_dir
                .join(&host)
                .join(Self::profile_dir(&manifest.profile))
                .join(&self.catalog.artifact.bindgen_bin)
        } else {
            let mut args = strings(&["cargo", "build", "--locked", "--package"]);
            args.push(self.catalog.artifact.ffi_package.clone());
            args.extend(strings(&["--bin"]));
            args.push(self.catalog.artifact.bindgen_bin.clone());
            args.push("--no-default-features".into());
            args.extend(self.cargo_feature_args(resolution));
            args.extend(Self::profile_args(&manifest.profile));
            self.runner.run(&args, &self.repo_root, &cargo_env, false)?;
            target_dir
                .join(Self::profile_dir(&manifest.profile))
                .join(&self.catalog.artifact.bindgen_bin)
        };
        let generated = staging.join("apple-generated");
        fs::create_dir_all(&generated)
            .map_err(|error| refusal(format!("cannot stage UniFFI: {error}")))?;
        let args = vec![
            bindgen.display().to_string(),
            "generate".into(),
            "--library".into(),
            built
                .values()
                .next()
                .expect("slice library")
                .display()
                .to_string(),
            "--language".into(),
            "swift".into(),
            "--out-dir".into(),
            generated.display().to_string(),
            "--no-format".into(),
        ];
        self.runner.run(&args, &self.repo_root, &cargo_env, false)?;
        let stem = &self.catalog.artifact.library_stem;
        let swift = generated.join(format!("{stem}.swift"));
        let header = generated.join(format!("{stem}FFI.h"));
        let modulemap = generated.join(format!("{stem}FFI.modulemap"));
        for path in [&swift, &header, &modulemap] {
            if !path.is_file() {
                return Err(refusal(format!(
                    "UniFFI did not produce {}",
                    path.display()
                )));
            }
        }
        let headers = staging.join("apple-headers");
        fs::create_dir_all(&headers)
            .map_err(|error| refusal(format!("cannot stage headers: {error}")))?;
        copy(&header, &headers.join(header.file_name().unwrap()))?;
        copy(&modulemap, &headers.join("module.modulemap"))?;
        let xcframework = output.join(&self.catalog.apple.xcframework_name);
        let mut args = strings(&["xcodebuild", "-create-xcframework"]);
        for library in slices {
            args.push("-library".into());
            args.push(library.display().to_string());
            args.push("-headers".into());
            args.push(headers.display().to_string());
        }
        args.push("-output".into());
        args.push(xcframework.display().to_string());
        self.runner
            .run(&args, &self.repo_root, &BTreeMap::new(), false)?;
        if !xcframework.is_dir() {
            return Err(refusal(format!(
                "xcodebuild did not produce {}",
                xcframework.display()
            )));
        }
        let ffi = output.join("Sources").join(&self.catalog.apple.ffi_target);
        fs::create_dir_all(&ffi)
            .map_err(|error| refusal(format!("cannot stage Swift FFI: {error}")))?;
        copy(&swift, &ffi.join(swift.file_name().unwrap()))?;
        let sources = self
            .catalog
            .selected_swift_sources(&resolution.resolved_features)?;
        self.materialize_sources(&sources, output, selected, true)?;
        write(&output.join("Package.swift"), self.swift_package(&sources)?)?;
        Ok(())
    }

    fn build_kotlin(
        &self,
        output: &Path,
        staging: &Path,
        manifest: &AppManifest,
        resolution: &CargoResolution,
        selected: &BTreeSet<String>,
    ) -> Result<()> {
        if !matches!(std::env::consts::OS, "macos" | "linux") {
            return Err(refusal("Kotlin/JVM preparation requires macOS or Linux"));
        }
        fs::create_dir_all(output)
            .map_err(|error| refusal(format!("cannot stage Kotlin product: {error}")))?;
        let cargo_env = self.cargo_env()?;
        let rustc = self
            .runner
            .run(
                &strings(&["rustc", "-vV"]),
                &self.repo_root,
                &BTreeMap::new(),
                true,
            )?
            .stdout;
        let host = host_target(&rustc)?;
        let mut args = strings(&["cargo", "build", "--locked", "--package"]);
        args.push(self.catalog.artifact.ffi_package.clone());
        args.push("--no-default-features".into());
        args.extend(self.cargo_feature_args(resolution));
        args.extend(Self::profile_args(&manifest.profile));
        args.extend(strings(&["--target"]));
        args.push(host.clone());
        args.extend(strings(&["--lib", "--bin"]));
        args.push(self.catalog.artifact.bindgen_bin.clone());
        self.runner.run(&args, &self.repo_root, &cargo_env, false)?;
        let extension = if std::env::consts::OS == "macos" {
            "dylib"
        } else {
            "so"
        };
        let native_name = format!("lib{}.{extension}", self.catalog.artifact.library_stem);
        let target_dir = PathBuf::from(&cargo_env["CARGO_TARGET_DIR"]);
        let native = target_dir
            .join(&host)
            .join(Self::profile_dir(&manifest.profile))
            .join(&native_name);
        let bindgen = target_dir
            .join(&host)
            .join(Self::profile_dir(&manifest.profile))
            .join(&self.catalog.artifact.bindgen_bin);
        if !native.is_file() {
            return Err(refusal(format!(
                "Cargo did not produce expected Kotlin library {}",
                native.display()
            )));
        }
        if !bindgen.is_file() {
            return Err(refusal(format!(
                "Cargo did not produce expected binding generator {}",
                bindgen.display()
            )));
        }
        let generated = staging.join("kotlin-generated");
        fs::create_dir_all(&generated)
            .map_err(|error| refusal(format!("cannot stage Kotlin bindings: {error}")))?;
        let args = vec![
            bindgen.display().to_string(),
            "generate".into(),
            "--library".into(),
            native.display().to_string(),
            "--language".into(),
            "kotlin".into(),
            "--out-dir".into(),
            generated.display().to_string(),
            "--no-format".into(),
        ];
        self.runner.run(&args, &self.repo_root, &cargo_env, false)?;
        let binding = generated
            .join("uniffi")
            .join(&self.catalog.artifact.library_stem)
            .join(format!("{}.kt", self.catalog.artifact.library_stem));
        if !binding.is_file() {
            return Err(refusal(format!(
                "UniFFI did not produce {}",
                binding.display()
            )));
        }
        let destination = output
            .join("src/main/kotlin/uniffi")
            .join(&self.catalog.artifact.library_stem);
        fs::create_dir_all(&destination)
            .map_err(|error| refusal(format!("cannot stage Kotlin binding: {error}")))?;
        copy(&binding, &destination.join(binding.file_name().unwrap()))?;
        let sources = self
            .catalog
            .selected_kotlin_sources(&resolution.resolved_features, "kotlin-jvm")?;
        self.materialize_sources(&sources, output, selected, false)?;
        write_source_inventory(&output.join("nmp-kotlin-sources.json"), &sources)?;
        let prefix = jna_prefix()?;
        let resource = output.join("src/main/resources").join(prefix);
        fs::create_dir_all(&resource)
            .map_err(|error| refusal(format!("cannot stage native resource: {error}")))?;
        copy(&native, &resource.join(&native_name))?;
        if std::env::consts::OS == "macos" {
            let legacy = output.join("src/main/resources/darwin");
            fs::create_dir_all(&legacy)
                .map_err(|error| refusal(format!("cannot stage legacy resource: {error}")))?;
            copy(&native, &legacy.join(&native_name))?;
        }
        write(
            &output.join("settings.gradle.kts"),
            format!(
                "rootProject.name = {:?}\n",
                self.catalog.kotlin.project_name
            ),
        )?;
        write(&output.join("build.gradle.kts"), self.kotlin_gradle())?;
        Ok(())
    }

    fn build_android(
        &self,
        output: &Path,
        staging: &Path,
        manifest: &AppManifest,
        resolution: &CargoResolution,
        selected: &BTreeSet<String>,
        provenance: (&str, &Value),
    ) -> Result<()> {
        let (identity, context) = provenance;
        if !matches!(std::env::consts::OS, "macos" | "linux") {
            return Err(refusal("Android preparation requires macOS or Linux"));
        }
        self.android_context()?;
        copy_tree(
            &safe_path(&self.repo_root, &self.catalog.android.project_template)?,
            output,
        )?;
        let wrapper = safe_path(
            &self.repo_root,
            &self.catalog.android.gradle_wrapper_project,
        )?;
        for relative in [
            "gradlew",
            "gradlew.bat",
            "gradle/wrapper/gradle-wrapper.jar",
            "gradle/wrapper/gradle-wrapper.properties",
        ] {
            copy(&wrapper.join(relative), &output.join(relative))?;
        }
        let (sdk, ndk) = self.android_roots()?;
        let mut cargo_env = self.cargo_env()?;
        cargo_env.insert("ANDROID_HOME".into(), sdk.display().to_string());
        cargo_env.insert("ANDROID_SDK_ROOT".into(), sdk.display().to_string());
        cargo_env.insert("ANDROID_NDK_HOME".into(), ndk.display().to_string());
        if let Ok(java_home) = std::env::var("JAVA_HOME") {
            cargo_env.insert("JAVA_HOME".into(), java_home);
        }
        let installed = self
            .runner
            .run(
                &strings(&["rustup", "target", "list", "--installed"]),
                &self.repo_root,
                &BTreeMap::new(),
                true,
            )?
            .stdout;
        let installed: BTreeSet<_> = installed.lines().collect();
        let missing: Vec<_> = self
            .catalog
            .android
            .abis
            .iter()
            .filter(|abi| !installed.contains(abi.rust_target.as_str()))
            .map(|abi| abi.rust_target.clone())
            .collect();
        if !missing.is_empty() {
            let mut args = strings(&["rustup", "target", "add"]);
            args.extend(missing);
            self.runner
                .run(&args, &self.repo_root, &BTreeMap::new(), false)?;
        }
        let jni = output.join("src/main/jniLibs");
        fs::create_dir_all(&jni)
            .map_err(|error| refusal(format!("cannot stage Android JNI directory: {error}")))?;
        let mut args = strings(&["cargo", "ndk"]);
        for abi in &self.catalog.android.abis {
            args.push("--target".into());
            args.push(abi.name.clone());
        }
        args.push("--platform".into());
        args.push(self.catalog.android.min_sdk.to_string());
        args.push("--output-dir".into());
        args.push(jni.display().to_string());
        args.extend(strings(&["build", "--locked", "--package"]));
        args.push(self.catalog.artifact.ffi_package.clone());
        args.push("--no-default-features".into());
        args.extend(self.cargo_feature_args(resolution));
        args.extend(Self::profile_args(&manifest.profile));
        args.push("--lib".into());
        self.runner.run(&args, &self.repo_root, &cargo_env, false)?;
        let library_name = format!("lib{}.so", self.catalog.artifact.library_stem);
        let mut libraries = Vec::new();
        for abi in &self.catalog.android.abis {
            let library = jni.join(&abi.name).join(&library_name);
            if !library.is_file() {
                return Err(refusal(format!(
                    "cargo-ndk did not produce {library_name} for Android ABI {}",
                    abi.name
                )));
            }
            libraries.push(library);
        }
        let rustc = self
            .runner
            .run(
                &strings(&["rustc", "-vV"]),
                &self.repo_root,
                &BTreeMap::new(),
                true,
            )?
            .stdout;
        let host = host_target(&rustc)?;
        let mut bind_args = strings(&["cargo", "build", "--locked", "--package"]);
        bind_args.push(self.catalog.artifact.ffi_package.clone());
        bind_args.extend(strings(&["--bin"]));
        bind_args.push(self.catalog.artifact.bindgen_bin.clone());
        bind_args.push("--no-default-features".into());
        bind_args.extend(self.cargo_feature_args(resolution));
        bind_args.extend(Self::profile_args(&manifest.profile));
        bind_args.push("--target".into());
        bind_args.push(host.clone());
        self.runner
            .run(&bind_args, &self.repo_root, &cargo_env, false)?;
        let bindgen = PathBuf::from(&cargo_env["CARGO_TARGET_DIR"])
            .join(&host)
            .join(Self::profile_dir(&manifest.profile))
            .join(&self.catalog.artifact.bindgen_bin);
        if !bindgen.is_file() {
            return Err(refusal(format!(
                "Cargo did not produce {}",
                bindgen.display()
            )));
        }
        let config = staging.join("android-uniffi.toml");
        write(
            &config,
            format!(
                "[bindings.kotlin]\nandroid = true\nkotlin_target_version = {:?}\n",
                self.catalog.android.kotlin_version
            ),
        )?;
        let generated = staging.join("android-generated");
        fs::create_dir_all(&generated)
            .map_err(|error| refusal(format!("cannot stage Android bindings: {error}")))?;
        let args = vec![
            bindgen.display().to_string(),
            "generate".into(),
            "--library".into(),
            libraries[0].display().to_string(),
            "--language".into(),
            "kotlin".into(),
            "--config".into(),
            config.display().to_string(),
            "--out-dir".into(),
            generated.display().to_string(),
            "--no-format".into(),
        ];
        self.runner.run(&args, &self.repo_root, &cargo_env, false)?;
        let binding = generated
            .join("uniffi")
            .join(&self.catalog.artifact.library_stem)
            .join(format!("{}.kt", self.catalog.artifact.library_stem));
        if !binding.is_file() {
            return Err(refusal(format!(
                "UniFFI did not produce {}",
                binding.display()
            )));
        }
        let binding_destination = output
            .join("src/main/kotlin/uniffi")
            .join(&self.catalog.artifact.library_stem);
        fs::create_dir_all(&binding_destination)
            .map_err(|error| refusal(format!("cannot stage Android binding: {error}")))?;
        copy(
            &binding,
            &binding_destination.join(binding.file_name().unwrap()),
        )?;
        let sources = self
            .catalog
            .selected_kotlin_sources(&resolution.resolved_features, "android")?;
        self.materialize_sources(&sources, output, selected, false)?;
        write_source_inventory(&output.join("nmp-kotlin-sources.json"), &sources)?;
        let selection = json!({"schema": OUTPUT_SCHEMA_VERSION, "identity": identity, "requested_features": context["requested_features"], "resolved_features": context["resolved_features"], "active_ffi_features": context["active_ffi_features"], "android": context["android"], "profile": manifest.profile, "source_sha256": context["source_sha256"]});
        let selection_bytes = serde_json::to_vec_pretty(&selection)
            .map_err(|error| refusal(format!("cannot render Android selection: {error}")))?;
        write(&output.join("nmp-native-selection.json"), &selection_bytes)?;
        write(
            &output.join("src/main/assets/nmp/selection.json"),
            &selection_bytes,
        )?;
        write(&output.join("gradle.properties"), format!("android.useAndroidX=true\norg.gradle.jvmargs=-Xmx4g -Dfile.encoding=UTF-8\nkotlin.code.style=official\nnmpGroup={}\nnmpVersion={}\nnmpArtifactId={}\nnmpNamespace={}\nnmpCompileSdk={}\nnmpMinSdk={}\nnmpNdkVersion={}\n", self.catalog.kotlin.group, self.catalog.kotlin.version, self.catalog.android.artifact_id, self.catalog.android.namespace, self.catalog.android.compile_sdk, self.catalog.android.min_sdk, self.catalog.android.ndk_version))?;
        let repository = staging.join("android-repository");
        let gradle = output.join("gradlew");
        let args = vec![
            gradle.display().to_string(),
            "--no-daemon".into(),
            "--console=plain".into(),
            "-p".into(),
            output.display().to_string(),
            format!("-PnmpRepository={}", repository.display()),
            "clean".into(),
            "assembleRelease".into(),
            "publishReleasePublicationToNmpNativeRepository".into(),
        ];
        self.runner.run(&args, &self.repo_root, &cargo_env, false)?;
        let aar = output
            .join("build/outputs/aar")
            .join(format!("{}-release.aar", self.catalog.android.project_name));
        if !aar.is_file() {
            return Err(refusal(format!("Gradle did not produce {}", aar.display())));
        }
        let artifacts = output.join("artifacts");
        fs::create_dir_all(&artifacts)
            .map_err(|error| refusal(format!("cannot stage AAR: {error}")))?;
        copy(
            &aar,
            &artifacts.join(format!(
                "{}-{}.aar",
                self.catalog.android.artifact_id, self.catalog.kotlin.version
            )),
        )?;
        if !repository.is_dir() {
            return Err(refusal(
                "Gradle did not publish the Android Maven repository",
            ));
        }
        copy_tree(&repository, &output.join("repository"))?;
        remove_if_exists(&output.join("build"))?;
        remove_if_exists(&output.join(".gradle"))?;
        Ok(())
    }

    fn materialize_sources(
        &self,
        sources: &[SourceSpec],
        output: &Path,
        selected: &BTreeSet<String>,
        swift: bool,
    ) -> Result<()> {
        let known: BTreeSet<_> = self
            .catalog
            .features
            .iter()
            .map(|feature| feature.key.clone())
            .collect();
        for source in sources {
            let original = safe_path(&self.repo_root, &source.path)?;
            let destination = if swift {
                output
                    .join("Sources")
                    .join(source.target.as_ref().expect("validated Swift target"))
                    .join(&source.destination)
            } else {
                output.join("src/main/kotlin").join(&source.destination)
            };
            let filtered = filter_source(&read(&original)?, selected, &known, &source.path)?;
            write(&destination, filtered)?;
        }
        Ok(())
    }

    fn swift_package(&self, sources: &[SourceSpec]) -> Result<String> {
        let mut names: BTreeSet<_> = sources
            .iter()
            .filter_map(|source| source.target.clone())
            .collect();
        let targets: BTreeMap<_, _> = self
            .catalog
            .apple
            .targets
            .iter()
            .map(|target| (target.name.as_str(), target))
            .collect();
        let mut pending = names.iter().cloned().collect::<Vec<_>>();
        while let Some(name) = pending.pop() {
            let target = targets
                .get(name.as_str())
                .ok_or_else(|| refusal(format!("unknown Swift target {name}")))?;
            for dependency in &target.dependencies {
                if targets.contains_key(dependency.as_str()) && names.insert(dependency.clone()) {
                    pending.push(dependency.clone());
                }
            }
        }
        let mut text = String::from("// swift-tools-version:5.9\n// Generated by nmp; edit .nmp.toml instead.\nimport PackageDescription\n\nlet package = Package(\n");
        text.push_str(&format!(
            "    name: {:?},\n    platforms: [\n",
            self.catalog.apple.package_name
        ));
        for platform in &self.catalog.apple.platforms {
            text.push_str(&format!("        {platform},\n"));
        }
        text.push_str("    ],\n    products: [\n");
        for name in &names {
            text.push_str(&format!(
                "        .library(name: {name:?}, targets: [{name:?}]),\n"
            ));
        }
        text.push_str("    ],\n    targets: [\n        .binaryTarget(\n");
        text.push_str(&format!("            name: {:?},\n            path: {:?}\n        ),\n        .target(\n            name: {:?},\n            dependencies: [{:?}],\n            linkerSettings: [\n", self.catalog.apple.binary_target, self.catalog.apple.xcframework_name, self.catalog.apple.ffi_target, self.catalog.apple.binary_target));
        for framework in &self.catalog.apple.linked_frameworks {
            text.push_str(&format!(
                "                .linkedFramework({framework:?}),\n"
            ));
        }
        text.push_str("            ]\n        ),\n");
        for name in &names {
            let dependencies = targets[name.as_str()]
                .dependencies
                .iter()
                .map(|dependency| format!("{dependency:?}"))
                .collect::<Vec<_>>()
                .join(", ");
            text.push_str(&format!("        .target(\n            name: {name:?},\n            dependencies: [{dependencies}]\n        ),\n"));
        }
        text.push_str("    ]\n)\n");
        Ok(text)
    }

    fn kotlin_gradle(&self) -> String {
        let mut text = format!("// Generated by nmp; edit .nmp.toml instead.\nplugins {{\n    kotlin(\"jvm\") version \"2.0.21\"\n}}\n\ngroup = {:?}\nversion = {:?}\n\nrepositories {{\n    mavenCentral()\n}}\n\ndependencies {{\n", self.catalog.kotlin.group, self.catalog.kotlin.version);
        for dependency in &self.catalog.kotlin.dependencies {
            text.push_str(&format!("    implementation({dependency:?})\n"));
        }
        text.push_str(&format!(
            "}}\n\nkotlin {{\n    jvmToolchain({})\n}}\n",
            self.catalog.kotlin.jvm_toolchain
        ));
        text
    }

    fn android_context(&self) -> Result<Value> {
        let spec = &self.catalog.android;
        let (sdk, ndk) = self.android_roots()?;
        let platform = sdk
            .join("platforms")
            .join(format!("android-{}", spec.compile_sdk));
        if !platform.join("android.jar").is_file() {
            return Err(refusal(format!(
                "Android SDK is missing platforms/android-{}; install it with sdkmanager",
                spec.compile_sdk
            )));
        }
        if property(
            &platform.join("source.properties"),
            "AndroidVersion.ApiLevel",
        )?
        .as_deref()
            != Some(&spec.compile_sdk.to_string())
        {
            return Err(refusal(format!(
                "Android platform {} has missing or mismatched provenance; reinstall that platform",
                spec.compile_sdk
            )));
        }
        let build_tools = sdk.join("build-tools").join(&spec.build_tools_version);
        if !build_tools.is_dir() {
            return Err(refusal(format!(
                "Android SDK is missing build-tools {}; install it with sdkmanager",
                spec.build_tools_version
            )));
        }
        if property(&build_tools.join("source.properties"), "Pkg.Revision")?.as_deref()
            != Some(spec.build_tools_version.as_str())
        {
            return Err(refusal(format!(
                "Android build-tools {} has missing or mismatched provenance; reinstall it",
                spec.build_tools_version
            )));
        }
        let ndk_version = property(&ndk.join("source.properties"), "Pkg.Revision")?
            .unwrap_or_else(|| "missing".into());
        if ndk_version != spec.ndk_version {
            return Err(refusal(format!("Android NDK {} is required; found {ndk_version} at {}. Install the required side-by-side NDK or set NMP_ANDROID_NDK_HOME", spec.ndk_version, ndk.display())));
        }
        let java_home = std::env::var("JAVA_HOME").map_err(|_| {
            refusal(format!(
                "Android preparation requires JDK {}; set JAVA_HOME to that JDK",
                spec.jdk_version
            ))
        })?;
        let java = absolute(Path::new(&java_home))?.join("bin/java");
        if !java.is_file() {
            return Err(refusal(format!(
                "JAVA_HOME does not contain bin/java: {}",
                java.display()
            )));
        }
        let java_version = self.runner.run(
            &[java.display().to_string(), "-version".into()],
            &self.repo_root,
            &BTreeMap::new(),
            true,
        )?;
        let combined = format!("{}{}", java_version.stdout, java_version.stderr);
        let java_re = regex::Regex::new(r#"version\s+"(?P<major>[0-9]+)"#).unwrap();
        let found = java_re
            .captures(&combined)
            .and_then(|capture| capture.name("major"))
            .map(|value| value.as_str())
            .unwrap_or("unknown");
        if found != spec.jdk_version.to_string() {
            return Err(refusal(format!("JDK {} is required; JAVA_HOME reports {found}. Point JAVA_HOME at the supported JDK", spec.jdk_version)));
        }
        let cargo_ndk = self.runner.run(
            &strings(&["cargo", "ndk", "--version"]),
            &self.repo_root,
            &BTreeMap::new(),
            true,
        )?;
        let combined = format!("{}{}", cargo_ndk.stdout, cargo_ndk.stderr);
        if !combined
            .split_whitespace()
            .any(|item| item == spec.cargo_ndk_version)
        {
            return Err(refusal(format!(
                "cargo-ndk {} is required; install it with `cargo install cargo-ndk --version {}`",
                spec.cargo_ndk_version, spec.cargo_ndk_version
            )));
        }
        let wrapper_properties = safe_path(&self.repo_root, &spec.gradle_wrapper_project)?
            .join("gradle/wrapper/gradle-wrapper.properties");
        let distribution = property(&wrapper_properties, "distributionUrl")?.unwrap_or_default();
        if !distribution.contains(&format!("gradle-{}-bin.zip", spec.gradle_version)) {
            return Err(refusal(format!(
                "Gradle wrapper must select {}",
                spec.gradle_version
            )));
        }
        let clang: Vec<_> = WalkDir::new(ndk.join("toolchains/llvm/prebuilt"))
            .into_iter()
            .filter_map(std::result::Result::ok)
            .map(|entry| entry.into_path())
            .filter(|path| path.ends_with("bin/clang") && path.is_file())
            .collect();
        if clang.len() != 1 {
            return Err(refusal(format!(
                "Android NDK must contain exactly one host clang; found {}",
                clang.len()
            )));
        }
        let clang_version = self
            .runner
            .run(
                &[clang[0].display().to_string(), "--version".into()],
                &self.repo_root,
                &BTreeMap::new(),
                true,
            )?
            .stdout
            .trim()
            .to_owned();
        Ok(
            json!({"project_name": spec.project_name, "namespace": spec.namespace, "maven_coordinate": format!("{}:{}:{}", self.catalog.kotlin.group, spec.artifact_id, self.catalog.kotlin.version), "min_sdk": spec.min_sdk, "compile_sdk": spec.compile_sdk, "sdk_revision": property(&platform.join("source.properties"), "Pkg.Revision")?, "build_tools": spec.build_tools_version, "ndk": spec.ndk_version, "cargo_ndk": spec.cargo_ndk_version, "gradle": spec.gradle_version, "android_gradle_plugin": spec.android_gradle_plugin_version, "kotlin": spec.kotlin_version, "jdk": combined.trim(), "clang": clang_version, "abis": spec.abis.iter().map(|abi| json!({"name": abi.name, "rust_target": abi.rust_target})).collect::<Vec<_>>()}),
        )
    }

    fn android_roots(&self) -> Result<(PathBuf, PathBuf)> {
        let spec = &self.catalog.android;
        let sdk_value = std::env::var("ANDROID_HOME")
            .or_else(|_| std::env::var("ANDROID_SDK_ROOT"))
            .map_err(|_| {
                refusal(format!(
                    "Android preparation needs an SDK containing platform {} and build-tools {}; set ANDROID_HOME to that SDK",
                    spec.compile_sdk, spec.build_tools_version
                ))
            })?;
        let sdk = absolute(Path::new(&sdk_value))?;
        let ndk = if let Ok(path) = std::env::var("NMP_ANDROID_NDK_HOME") {
            absolute(Path::new(&path))?
        } else {
            sdk.join("ndk").join(&spec.ndk_version)
        };
        Ok((sdk, ndk))
    }
}

fn strings(values: &[&str]) -> Vec<String> {
    values.iter().map(|value| (*value).to_owned()).collect()
}
fn host_target(rustc: &str) -> Result<String> {
    rustc
        .lines()
        .find_map(|line| line.strip_prefix("host: "))
        .map(str::to_owned)
        .ok_or_else(|| refusal("rustc -vV did not report a host target"))
}
fn absolute(path: &Path) -> Result<PathBuf> {
    if path.is_absolute() {
        Ok(path.to_path_buf())
    } else {
        std::env::current_dir()
            .map(|cwd| cwd.join(path))
            .map_err(|error| refusal(format!("cannot resolve {}: {error}", path.display())))
    }
}
fn copy(source: &Path, destination: &Path) -> Result<()> {
    if let Some(parent) = destination.parent() {
        fs::create_dir_all(parent)
            .map_err(|error| refusal(format!("cannot create {}: {error}", parent.display())))?;
    }
    fs::copy(source, destination).map_err(|error| {
        refusal(format!(
            "cannot copy {} to {}: {error}",
            source.display(),
            destination.display()
        ))
    })?;
    Ok(())
}
fn copy_tree(source: &Path, destination: &Path) -> Result<()> {
    for entry in WalkDir::new(source) {
        let entry =
            entry.map_err(|error| refusal(format!("cannot walk {}: {error}", source.display())))?;
        let relative = entry.path().strip_prefix(source).expect("walk root");
        let target = destination.join(relative);
        if entry.file_type().is_dir() {
            fs::create_dir_all(&target)
                .map_err(|error| refusal(format!("cannot create {}: {error}", target.display())))?;
        } else if entry.file_type().is_file() {
            copy(entry.path(), &target)?;
        }
    }
    Ok(())
}
fn remove_if_exists(path: &Path) -> Result<()> {
    if path.is_dir() {
        fs::remove_dir_all(path)
            .map_err(|error| refusal(format!("cannot remove {}: {error}", path.display())))?;
    } else if path.exists() {
        fs::remove_file(path)
            .map_err(|error| refusal(format!("cannot remove {}: {error}", path.display())))?;
    }
    Ok(())
}
fn canonical_json(value: &Value) -> Result<Vec<u8>> {
    serde_json::to_vec(value).map_err(|error| refusal(format!("cannot encode identity: {error}")))
}
fn sha256_bytes(bytes: &[u8]) -> String {
    format!("{:x}", Sha256::digest(bytes))
}
fn sha256_file(path: &Path) -> Result<String> {
    let mut hasher = Sha256::new();
    update_hasher_from_file(&mut hasher, path)?;
    Ok(format!("{:x}", hasher.finalize()))
}
fn update_hasher_from_file(hasher: &mut Sha256, path: &Path) -> Result<()> {
    let file = fs::File::open(path)
        .map_err(|error| refusal(format!("cannot hash {}: {error}", path.display())))?;
    let mut reader = BufReader::with_capacity(1024 * 1024, file);
    let mut buffer = vec![0_u8; 1024 * 1024];
    loop {
        let count = reader
            .read(&mut buffer)
            .map_err(|error| refusal(format!("cannot hash {}: {error}", path.display())))?;
        if count == 0 {
            break;
        }
        hasher.update(&buffer[..count]);
    }
    Ok(())
}
fn collect_files(root: &Path, paths: &mut BTreeSet<PathBuf>) -> Result<()> {
    for entry in WalkDir::new(root)
        .into_iter()
        .filter_entry(|entry| !matches!(entry.file_name().to_str(), Some("target" | ".git")))
    {
        let entry = entry.map_err(|error| {
            refusal(format!(
                "cannot inspect source root {}: {error}",
                root.display()
            ))
        })?;
        if entry.file_type().is_file() {
            paths.insert(entry.into_path());
        }
    }
    Ok(())
}
fn property(path: &Path, key: &str) -> Result<Option<String>> {
    if !path.is_file() {
        return Ok(None);
    }
    for line in read(path)?.lines() {
        let line = line.trim();
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        if let Some((name, value)) = line.split_once('=') {
            if name.trim() == key {
                return Ok(Some(value.trim().replace("\\:", ":")));
            }
        }
    }
    Ok(None)
}
fn jna_prefix() -> Result<&'static str> {
    match (std::env::consts::OS, std::env::consts::ARCH) {
        ("macos", "aarch64") => Ok("darwin-aarch64"),
        ("macos", "x86_64") => Ok("darwin-x86-64"),
        ("linux", "aarch64") => Ok("linux-aarch64"),
        ("linux", "x86_64") => Ok("linux-x86-64"),
        (os, arch) => Err(refusal(format!("unsupported Kotlin/JVM host {os}/{arch}"))),
    }
}
fn write_source_inventory(path: &Path, sources: &[SourceSpec]) -> Result<()> {
    let rows = sources.iter().map(|source| json!({"source": source.path, "destination": source.destination, "platforms": source.platforms})).collect::<Vec<_>>();
    write(
        path,
        serde_json::to_vec_pretty(&rows)
            .map_err(|error| refusal(format!("cannot render source inventory: {error}")))?,
    )
}
fn content_inventory(root: &Path) -> Result<Vec<ContentRecord>> {
    let mut records = Vec::new();
    for entry in WalkDir::new(root) {
        let entry = entry
            .map_err(|error| refusal(format!("cannot inventory {}: {error}", root.display())))?;
        if !entry.file_type().is_file()
            || matches!(
                entry.file_name().to_str(),
                Some(PROVENANCE_FILE | GENERATED_MARKER)
            )
        {
            continue;
        }
        records.push(ContentRecord {
            path: entry
                .path()
                .strip_prefix(root)
                .expect("inventory root")
                .to_string_lossy()
                .replace('\\', "/"),
            sha256: sha256_file(entry.path())?,
        });
    }
    records.sort_by(|left, right| left.path.cmp(&right.path));
    Ok(records)
}
fn verify_product(output: &Path, identity: &str) -> Result<()> {
    let provenance: Value =
        serde_json::from_str(&read(&output.join(PROVENANCE_FILE))?).map_err(|error| {
            refusal(format!(
                "cache entry {} has invalid provenance: {error}",
                output.display()
            ))
        })?;
    let marker = read(&output.join(GENERATED_MARKER))?;
    if provenance["identity"].as_str() != Some(identity) || marker.trim() != identity {
        return Err(refusal(format!(
            "cache entry {} has mismatched provenance",
            output.display()
        )));
    }
    let expected: Vec<ContentRecord> = serde_json::from_value(provenance["contents"].clone())
        .map_err(|error| refusal(format!("cache entry has invalid inventory: {error}")))?;
    if expected != content_inventory(output)? {
        return Err(refusal(format!(
            "cache entry {} content hash mismatch",
            output.display()
        )));
    }
    Ok(())
}

pub fn verify_prepared_product(output: &Path) -> Result<()> {
    let provenance_path = output.join(PROVENANCE_FILE);
    let provenance: Value = serde_json::from_str(&read(&provenance_path)?).map_err(|error| {
        refusal(format!(
            "prepared product {} has invalid provenance: {error}",
            output.display()
        ))
    })?;
    let identity = provenance["identity"].as_str().ok_or_else(|| {
        refusal(format!(
            "prepared product {} provenance has no identity",
            output.display()
        ))
    })?;
    verify_product(output, identity)
}
fn materialize_product(cache: &Path, output: &Path) -> Result<()> {
    let output = absolute(output)?;
    if output.parent().is_none() {
        return Err(refusal(format!(
            "refusing unsafe output path {}",
            output.display()
        )));
    }
    if output.exists() {
        if !output.is_dir() {
            return Err(refusal(format!(
                "output exists and is not a directory: {}",
                output.display()
            )));
        }
        if fs::read_dir(&output)
            .map_err(|error| refusal(format!("cannot inspect output: {error}")))?
            .next()
            .is_some()
            && !output.join(GENERATED_MARKER).is_file()
        {
            return Err(refusal(format!(
                "refusing to replace non-generated output directory {}",
                output.display()
            )));
        }
    }
    let parent = output.parent().expect("checked parent");
    fs::create_dir_all(parent)
        .map_err(|error| refusal(format!("cannot create output parent: {error}")))?;
    let staged = tempfile::Builder::new()
        .prefix(&format!(
            ".{}-",
            output
                .file_name()
                .and_then(|name| name.to_str())
                .unwrap_or("nmp")
        ))
        .tempdir_in(parent)
        .map_err(|error| refusal(format!("cannot stage output: {error}")))?;
    copy_tree(cache, staged.path())?;
    let staged_path = staged.keep();
    let backup = output.with_extension(format!("nmp-old-{}", std::process::id()));
    if output.exists() {
        if backup.exists() {
            remove_if_exists(&backup)?;
        }
        fs::rename(&output, &backup)
            .map_err(|error| refusal(format!("cannot replace output: {error}")))?;
    }
    if let Err(error) = fs::rename(&staged_path, &output) {
        if backup.exists() && !output.exists() {
            let _ = fs::rename(&backup, &output);
        }
        return Err(refusal(format!("cannot publish output: {error}")));
    }
    remove_if_exists(&backup)?;
    Ok(())
}
fn verify_resolver_lock(generated: &Path, repository: &Path) -> Result<()> {
    let generated: toml::Value = toml::from_str(&read(generated)?)
        .map_err(|error| refusal(format!("invalid generated Cargo.lock: {error}")))?;
    let repository: toml::Value = toml::from_str(&read(repository)?)
        .map_err(|error| refusal(format!("invalid repository Cargo.lock: {error}")))?;
    fn externals(value: &toml::Value) -> BTreeSet<(String, String, String, String)> {
        value
            .get("package")
            .and_then(toml::Value::as_array)
            .into_iter()
            .flatten()
            .filter_map(|package| {
                let source = package.get("source")?.as_str()?;
                Some((
                    package.get("name")?.as_str()?.into(),
                    package.get("version")?.as_str()?.into(),
                    source.into(),
                    package
                        .get("checksum")
                        .and_then(toml::Value::as_str)
                        .unwrap_or("")
                        .into(),
                ))
            })
            .collect()
    }
    let unexpected: Vec<_> = externals(&generated)
        .difference(&externals(&repository))
        .map(|(name, version, _, _)| format!("{name}@{version}"))
        .collect();
    if !unexpected.is_empty() {
        return Err(refusal(format!(
            "generated Cargo resolver selected packages outside Cargo.lock: {}",
            unexpected.join(", ")
        )));
    }
    Ok(())
}
