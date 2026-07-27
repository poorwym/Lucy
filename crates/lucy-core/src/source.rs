use std::collections::{BTreeMap, BTreeSet};
use std::fmt;

use serde::Deserialize;

use crate::tile::MAX_TILE_LEVEL;

pub const DEFAULT_CONFIG_PATH: &str = "lucy.yaml";
pub const DEFAULT_BASE_HEIGHT_M: f32 = 0.0;
pub const DEFAULT_CONTENT_URI_TEMPLATE: &str = "content/{level}/{x}/{y}.glb";
pub const DEFAULT_CONTENT_START_LEVEL: u8 = 0;
pub const DEFAULT_ROOT_GEOMETRIC_ERROR_M: f64 = 512.0;
pub const DEFAULT_SUBTREE_URI_TEMPLATE: &str = "subtrees/{level}/{x}/{y}.subtree";
pub const MAX_PICKABLE_FEATURES_PER_TILE: u32 = 1 << 24;
pub const MAX_SUBTREE_LEVELS: u8 = 8;

#[derive(Clone, Copy, Debug, Default, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum Compression {
    #[default]
    Meshopt,
    Draco,
    None,
}

#[derive(Clone, Debug, Deserialize)]
pub struct SourceCatalog {
    #[serde(default)]
    pub default_source: Option<String>,
    #[serde(default)]
    pub validation: ValidationConfig,
    pub sources: BTreeMap<String, SourceConfig>,
}

impl SourceCatalog {
    pub fn from_yaml_str(raw: &str) -> Result<Self, ConfigError> {
        let catalog: SourceCatalog =
            serde_yaml::from_str(raw).map_err(|source| ConfigError::Parse { source })?;

        catalog.validate()?;
        Ok(catalog)
    }

    pub fn validate(&self) -> Result<(), ConfigError> {
        if self.sources.is_empty() {
            return Err(ConfigError::Validation(
                "at least one source must be configured".to_string(),
            ));
        }

        if let Some(default_source) = &self.default_source {
            require_identifier(default_source, "default_source")?;
            if !self.sources.contains_key(default_source) {
                return Err(ConfigError::Validation(format!(
                    "default_source {default_source:?} is not present in sources"
                )));
            }
        }

        for (source_id, source) in &self.sources {
            source.validate(source_id)?;
        }

        Ok(())
    }

    pub fn default_source_id(&self) -> Option<&str> {
        self.default_source
            .as_deref()
            .or_else(|| self.sources.keys().next().map(String::as_str))
    }
}

#[derive(Clone, Copy, Debug, Default, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct ValidationConfig {
    #[serde(default)]
    pub startup: StartupValidation,
}

#[derive(Clone, Copy, Debug, Default, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum StartupValidation {
    #[default]
    Metadata,
    Full,
    None,
}

