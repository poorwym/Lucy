use std::sync::Arc;

use lucy_core::{ConfigError, SourceCatalog, SourceConfig};

use crate::error::{RouteError, ServerError};
use crate::settings::ServerSettings;

#[derive(Clone, Debug)]
pub struct AppState {
    catalog: Arc<SourceCatalog>,
    default_source_id: Arc<str>,
    settings: ServerSettings,
}

impl AppState {
    pub fn new(catalog: SourceCatalog, settings: ServerSettings) -> Result<Self, ServerError> {
        let default_source_id =
            catalog
                .sources
                .keys()
                .next()
                .cloned()
                .ok_or(ServerError::Config(ConfigError::Validation(
                    "at least one source must be configured".to_string(),
                )))?;

        Ok(Self {
            catalog: Arc::new(catalog),
            default_source_id: Arc::from(default_source_id),
            settings,
        })
    }

    pub(crate) fn default_source(&self) -> Result<SourceConfig, RouteError> {
        self.source(&self.default_source_id)
    }

    pub(crate) fn source(&self, source_id: &str) -> Result<SourceConfig, RouteError> {
        self.catalog
            .sources
            .get(source_id)
            .cloned()
            .ok_or_else(|| RouteError::not_found(format!("unknown source {source_id}")))
    }

    pub(crate) fn source_count(&self) -> usize {
        self.catalog.sources.len()
    }

    pub(crate) fn default_source_id(&self) -> &str {
        &self.default_source_id
    }

    pub(crate) fn config_path(&self) -> Option<String> {
        self.settings.config_path.as_deref().map(str::to_string)
    }
}
