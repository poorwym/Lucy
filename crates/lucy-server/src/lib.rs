use std::fmt;
use std::fs;
use std::path::Path;

pub mod postgis;
pub mod server;

pub use server::{DEFAULT_POC_ADDR, PocServerError, run_poc_server};

pub fn load_source_catalog(
    path: impl AsRef<Path>,
) -> Result<lucy_core::SourceCatalog, ConfigLoadError> {
    let path = path.as_ref();
    let raw = fs::read_to_string(path).map_err(|source| ConfigLoadError::Read {
        path: path.display().to_string(),
        source,
    })?;

    lucy_core::SourceCatalog::from_yaml_str(&raw).map_err(|source| ConfigLoadError::Parse {
        path: path.display().to_string(),
        source,
    })
}

#[derive(Debug)]
pub enum ConfigLoadError {
    Read {
        path: String,
        source: std::io::Error,
    },
    Parse {
        path: String,
        source: lucy_core::ConfigError,
    },
}

impl fmt::Display for ConfigLoadError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            ConfigLoadError::Read { path, source } => {
                write!(f, "failed to read config {path}: {source}")
            }
            ConfigLoadError::Parse { path, source } => {
                write!(f, "failed to parse config {path}: {source}")
            }
        }
    }
}

impl std::error::Error for ConfigLoadError {}