#[derive(Clone, Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SourceConfig {
    pub connection: String,
    pub schema: String,
    pub table: String,
    pub geometry_column: String,
    pub id_column: String,
    pub srid: i32,
    pub source_model: SourceModel,
    #[serde(default)]
    pub compression: Compression,
    /// Allow subtree availability to treat a feature envelope wholly covered
    /// by a tile's conservative inner polygon as proven renderable content.
    /// Operators must only enable this after auditing every surface geometry
    /// against Lucy's decode, topology, planarity, and triangulation contract.
    #[serde(default)]
    pub surface_subtree_envelope_shortcut: bool,
    /// Optional explicit operation used by the PostGIS adapter to normalize a
    /// 3D surface into Lucy's fixed EPSG:4979 geometry contract.
    #[serde(default)]
    pub coordinate_operation: Option<CoordinateOperation>,
    pub base_height_column: Option<String>,
    #[serde(default)]
    pub height_column: Option<String>,
    pub geometry_types: Vec<GeometryType>,
    pub bounds: SourceBounds,
    pub min_level: u8,
    pub max_level: u8,
    pub subtree_levels: u8,
    pub max_features_per_tile: u32,
    #[serde(default)]
    pub tileset: TilesetConfig,
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
        if let Some(base_height_column) = &self.base_height_column {
            require_identifier(base_height_column, "base_height_column")?;
        }
        if let Some(height_column) = &self.height_column {
            require_identifier(height_column, "height_column")?;
        }

        for attribute in &self.attributes {
            require_identifier(attribute, "attribute")?;
            if attribute == "featureId" {
                return Err(ConfigError::Validation(format!(
                    "{source_id}: attribute featureId is reserved for source feature identifiers"
                )));
            }
        }

        if let Some(color_column) = &self.material.color_column {
            require_identifier(color_column, "material.color_column")?;
        }
        self.material.validate(source_id)?;

        if self.connection.trim().is_empty() {
            return Err(ConfigError::Validation(format!(
                "{source_id}: connection must not be empty"
            )));
        }

        if self.srid <= 0 {
            return Err(ConfigError::Validation(format!(
                "{source_id}: srid must be a positive PostGIS spatial_ref_sys identifier"
            )));
        }

        if self.geometry_types.is_empty() {
            return Err(ConfigError::Validation(format!(
                "{source_id}: at least one geometry type must be allowed"
            )));
        }

        if self.geometry_types.iter().collect::<BTreeSet<_>>().len() != self.geometry_types.len() {
            return Err(ConfigError::Validation(format!(
                "{source_id}: geometry_types must not contain duplicates"
            )));
        }

        self.validate_geometry_strategy(source_id)?;

        if self.min_level > self.max_level {
            return Err(ConfigError::Validation(format!(
                "{source_id}: min_level must be <= max_level"
            )));
        }

        if self.min_level != 0 {
            return Err(ConfigError::Validation(format!(
                "{source_id}: implicit QUADTREE sources currently require min_level = 0"
            )));
        }

        if self.max_level > MAX_TILE_LEVEL {
            return Err(ConfigError::Validation(format!(
                "{source_id}: max_level must be <= {MAX_TILE_LEVEL} for u32 tile coordinates"
            )));
        }

        if self.subtree_levels == 0 {
            return Err(ConfigError::Validation(format!(
                "{source_id}: subtree_levels must be greater than zero"
            )));
        }

        if self.subtree_levels > MAX_SUBTREE_LEVELS {
            return Err(ConfigError::Validation(format!(
                "{source_id}: subtree_levels must be <= {MAX_SUBTREE_LEVELS}"
            )));
        }

        if self.max_features_per_tile == 0 {
            return Err(ConfigError::Validation(format!(
                "{source_id}: max_features_per_tile must be greater than zero"
            )));
        }

        if self.max_features_per_tile > MAX_PICKABLE_FEATURES_PER_TILE {
            return Err(ConfigError::Validation(format!(
                "{source_id}: max_features_per_tile must be <= {MAX_PICKABLE_FEATURES_PER_TILE} for exact FLOAT feature IDs"
            )));
        }

        self.tileset
            .validate(source_id, self.max_level, self.max_level > self.min_level)?;

        self.bounds.validate(source_id)?;
        Ok(())
    }

    fn validate_geometry_strategy(&self, source_id: &str) -> Result<(), ConfigError> {
        match self.source_model {
            SourceModel::ExtrudedFootprint => {
                if self.surface_subtree_envelope_shortcut {
                    return Err(ConfigError::Validation(format!(
                        "{source_id}: surface_subtree_envelope_shortcut is only valid for surface_geometry_z"
                    )));
                }
                if self.coordinate_operation.is_some() {
                    return Err(ConfigError::Validation(format!(
                        "{source_id}: extruded_footprint does not accept coordinate_operation; the PostGIS adapter automatically normalizes horizontal coordinates to EPSG:4326"
                    )));
                }
                if self.height_column.is_none() {
                    return Err(ConfigError::Validation(format!(
                        "{source_id}: extruded_footprint requires height_column"
                    )));
                }
                if self
                    .geometry_types
                    .iter()
                    .any(|geometry_type| geometry_type.has_z())
                {
                    return Err(ConfigError::Validation(format!(
                        "{source_id}: extruded_footprint only accepts Polygon and MultiPolygon geometry_types"
                    )));
                }
            }
            SourceModel::SurfaceGeometryZ => {
                if self.base_height_column.is_some() || self.height_column.is_some() {
                    return Err(ConfigError::Validation(format!(
                        "{source_id}: surface_geometry_z must not configure base_height_column or height_column"
                    )));
                }
                if self
                    .geometry_types
                    .iter()
                    .any(|geometry_type| !geometry_type.has_z())
                {
                    return Err(ConfigError::Validation(format!(
                        "{source_id}: surface_geometry_z only accepts PolygonZ and MultiPolygonZ geometry_types"
                    )));
                }

                match self.coordinate_operation {
                    Some(CoordinateOperation::Rdnaptrans2018Epsg1149) if self.srid != 7415 => {
                        return Err(ConfigError::Validation(format!(
                            "{source_id}: rdnaptrans2018_epsg_1149 is only valid for EPSG:7415"
                        )));
                    }
                    None if self.srid != 4979 => {
                        return Err(ConfigError::Validation(format!(
                            "{source_id}: surface_geometry_z in EPSG:{} needs an explicit supported coordinate_operation to normalize it to EPSG:4979",
                            self.srid
                        )));
                    }
                    _ => {}
                }
            }
        }

        Ok(())
    }

    pub fn base_height_column_or_default(&self) -> Option<&str> {
        self.base_height_column.as_deref()
    }

    pub fn extrusion_height_column(&self) -> Option<&str> {
        self.height_column.as_deref()
    }

    pub fn content_query_attributes(&self) -> Vec<String> {
        let mut attributes = Vec::new();

        for attribute in &self.attributes {
            push_unique_attribute(&mut attributes, attribute);
        }

        if let Some(color_column) = &self.material.color_column {
            push_unique_attribute(&mut attributes, color_column);
        }

        if self.source_model == SourceModel::ExtrudedFootprint {
            if let Some(base_height_column) = &self.base_height_column {
                push_unique_attribute(&mut attributes, base_height_column);
            }
            if let Some(height_column) = &self.height_column {
                push_unique_attribute(&mut attributes, height_column);
            }
        }

        attributes
    }
}

