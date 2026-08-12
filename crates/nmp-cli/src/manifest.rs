use crate::{read, refusal, write, Catalog, Result, APP_MANIFEST_SCHEMA_VERSION};
use serde::{Deserialize, Serialize};
use std::collections::BTreeSet;
use std::path::{Path, PathBuf};

#[derive(Clone, Copy, Debug, Deserialize, Serialize, Eq, Ord, PartialEq, PartialOrd)]
#[serde(rename_all = "kebab-case")]
pub enum Product {
    Apple,
    Android,
    KotlinJvm,
}

impl Product {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Apple => "apple",
            Self::Android => "android",
            Self::KotlinJvm => "kotlin-jvm",
        }
    }
}

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
struct ManifestFile {
    schema: u32,
    #[serde(default)]
    capabilities: Vec<String>,
    products: Vec<Product>,
    #[serde(default = "default_profile")]
    profile: String,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    apple_targets: Vec<String>,
}

fn default_profile() -> String {
    "release".into()
}

#[derive(Clone, Debug)]
pub struct AppManifest {
    pub path: PathBuf,
    pub capabilities: Vec<String>,
    pub products: Vec<Product>,
    pub profile: String,
    pub apple_targets: Vec<String>,
}

impl AppManifest {
    pub fn new(path: PathBuf, products: Vec<Product>, capabilities: Vec<String>) -> Result<Self> {
        let mut manifest = Self {
            path,
            capabilities,
            products,
            profile: default_profile(),
            apple_targets: Vec::new(),
        };
        manifest.canonicalize()?;
        Ok(manifest)
    }

    pub fn load(path: &Path) -> Result<Self> {
        let text = read(path)?;
        let parsed: ManifestFile = toml::from_str(&text).map_err(|error| {
            refusal(format!("invalid app manifest {}: {error}", path.display()))
        })?;
        if parsed.schema != APP_MANIFEST_SCHEMA_VERSION {
            return Err(refusal(format!(
                "app manifest {} has unsupported schema {}; expected {}",
                path.display(),
                parsed.schema,
                APP_MANIFEST_SCHEMA_VERSION
            )));
        }
        let mut manifest = Self {
            path: path.canonicalize().unwrap_or_else(|_| path.to_path_buf()),
            capabilities: parsed.capabilities,
            products: parsed.products,
            profile: parsed.profile,
            apple_targets: parsed.apple_targets,
        };
        manifest.canonicalize()?;
        Ok(manifest)
    }

    fn canonicalize(&mut self) -> Result<()> {
        if self.products.is_empty() {
            return Err(refusal("app manifest products must not be empty"));
        }
        if self.profile.is_empty()
            || !self.profile.chars().enumerate().all(|(index, c)| {
                c.is_ascii_lowercase() || c.is_ascii_digit() || (c == '-' && index > 0)
            })
        {
            return Err(refusal(format!("invalid Cargo profile {:?}", self.profile)));
        }
        canonical_strings(&mut self.capabilities, "app manifest capabilities")?;
        canonical_strings(&mut self.apple_targets, "app manifest apple_targets")?;
        self.products.sort();
        if self.products.windows(2).any(|pair| pair[0] == pair[1]) {
            return Err(refusal("app manifest products contains duplicates"));
        }
        if !self.apple_targets.is_empty() && !self.products.contains(&Product::Apple) {
            return Err(refusal(
                "app manifest apple_targets requires the Apple product",
            ));
        }
        Ok(())
    }

    pub fn validate_capabilities(&self, catalog: &Catalog) -> Result<()> {
        let known: BTreeSet<_> = catalog
            .features
            .iter()
            .map(|feature| feature.key.as_str())
            .collect();
        let unknown: Vec<_> = self
            .capabilities
            .iter()
            .filter(|key| !known.contains(key.as_str()))
            .cloned()
            .collect();
        if !unknown.is_empty() {
            return Err(refusal(format!(
                "app manifest selects unknown or internal-only capabilities: {}",
                unknown.join(", ")
            )));
        }
        Ok(())
    }

    pub fn save(&self) -> Result<()> {
        let file = ManifestFile {
            schema: APP_MANIFEST_SCHEMA_VERSION,
            capabilities: self.capabilities.clone(),
            products: self.products.clone(),
            profile: self.profile.clone(),
            apple_targets: self.apple_targets.clone(),
        };
        let mut rendered = toml::to_string_pretty(&file)
            .map_err(|error| refusal(format!("cannot render app manifest: {error}")))?;
        if !rendered.ends_with('\n') {
            rendered.push('\n');
        }
        write(&self.path, rendered)
    }

    pub fn resolve_capability<'a>(catalog: &'a Catalog, input: &str) -> Result<&'a str> {
        let matches: Vec<_> = catalog
            .features
            .iter()
            .filter(|feature| feature.key == input || feature.capability == input)
            .collect();
        match matches.as_slice() {
            [feature] => Ok(feature.key.as_str()),
            [] => Err(refusal(format!(
                "unknown or internal-only capability {input:?}; run `nmp capability list`"
            ))),
            _ => Err(refusal(format!(
                "ambiguous capability {input:?} in catalog"
            ))),
        }
    }

    pub fn add_capabilities(&mut self, catalog: &Catalog, inputs: &[String]) -> Result<()> {
        for input in inputs {
            let key = Self::resolve_capability(catalog, input)?.to_owned();
            if !self.capabilities.contains(&key) {
                self.capabilities.push(key);
            }
        }
        self.capabilities.sort();
        Ok(())
    }

    pub fn remove_capabilities(&mut self, catalog: &Catalog, inputs: &[String]) -> Result<()> {
        let mut keys = BTreeSet::new();
        for input in inputs {
            keys.insert(Self::resolve_capability(catalog, input)?.to_owned());
        }
        self.capabilities.retain(|key| !keys.contains(key));
        Ok(())
    }
}

fn canonical_strings(values: &mut [String], where_: &str) -> Result<()> {
    if values.iter().any(|value| value.is_empty()) {
        return Err(refusal(format!("{where_} contains an empty value")));
    }
    values.sort();
    if values.windows(2).any(|pair| pair[0] == pair[1]) {
        return Err(refusal(format!("{where_} contains duplicates")));
    }
    Ok(())
}
