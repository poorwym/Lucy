use std::collections::BTreeMap;
use std::fmt;
use std::fs;
use std::path::Path;

use serde::Deserialize;

pub mod glb;
pub mod mesh;
pub mod postgis;
pub mod server;
pub mod subtree;
pub mod tile;
pub mod tileset;

pub const DEFAULT_CONFIG_PATH: &str = "config/poc-sources.yaml";

#[derive(Debug, Deserialize)]
pub struct SourceCatalog {
    pub sources: BTreeMap<String, SourceConfig>,
}

impl SourceCatalog {
    pub fn load(path: impl AsRef<Path>) -> Result<Self, ConfigError> {
        let path = path.as_ref();
        let raw = fs::read_to_string(path).map_err(|source| ConfigError::Read {
            path: path.display().to_string(),
            source,
        })?;

        let catalog: SourceCatalog =
            serde_yaml::from_str(&raw).map_err(|source| ConfigError::Parse {
                path: path.display().to_string(),
                source,
            })?;

        catalog.validate()?;
        Ok(catalog)
    }

    pub fn validate(&self) -> Result<(), ConfigError> {
        if self.sources.is_empty() {
            return Err(ConfigError::Validation(
                "at least one source must be configured".to_string(),
            ));
        }

        for (source_id, source) in &self.sources {
            source.validate(source_id)?;
        }

        Ok(())
    }
}

#[derive(Debug, Deserialize)]
pub struct SourceConfig {
    pub connection: String,
    pub schema: String,
    pub table: String,
    pub geometry_column: String,
    pub id_column: String,
    pub srid: i32,
    pub source_model: SourceModel,
    pub vertical_reference: String,
    pub base_height_column: Option<String>,
    pub height_column: String,
    pub geometry_types: Vec<GeometryType>,
    pub bounds: SourceBounds,
    pub min_level: u8,
    pub max_level: u8,
    pub subtree_levels: u8,
    pub max_features_per_tile: u32,
    #[serde(default)]
    pub attributes: Vec<String>,
    pub material: MaterialConfig,
}

impl SourceConfig {
    fn validate(&self, source_id: &str) -> Result<(), ConfigError> {
        require_identifier(source_id, "source id")?;
        require_identifier(&self.schema, "schema")?;
        require_identifier(&self.table, "table")?;
        require_identifier(&self.geometry_column, "geometry_column")?;
        require_identifier(&self.id_column, "id_column")?;
        require_identifier(&self.height_column, "height_column")?;

        if let Some(base_height_column) = &self.base_height_column {
            require_identifier(base_height_column, "base_height_column")?;
        }

        for attribute in &self.attributes {
            require_identifier(attribute, "attribute")?;
        }

        if let Some(color_column) = &self.material.color_column {
            require_identifier(color_column, "material.color_column")?;
        }

        if self.connection.trim().is_empty() {
            return Err(ConfigError::Validation(format!(
                "{source_id}: connection must not be empty"
            )));
        }

        if self.srid != 4326 {
            return Err(ConfigError::Validation(format!(
                "{source_id}: Phase 0 only supports SRID 4326"
            )));
        }

        if self.source_model != SourceModel::ExtrudedFootprint {
            return Err(ConfigError::Validation(format!(
                "{source_id}: Phase 0 only supports extruded_footprint sources"
            )));
        }

        if self.geometry_types.is_empty() {
            return Err(ConfigError::Validation(format!(
                "{source_id}: at least one geometry type must be allowed"
            )));
        }

        if self.min_level > self.max_level {
            return Err(ConfigError::Validation(format!(
                "{source_id}: min_level must be <= max_level"
            )));
        }

        if self.subtree_levels == 0 {
            return Err(ConfigError::Validation(format!(
                "{source_id}: subtree_levels must be greater than zero"
            )));
        }

        if self.max_features_per_tile == 0 {
            return Err(ConfigError::Validation(format!(
                "{source_id}: max_features_per_tile must be greater than zero"
            )));
        }

        self.bounds.validate(source_id)?;
        Ok(())
    }

    pub fn base_height_column_or_default(&self) -> Option<&str> {
        self.base_height_column.as_deref()
    }
}

#[derive(Debug, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum SourceModel {
    ExtrudedFootprint,
}

#[derive(Debug, Deserialize, PartialEq, Eq)]
pub enum GeometryType {
    Polygon,
    MultiPolygon,
}

#[derive(Debug, Deserialize)]
pub struct SourceBounds {
    pub west: f64,
    pub south: f64,
    pub east: f64,
    pub north: f64,
    pub min_height_m: f64,
    pub max_height_m: f64,
}

impl SourceBounds {
    pub fn validate_region(&self, source_id: &str) -> Result<(), ConfigError> {
        self.validate(source_id)
    }

    fn validate(&self, source_id: &str) -> Result<(), ConfigError> {
        if self.west >= self.east {
            return Err(ConfigError::Validation(format!(
                "{source_id}: west must be less than east"
            )));
        }

        if self.south >= self.north {
            return Err(ConfigError::Validation(format!(
                "{source_id}: south must be less than north"
            )));
        }

        if self.min_height_m > self.max_height_m {
            return Err(ConfigError::Validation(format!(
                "{source_id}: min_height_m must be <= max_height_m"
            )));
        }

        Ok(())
    }
}

#[derive(Debug, Deserialize)]
pub struct MaterialConfig {
    pub color_column: Option<String>,
    pub default_base_color: [f32; 4],
}

#[derive(Debug)]
pub enum ConfigError {
    Read {
        path: String,
        source: std::io::Error,
    },
    Parse {
        path: String,
        source: serde_yaml::Error,
    },
    Validation(String),
}

impl fmt::Display for ConfigError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            ConfigError::Read { path, source } => {
                write!(f, "failed to read config {path}: {source}")
            }
            ConfigError::Parse { path, source } => {
                write!(f, "failed to parse config {path}: {source}")
            }
            ConfigError::Validation(message) => write!(f, "invalid config: {message}"),
        }
    }
}

impl std::error::Error for ConfigError {}

fn require_identifier(value: &str, field: &str) -> Result<(), ConfigError> {
    let mut chars = value.chars();
    let Some(first) = chars.next() else {
        return Err(ConfigError::Validation(format!(
            "{field} must not be empty"
        )));
    };

    if !(first == '_' || first.is_ascii_alphabetic()) {
        return Err(ConfigError::Validation(format!(
            "{field} must start with an ASCII letter or underscore"
        )));
    }

    if !chars.all(|ch| ch == '_' || ch.is_ascii_alphanumeric()) {
        return Err(ConfigError::Validation(format!(
            "{field} may only contain ASCII letters, numbers, and underscores"
        )));
    }

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn loads_phase_zero_fixture_config() {
        let config_path =
            Path::new(env!("CARGO_MANIFEST_DIR")).join("../../config/poc-sources.yaml");
        let catalog = SourceCatalog::load(config_path).expect("fixture config should load");
        let source = catalog
            .sources
            .get("poc_buildings")
            .expect("poc source should exist");

        assert_eq!(source.schema, "public");
        assert_eq!(source.table, "poc_buildings");
        assert_eq!(source.srid, 4326);
        assert_eq!(source.source_model, SourceModel::ExtrudedFootprint);
        assert_eq!(source.base_height_column.as_deref(), Some("base_height_m"));
        assert_eq!(source.height_column, "height_m");
    }
}