#[derive(Clone, Copy, Debug, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum SourceModel {
    ExtrudedFootprint,
    SurfaceGeometryZ,
}

#[derive(Clone, Copy, Debug, Deserialize, PartialEq, Eq, PartialOrd, Ord)]
pub enum GeometryType {
    Polygon,
    MultiPolygon,
    PolygonZ,
    MultiPolygonZ,
}

impl GeometryType {
    pub fn has_z(self) -> bool {
        matches!(self, Self::PolygonZ | Self::MultiPolygonZ)
    }
}

#[derive(Clone, Copy, Debug, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum CoordinateOperation {
    #[serde(rename = "rdnaptrans2018_epsg_1149")]
    Rdnaptrans2018Epsg1149,
}

#[derive(Clone, Debug, Deserialize)]
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
        for (field, value) in [
            ("west", self.west),
            ("south", self.south),
            ("east", self.east),
            ("north", self.north),
            ("min_height_m", self.min_height_m),
            ("max_height_m", self.max_height_m),
        ] {
            if !value.is_finite() {
                return Err(ConfigError::Validation(format!(
                    "{source_id}: bounds.{field} must be finite"
                )));
            }
        }

        if !(-180.0..=180.0).contains(&self.west) || !(-180.0..=180.0).contains(&self.east) {
            return Err(ConfigError::Validation(format!(
                "{source_id}: bounds west/east must be within -180..=180 degrees"
            )));
        }

        if !(-90.0..=90.0).contains(&self.south) || !(-90.0..=90.0).contains(&self.north) {
            return Err(ConfigError::Validation(format!(
                "{source_id}: bounds south/north must be within -90..=90 degrees"
            )));
        }

        if self.west >= self.east {
            return Err(ConfigError::Validation(format!(
                "{source_id}: bounds.west must be less than bounds.east; antimeridian-spanning bounds are not supported"
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

#[derive(Clone, Debug, Deserialize)]
pub struct MaterialConfig {
    pub color_column: Option<String>,
    pub default_base_color: [f32; 4],
}

impl MaterialConfig {
    fn validate(&self, source_id: &str) -> Result<(), ConfigError> {
        for (component, value) in self.default_base_color.iter().copied().enumerate() {
            if !value.is_finite() || !(0.0..=1.0).contains(&value) {
                return Err(ConfigError::Validation(format!(
                    "{source_id}: material.default_base_color component {component} must be finite and within 0..=1"
                )));
            }
        }

        Ok(())
    }
}

#[derive(Clone, Debug, Deserialize, PartialEq)]
pub struct TilesetConfig {
    #[serde(default = "default_content_start_level")]
    pub content_start_level: u8,
    #[serde(default = "default_root_geometric_error_m")]
    pub root_geometric_error_m: f64,
    #[serde(default = "default_content_uri_template")]
    pub content_uri_template: String,
    #[serde(default = "default_subtree_uri_template")]
    pub subtree_uri_template: String,
}

impl TilesetConfig {
    fn validate(
        &self,
        source_id: &str,
        max_level: u8,
        has_descendants: bool,
    ) -> Result<(), ConfigError> {
        if self.content_start_level > MAX_TILE_LEVEL {
            return Err(ConfigError::Validation(format!(
                "{source_id}: tileset.content_start_level must be <= {MAX_TILE_LEVEL}"
            )));
        }
        if self.content_start_level > max_level {
            return Err(ConfigError::Validation(format!(
                "{source_id}: tileset.content_start_level must be <= max_level ({max_level})"
            )));
        }
        if !self.root_geometric_error_m.is_finite() || self.root_geometric_error_m < 0.0 {
            return Err(ConfigError::Validation(format!(
                "{source_id}: tileset.root_geometric_error_m must be finite and nonnegative"
            )));
        }

        if has_descendants && self.root_geometric_error_m == 0.0 {
            return Err(ConfigError::Validation(format!(
                "{source_id}: tileset.root_geometric_error_m must be positive when max_level > min_level"
            )));
        }

        require_uri_template(
            &self.content_uri_template,
            source_id,
            "tileset.content_uri_template",
        )?;
        require_uri_template(
            &self.subtree_uri_template,
            source_id,
            "tileset.subtree_uri_template",
        )?;

        Ok(())
    }
}

impl Default for TilesetConfig {
    fn default() -> Self {
        Self {
            content_start_level: DEFAULT_CONTENT_START_LEVEL,
            root_geometric_error_m: DEFAULT_ROOT_GEOMETRIC_ERROR_M,
            content_uri_template: DEFAULT_CONTENT_URI_TEMPLATE.to_string(),
            subtree_uri_template: DEFAULT_SUBTREE_URI_TEMPLATE.to_string(),
        }
    }
}

fn default_content_start_level() -> u8 {
    DEFAULT_CONTENT_START_LEVEL
}

fn default_root_geometric_error_m() -> f64 {
    DEFAULT_ROOT_GEOMETRIC_ERROR_M
}

fn default_content_uri_template() -> String {
    DEFAULT_CONTENT_URI_TEMPLATE.to_string()
}

fn default_subtree_uri_template() -> String {
    DEFAULT_SUBTREE_URI_TEMPLATE.to_string()
}

fn require_uri_template(template: &str, source_id: &str, field: &str) -> Result<(), ConfigError> {
    if template.trim().is_empty() {
        return Err(ConfigError::Validation(format!(
            "{source_id}: {field} must not be empty"
        )));
    }

    for variable in ["{level}", "{x}", "{y}"] {
        if !template.contains(variable) {
            return Err(ConfigError::Validation(format!(
                "{source_id}: {field} must contain {variable}"
            )));
        }
    }

    Ok(())
}

#[derive(Debug)]
pub enum ConfigError {
    Parse { source: serde_yaml::Error },
    Validation(String),
}

impl fmt::Display for ConfigError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            ConfigError::Parse { source } => write!(f, "failed to parse config: {source}"),
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

fn push_unique_attribute(attributes: &mut Vec<String>, attribute: &str) {
    if !attributes.iter().any(|existing| existing == attribute) {
        attributes.push(attribute.to_string());
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn loads_phase_zero_fixture_config() {
        let raw = include_str!("../../../config/poc-sources.yaml");
        let catalog = SourceCatalog::from_yaml_str(raw).expect("fixture config should load");
        let source = catalog
            .sources
            .get("poc_buildings")
            .expect("poc source should exist");

        assert_eq!(source.schema, "public");
        assert_eq!(source.table, "poc_buildings");
        assert_eq!(source.srid, 4326);
        assert_eq!(source.source_model, SourceModel::ExtrudedFootprint);
        assert_eq!(source.compression, Compression::Meshopt);
        assert!(!source.surface_subtree_envelope_shortcut);
        assert_eq!(source.base_height_column.as_deref(), Some("base_height_m"));
        assert_eq!(source.height_column.as_deref(), Some("height_m"));
        assert_eq!(
            catalog.validation,
            ValidationConfig {
                startup: StartupValidation::Metadata,
            }
        );
        assert_eq!(source.tileset.root_geometric_error_m, 512.0);
        assert_eq!(
            source.tileset.content_uri_template,
            DEFAULT_CONTENT_URI_TEMPLATE
        );
        assert_eq!(
            source.tileset.subtree_uri_template,
            DEFAULT_SUBTREE_URI_TEMPLATE
        );
    }

    #[test]
    fn parses_supported_compression_backends_and_defaults_to_meshopt() {
        let raw = include_str!("../../../config/poc-sources.yaml");
        let default_catalog =
            SourceCatalog::from_yaml_str(raw).expect("omitted compression should load");
        assert!(
            default_catalog
                .sources
                .values()
                .all(|source| source.compression == Compression::Meshopt)
        );

        for (configured, expected) in [
            ("meshopt", Compression::Meshopt),
            ("draco", Compression::Draco),
            ("none", Compression::None),
        ] {
            let configured_raw = raw.replacen(
                "    source_model: extruded_footprint\n",
                &format!("    source_model: extruded_footprint\n    compression: {configured}\n"),
                1,
            );
            let catalog = SourceCatalog::from_yaml_str(&configured_raw)
                .expect("supported compression backend should load");
            assert_eq!(catalog.sources["poc_buildings"].compression, expected);
        }
    }

    #[test]
    fn rejects_unsupported_compression_backend() {
        let raw = include_str!("../../../config/poc-sources.yaml").replacen(
            "    source_model: extruded_footprint\n",
            "    source_model: extruded_footprint\n    compression: gzip\n",
            1,
        );
        let error =
            SourceCatalog::from_yaml_str(&raw).expect_err("unsupported compression should fail");

        let message = error.to_string();
        assert!(message.contains("compression"));
        assert!(message.contains("meshopt"));
        assert!(message.contains("draco"));
    }

    #[test]
    fn parses_catalog_startup_validation_modes() {
        for (configured, expected) in [
            ("metadata", StartupValidation::Metadata),
            ("full", StartupValidation::Full),
            ("none", StartupValidation::None),
        ] {
            let raw = include_str!("../../../config/poc-sources.yaml").replacen(
                "validation:\n  startup: metadata\n",
                &format!("validation:\n  startup: {configured}\n"),
                1,
            );
            let catalog = SourceCatalog::from_yaml_str(&raw)
                .expect("configured startup validation mode should load");
            assert_eq!(catalog.validation.startup, expected);
        }
    }

    #[test]
    fn defaults_startup_validation_to_metadata() {
        let raw = include_str!("../../../config/poc-sources.yaml")
            .replace("validation:\n  startup: metadata\n", "");
        let catalog = SourceCatalog::from_yaml_str(&raw)
            .expect("catalog without validation configuration should load");

        assert_eq!(catalog.validation.startup, StartupValidation::Metadata);
    }

    #[test]
    fn rejects_unknown_validation_configuration() {
        let raw = include_str!("../../../config/poc-sources.yaml").replacen(
            "validation:\n  startup: metadata\n",
            "validation:\n  cache: true\n",
            1,
        );
        let error = SourceCatalog::from_yaml_str(&raw)
            .expect_err("unsupported validation settings should be rejected");

        assert!(error.to_string().contains("cache"));
    }

    #[test]
    fn content_query_attributes_include_configured_height_columns() {
        let raw = r#"
sources:
  custom_buildings:
    connection: ${CUSTOM_DATABASE_URL}
    schema: public
    table: custom_buildings
    geometry_column: footprint
    id_column: feature_id
    srid: 4326
    source_model: extruded_footprint
    base_height_column: bottom_m
    height_column: top_delta_m
    geometry_types:
      - Polygon
    bounds:
      west: -1.0
      south: -1.0
      east: 1.0
      north: 1.0
      min_height_m: 0.0
      max_height_m: 10.0
    min_level: 0
    max_level: 1
    subtree_levels: 1
    max_features_per_tile: 10
    attributes:
      - name
      - top_delta_m
    material:
      color_column: color
      default_base_color: [1.0, 1.0, 1.0, 1.0]
"#;
        let catalog = SourceCatalog::from_yaml_str(raw).expect("config should load");
        let source = catalog
            .sources
            .get("custom_buildings")
            .expect("source should exist");

        assert_eq!(
            source.content_query_attributes(),
            vec!["name", "top_delta_m", "color", "bottom_m"]
        );
        assert_eq!(
            source.tileset,
            TilesetConfig {
                content_start_level: DEFAULT_CONTENT_START_LEVEL,
                root_geometric_error_m: DEFAULT_ROOT_GEOMETRIC_ERROR_M,
                content_uri_template: DEFAULT_CONTENT_URI_TEMPLATE.to_string(),
                subtree_uri_template: DEFAULT_SUBTREE_URI_TEMPLATE.to_string(),
            }
        );
    }

    #[test]
    fn accepts_non_4326_footprint_source_srid_for_adapter_normalization() {
        let raw =
            include_str!("../../../config/poc-sources.yaml").replace("srid: 4326", "srid: 28992");
        let catalog = SourceCatalog::from_yaml_str(&raw)
            .expect("projected footprints should be normalized by the PostGIS adapter");

        assert!(catalog.sources.values().all(|source| source.srid == 28992));
    }

    #[test]
    fn rejects_removed_coordinate_fields_instead_of_ignoring_them() {
        let raw = include_str!("../../../config/poc-sources.yaml").replacen(
            "    source_model: extruded_footprint\n",
            "    source_model: extruded_footprint\n    vertical_reference: local_ground_meters\n",
            1,
        );
        let error = SourceCatalog::from_yaml_str(&raw)
            .expect_err("removed coordinate fields should require an explicit migration");

        assert!(error.to_string().contains("vertical_reference"));
    }

    #[test]
    fn loads_surface_geometry_z_without_extrusion_columns() {
        let raw = r#"
sources:
  sibbe_lod12:
    connection: ${DATABASE_URL}
    schema: bag
    table: lod12_3d
    geometry_column: geom
    id_column: identificatie
    srid: 7415
    source_model: surface_geometry_z
    surface_subtree_envelope_shortcut: true
    coordinate_operation: rdnaptrans2018_epsg_1149
    geometry_types:
      - PolygonZ
      - MultiPolygonZ
    bounds:
      west: 5.84
      south: 50.87
      east: 5.88
      north: 50.90
      min_height_m: 40.0
      max_height_m: 180.0
    min_level: 0
    max_level: 0
    subtree_levels: 4
    max_features_per_tile: 1000
    attributes:
      - status
    material:
      default_base_color: [0.72, 0.70, 0.65, 1.0]
"#;
        let catalog = SourceCatalog::from_yaml_str(raw).expect("surface config should load");
        let source = &catalog.sources["sibbe_lod12"];

        assert_eq!(source.source_model, SourceModel::SurfaceGeometryZ);
        assert!(source.surface_subtree_envelope_shortcut);
        assert_eq!(source.base_height_column, None);
        assert_eq!(source.height_column, None);
        assert_eq!(source.content_query_attributes(), vec!["status"]);
        assert_eq!(
            source.coordinate_operation,
            Some(CoordinateOperation::Rdnaptrans2018Epsg1149)
        );
    }

    #[test]
    fn rejects_surface_subtree_envelope_shortcut_for_footprints() {
        let raw = include_str!("../../../config/poc-sources.yaml").replacen(
            "    source_model: extruded_footprint\n",
            "    source_model: extruded_footprint\n    surface_subtree_envelope_shortcut: true\n",
            1,
        );
        let error = SourceCatalog::from_yaml_str(&raw)
            .expect_err("surface-only shortcut should reject footprint sources");

        assert!(
            error
                .to_string()
                .contains("surface_subtree_envelope_shortcut is only valid")
        );
    }

    #[test]
    fn rejects_surface_geometry_without_z_or_supported_coordinate_operation() {
        let valid = r#"
sources:
  surfaces:
    connection: ${DATABASE_URL}
    schema: public
    table: surfaces
    geometry_column: geom
    id_column: id
    srid: 7415
    source_model: surface_geometry_z
    coordinate_operation: rdnaptrans2018_epsg_1149
    geometry_types: [MultiPolygonZ]
    bounds:
      west: 5.0
      south: 50.0
      east: 6.0
      north: 51.0
      min_height_m: 0.0
      max_height_m: 100.0
    min_level: 0
    max_level: 0
    subtree_levels: 1
    max_features_per_tile: 10
    attributes: []
    material:
      default_base_color: [1.0, 1.0, 1.0, 1.0]
"#;

        let no_z = valid.replace("[MultiPolygonZ]", "[MultiPolygon]");
        let error = SourceCatalog::from_yaml_str(&no_z).expect_err("2D surface should fail");
        assert!(error.to_string().contains("PolygonZ and MultiPolygonZ"));

        let no_operation =
            valid.replace("    coordinate_operation: rdnaptrans2018_epsg_1149\n", "");
        let error = SourceCatalog::from_yaml_str(&no_operation)
            .expect_err("non-4979 surface without an operation should fail");
        assert!(
            error
                .to_string()
                .contains("needs an explicit supported coordinate_operation")
        );

        let unsupported_postgis = valid.replace("srid: 7415", "srid: 28992");
        let error = SourceCatalog::from_yaml_str(&unsupported_postgis)
            .expect_err("an unsupported explicit 3D operation should fail");
        assert!(error.to_string().contains("only valid for EPSG:7415"));

        let subdivided = valid.replace("max_level: 0", "max_level: 1");
        let catalog = SourceCatalog::from_yaml_str(&subdivided)
            .expect("native surfaces should support subdivision into child tiles");
        assert_eq!(catalog.sources["surfaces"].max_level, 1);
    }

    #[test]
    fn rejects_non_finite_or_out_of_range_geodetic_bounds() {
        let valid = SourceBounds {
            west: 4.0,
            south: 50.0,
            east: 6.0,
            north: 52.0,
            min_height_m: -10.0,
            max_height_m: 100.0,
        };
        valid.validate("fixture").expect("valid bounds");

        for (field, bounds) in [
            (
                "finite",
                SourceBounds {
                    west: f64::NAN,
                    ..valid.clone()
                },
            ),
            (
                "longitude",
                SourceBounds {
                    east: 181.0,
                    ..valid.clone()
                },
            ),
            (
                "latitude",
                SourceBounds {
                    north: 91.0,
                    ..valid.clone()
                },
            ),
            (
                "height",
                SourceBounds {
                    max_height_m: f64::INFINITY,
                    ..valid.clone()
                },
            ),
        ] {
            let error = bounds.validate("fixture").expect_err(field);
            assert!(
                error.to_string().contains("bounds"),
                "{field}: unexpected error: {error}"
            );
        }
    }

    #[test]
    fn rejects_impractically_large_subtree_levels() {
        let raw = include_str!("../../../config/poc-sources.yaml")
            .replace("subtree_levels: 4", "subtree_levels: 9");
        let error = SourceCatalog::from_yaml_str(&raw).expect_err("config should be rejected");

        assert!(error.to_string().contains("subtree_levels must be <= 8"));
    }

    #[test]
    fn preserves_zero_min_level_constraint() {
        let raw = include_str!("../../../config/poc-sources.yaml")
            .replace("min_level: 0", "min_level: 1");
        let error = SourceCatalog::from_yaml_str(&raw).expect_err("config should be rejected");

        assert!(error.to_string().contains("require min_level = 0"));
    }

    #[test]
    fn rejects_levels_outside_u32_coordinate_domain() {
        let raw = include_str!("../../../config/poc-sources.yaml")
            .replace("max_level: 16", "max_level: 32");
        let error = SourceCatalog::from_yaml_str(&raw).expect_err("config should be rejected");

        assert!(error.to_string().contains("max_level must be <= 31"));
    }

    #[test]
    fn rejects_content_start_level_above_source_max_level() {
        let raw = include_str!("../../../config/poc-sources.yaml")
            .replace("content_start_level: 6", "content_start_level: 8");
        let error = SourceCatalog::from_yaml_str(&raw).expect_err("config should be rejected");

        assert!(
            error
                .to_string()
                .contains("content_start_level must be <= max_level")
        );
    }

    #[test]
    fn validates_configured_root_geometric_error() {
        let negative = include_str!("../../../config/poc-sources.yaml").replace(
            "root_geometric_error_m: 512.0",
            "root_geometric_error_m: -1.0",
        );
        let error = SourceCatalog::from_yaml_str(&negative).expect_err("config should be rejected");
        assert!(error.to_string().contains("finite and nonnegative"));

        let zero = include_str!("../../../config/poc-sources.yaml").replace(
            "root_geometric_error_m: 512.0",
            "root_geometric_error_m: 0.0",
        );
        let error = SourceCatalog::from_yaml_str(&zero).expect_err("config should be rejected");
        assert!(
            error
                .to_string()
                .contains("must be positive when max_level > min_level")
        );
    }

    #[test]
    fn validates_tileset_uri_template_placeholders() {
        for (field, original, configured) in [
            (
                "tileset.content_uri_template",
                DEFAULT_CONTENT_URI_TEMPLATE,
                "content/{level}/{x}/tile.glb",
            ),
            (
                "tileset.subtree_uri_template",
                DEFAULT_SUBTREE_URI_TEMPLATE,
                "subtrees/{level}/{y}/tree.subtree",
            ),
        ] {
            let raw =
                include_str!("../../../config/poc-sources.yaml").replace(original, configured);
            let error = SourceCatalog::from_yaml_str(&raw).expect_err("config should be rejected");

            assert!(error.to_string().contains(field));
            assert!(error.to_string().contains("must contain"));
        }
    }

    #[test]
    fn validates_default_material_color_components() {
        let raw = include_str!("../../../config/poc-sources.yaml").replace(
            "default_base_color: [0.72, 0.70, 0.65, 1.0]",
            "default_base_color: [0.72, 1.20, 0.65, 1.0]",
        );
        let error = SourceCatalog::from_yaml_str(&raw).expect_err("config should be rejected");

        assert!(
            error
                .to_string()
                .contains("material.default_base_color component 1")
        );
    }

    #[test]
    fn limits_feature_counts_to_exact_float_picking_ids() {
        let raw = include_str!("../../../config/poc-sources.yaml").replace(
            "max_features_per_tile: 1000",
            "max_features_per_tile: 16777217",
        );
        let error = SourceCatalog::from_yaml_str(&raw).expect_err("config should be rejected");

        assert!(
            error
                .to_string()
                .contains("max_features_per_tile must be <= 16777216")
        );
    }
}
