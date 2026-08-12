use crate::{read, refusal, Result, CATALOG_SCHEMA_VERSION};
use serde::Deserialize;
use std::collections::BTreeSet;
use std::path::{Path, PathBuf};

#[derive(Clone, Debug, Deserialize, Eq, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct SourceSpec {
    pub path: String,
    pub destination: String,
    #[serde(default)]
    pub target: Option<String>,
    #[serde(default)]
    pub platforms: Vec<String>,
}

#[derive(Clone, Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct FeatureSpec {
    pub key: String,
    pub capability: String,
    pub cargo_feature: String,
    pub ffi_sources: Vec<String>,
    pub swift_sources: Vec<SourceSpec>,
    pub kotlin_sources: Vec<SourceSpec>,
}

#[derive(Clone, Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ArtifactSpec {
    pub ffi_package: String,
    pub ffi_manifest: String,
    pub library_stem: String,
    pub bindgen_bin: String,
}

#[derive(Clone, Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SwiftTarget {
    pub name: String,
    pub dependencies: Vec<String>,
}

#[derive(Clone, Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct AppleSlice {
    pub name: String,
    pub targets: Vec<String>,
}

#[derive(Clone, Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct AppleSpec {
    pub package_name: String,
    pub xcframework_name: String,
    pub binary_target: String,
    pub ffi_target: String,
    pub macos_deployment_target: String,
    pub platforms: Vec<String>,
    pub linked_frameworks: Vec<String>,
    pub targets: Vec<SwiftTarget>,
    pub slices: Vec<AppleSlice>,
}

#[derive(Clone, Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct KotlinSpec {
    pub project_name: String,
    pub group: String,
    pub version: String,
    pub jvm_toolchain: u32,
    pub dependencies: Vec<String>,
}

#[derive(Clone, Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct AndroidAbi {
    pub name: String,
    pub rust_target: String,
}

#[derive(Clone, Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct AndroidSpec {
    pub project_template: String,
    pub gradle_wrapper_project: String,
    pub project_name: String,
    pub namespace: String,
    pub artifact_id: String,
    pub min_sdk: u32,
    pub compile_sdk: u32,
    pub build_tools_version: String,
    pub ndk_version: String,
    pub cargo_ndk_version: String,
    pub gradle_version: String,
    pub android_gradle_plugin_version: String,
    pub kotlin_version: String,
    pub jdk_version: u32,
    pub abis: Vec<AndroidAbi>,
}

#[derive(Clone, Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CoreSpec {
    pub ffi_sources: Vec<String>,
    pub swift_sources: Vec<SourceSpec>,
    pub kotlin_sources: Vec<SourceSpec>,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct CatalogFile {
    schema: u32,
    artifact: ArtifactSpec,
    apple: AppleSpec,
    kotlin: KotlinSpec,
    android: AndroidSpec,
    core: CoreSpec,
    features: Vec<FeatureSpec>,
}

#[derive(Clone, Debug)]
pub struct Catalog {
    pub path: PathBuf,
    pub artifact: ArtifactSpec,
    pub apple: AppleSpec,
    pub kotlin: KotlinSpec,
    pub android: AndroidSpec,
    pub core: CoreSpec,
    pub features: Vec<FeatureSpec>,
}

impl Catalog {
    pub fn load(path: &Path, repo_root: &Path) -> Result<Self> {
        let parsed: CatalogFile = toml::from_str(&read(path)?).map_err(|error| {
            refusal(format!(
                "invalid native catalog {}: {error}",
                path.display()
            ))
        })?;
        if parsed.schema != CATALOG_SCHEMA_VERSION {
            return Err(refusal(format!(
                "native catalog {} has unsupported schema {}; expected {}",
                path.display(),
                parsed.schema,
                CATALOG_SCHEMA_VERSION
            )));
        }
        if parsed.features.is_empty() {
            return Err(refusal(
                "native catalog contains no app-facing capabilities",
            ));
        }
        let keys: Vec<_> = parsed
            .features
            .iter()
            .map(|feature| feature.key.as_str())
            .collect();
        let mut sorted = keys.clone();
        sorted.sort_unstable();
        if keys != sorted {
            return Err(refusal("catalog.features must be in canonical key order"));
        }
        unique(keys.iter().copied(), "catalog feature keys")?;
        unique(
            parsed
                .features
                .iter()
                .map(|feature| feature.capability.as_str()),
            "catalog capability names",
        )?;
        unique(
            parsed
                .features
                .iter()
                .map(|feature| feature.cargo_feature.as_str()),
            "catalog Cargo features",
        )?;
        let catalog = Self {
            path: path.canonicalize().unwrap_or_else(|_| path.to_path_buf()),
            artifact: parsed.artifact,
            apple: parsed.apple,
            kotlin: parsed.kotlin,
            android: parsed.android,
            core: parsed.core,
            features: parsed.features,
        };
        catalog.validate(repo_root)?;
        Ok(catalog)
    }

