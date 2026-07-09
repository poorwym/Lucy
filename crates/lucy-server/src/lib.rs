use lucy_core::source::{ConfigError, SourceCatalog};
use std::fmt;
use std::fs;
use std::path::Path;

pub mod error;
pub mod postgis;
mod response;
pub mod routes;
pub mod server;
pub mod settings;
pub mod state;

pub use error::{RouteError, ServerError};
pub use server::{DEFAULT_ADDR, build_app, build_app_with_settings, run_server};
pub use settings::ServerSettings;
pub use state::AppState;

pub const DEFAULT_POC_ADDR: &str = DEFAULT_ADDR;
pub type PocServerError = ServerError;

pub fn load_source_catalog(path: impl AsRef<Path>) -> Result<SourceCatalog, ConfigLoadError> {
    let path = path.as_ref();
    let raw = fs::read_to_string(path).map_err(|source| ConfigLoadError::Read {
        path: path.display().to_string(),
        source,
    })?;

    SourceCatalog::from_yaml_str(&raw).map_err(|source| ConfigLoadError::Parse {
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
        source: ConfigError,
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
