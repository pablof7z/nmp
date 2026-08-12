mod catalog;
mod command;
mod manifest;
mod prepare;
mod source_filter;

pub use catalog::{Catalog, FeatureSpec, SourceSpec};
pub use command::{CommandOutput, CommandRunner, ProcessRunner};
pub use manifest::{AppManifest, Product};
pub use prepare::{verify_prepared_product, PrepareOptions, PrepareResult, Preparer};
pub use source_filter::filter_source;

use std::path::{Path, PathBuf};

pub const APP_MANIFEST_SCHEMA_VERSION: u32 = 1;
pub const CATALOG_SCHEMA_VERSION: u32 = 3;
pub const OUTPUT_SCHEMA_VERSION: u32 = 1;
pub const GENERATED_MARKER: &str = ".nmp-native-generated";
pub const PROVENANCE_FILE: &str = "nmp-native-provenance.json";
pub const DEFAULT_MANIFEST: &str = ".nmp.toml";
pub const DEFAULT_OUTPUT: &str = ".nmp";

#[derive(Debug, thiserror::Error)]
pub enum Error {
    #[error("{0}")]
    Refusal(String),
    #[error("cannot read {path}: {source}")]
    Read {
        path: PathBuf,
        #[source]
        source: std::io::Error,
    },
    #[error("cannot write {path}: {source}")]
    Write {
        path: PathBuf,
        #[source]
        source: std::io::Error,
    },
    #[error("command `{command}` could not start: {source}")]
    CommandStart {
        command: String,
        #[source]
        source: std::io::Error,
    },
    #[error("command failed with exit {status}: {command}{detail}")]
    CommandFailed {
        command: String,
        status: String,
        detail: String,
    },
}

pub type Result<T> = std::result::Result<T, Error>;

pub(crate) fn read(path: &Path) -> Result<String> {
    std::fs::read_to_string(path).map_err(|source| Error::Read {
        path: path.to_path_buf(),
        source,
    })
}

pub(crate) fn write(path: &Path, contents: impl AsRef<[u8]>) -> Result<()> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent).map_err(|source| Error::Write {
            path: parent.to_path_buf(),
            source,
        })?;
    }
    std::fs::write(path, contents).map_err(|source| Error::Write {
        path: path.to_path_buf(),
        source,
    })
}

pub(crate) fn refusal(message: impl Into<String>) -> Error {
    Error::Refusal(message.into())
}