    pub fn by_key(&self, key: &str) -> Option<&FeatureSpec> {
        self.features.iter().find(|feature| feature.key == key)
    }

    pub fn by_cargo_feature(&self, key: &str) -> Option<&FeatureSpec> {
        self.features
            .iter()
            .find(|feature| feature.cargo_feature == key)
    }

    fn validate(&self, root: &Path) -> Result<()> {
        required_file(
            root,
            &self.artifact.ffi_manifest,
            "catalog artifact manifest",
        )?;
        for path in &self.core.ffi_sources {
            required_file(root, path, "catalog core FFI source")?;
        }
        for source in self
            .core
            .swift_sources
            .iter()
            .chain(&self.core.kotlin_sources)
        {
            required_file(root, &source.path, "catalog core SDK source")?;
        }
        required_dir(
            root,
            &self.android.project_template,
            "Android project template",
        )?;
        let wrapper = safe_path(root, &self.android.gradle_wrapper_project)?;
        for path in [
            "gradlew",
            "gradlew.bat",
            "gradle/wrapper/gradle-wrapper.jar",
            "gradle/wrapper/gradle-wrapper.properties",
        ] {
            if !wrapper.join(path).is_file() {
                return Err(refusal(format!("Gradle wrapper project is missing {path}")));
            }
        }
        let target_names: BTreeSet<_> = self
            .apple
            .targets
            .iter()
            .map(|target| target.name.as_str())
            .collect();
        for source in self.core.swift_sources.iter().chain(
            self.features
                .iter()
                .flat_map(|feature| &feature.swift_sources),
        ) {
            let Some(target) = &source.target else {
                return Err(refusal(format!(
                    "Swift source {} has no target",
                    source.path
                )));
            };
            if !target_names.contains(target.as_str()) {
                return Err(refusal(format!(
                    "Swift source {} names unknown target {target}",
                    source.path
                )));
            }
        }
        Ok(())
    }

    pub fn selected_swift_sources(&self, resolved: &[FeatureSpec]) -> Result<Vec<SourceSpec>> {
        let sources = self
            .core
            .swift_sources
            .iter()
            .chain(resolved.iter().flat_map(|feature| &feature.swift_sources));
        dedupe_sources(sources.cloned(), "Swift")
    }

    pub fn selected_kotlin_sources(
        &self,
        resolved: &[FeatureSpec],
        platform: &str,
    ) -> Result<Vec<SourceSpec>> {
        let sources = self
            .core
            .kotlin_sources
            .iter()
            .chain(resolved.iter().flat_map(|feature| &feature.kotlin_sources))
            .filter(|source| {
                source.platforms.is_empty() || source.platforms.iter().any(|item| item == platform)
            });
        dedupe_sources(sources.cloned(), "Kotlin")
    }
}

pub(crate) fn safe_path(root: &Path, relative: &str) -> Result<PathBuf> {
    let relative_path = Path::new(relative);
    if relative_path.is_absolute()
        || relative_path
            .components()
            .any(|part| matches!(part, std::path::Component::ParentDir))
    {
        return Err(refusal(format!(
            "catalog path must be a safe relative path: {relative}"
        )));
    }
    Ok(root.join(relative_path))
}

fn required_file(root: &Path, relative: &str, what: &str) -> Result<()> {
    let path = safe_path(root, relative)?;
    if !path.is_file() {
        return Err(refusal(format!("{what} does not exist: {relative}")));
    }
    Ok(())
}

fn required_dir(root: &Path, relative: &str, what: &str) -> Result<()> {
    let path = safe_path(root, relative)?;
    if !path.is_dir() {
        return Err(refusal(format!("{what} does not exist: {relative}")));
    }
    Ok(())
}

fn unique<'a>(values: impl Iterator<Item = &'a str>, what: &str) -> Result<()> {
    let mut seen = BTreeSet::new();
    for value in values {
        if !seen.insert(value) {
            return Err(refusal(format!("{what} contains duplicate {value:?}")));
        }
    }
    Ok(())
}

fn dedupe_sources(
    sources: impl Iterator<Item = SourceSpec>,
    platform: &str,
) -> Result<Vec<SourceSpec>> {
    let mut by_destination: std::collections::BTreeMap<(Option<String>, String), SourceSpec> =
        std::collections::BTreeMap::new();
    for source in sources {
        let key = (source.target.clone(), source.destination.clone());
        if let Some(previous) = by_destination.get(&key) {
            if previous != &source {
                return Err(refusal(format!(
                    "{platform} catalog maps both {} and {} to {}",
                    previous.path, source.path, source.destination
                )));
            }
        }
        by_destination.insert(key, source);
    }
    Ok(by_destination.into_values().collect())
}
