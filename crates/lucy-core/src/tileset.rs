use serde::Serialize;

use crate::mesh::MeshFrame;
use crate::source::{ConfigError, SourceConfig};
use crate::tile::TileCoord;

pub use crate::source::{
    DEFAULT_CONTENT_URI_TEMPLATE, DEFAULT_ROOT_GEOMETRIC_ERROR_M, DEFAULT_SUBTREE_URI_TEMPLATE,
};

#[derive(Debug, Clone, PartialEq)]
pub struct TilesetOptions {
    pub root_geometric_error_m: f64,
    pub content_uri_template: String,
    pub subtree_uri_template: String,
}

impl Default for TilesetOptions {
    fn default() -> Self {
        Self {
            root_geometric_error_m: DEFAULT_ROOT_GEOMETRIC_ERROR_M,
            content_uri_template: DEFAULT_CONTENT_URI_TEMPLATE.to_string(),
            subtree_uri_template: DEFAULT_SUBTREE_URI_TEMPLATE.to_string(),
        }
    }
}

impl TilesetOptions {
    pub fn from_source(source: &SourceConfig) -> Self {
        Self {
            root_geometric_error_m: source.tileset.root_geometric_error_m,
            content_uri_template: source.tileset.content_uri_template.clone(),
            subtree_uri_template: source.tileset.subtree_uri_template.clone(),
        }
    }

    pub fn geometric_error_at_level(&self, level: u8) -> Result<f64, ConfigError> {
        validate_root_geometric_error(self.root_geometric_error_m, false)?;
        Ok(self.root_geometric_error_m / 2_f64.powi(i32::from(level)))
    }
}

#[tracing::instrument(level = "debug", skip(source, options))]
pub fn generate_tileset_json(
    source: &SourceConfig,
    options: &TilesetOptions,
) -> Result<String, ConfigError> {
    let tileset = generate_tileset(source, options)?;

    let json = serde_json::to_string_pretty(&tileset).map_err(|error| {
        ConfigError::Validation(format!("failed to encode tileset JSON: {error}"))
    })?;
    tracing::debug!(output_bytes = json.len(), "tileset JSON encoded");
    Ok(json)
}

pub fn generate_tileset(
    source: &SourceConfig,
    options: &TilesetOptions,
) -> Result<Tileset, ConfigError> {
    if source.min_level != 0 {
        return Err(ConfigError::Validation(
            "Phase 0 implicit tileset generation requires min_level = 0".to_string(),
        ));
    }

    validate_root_geometric_error(
        options.root_geometric_error_m,
        source.max_level > source.min_level,
    )?;
    options.geometric_error_at_level(source.max_level)?;

    require_template(&options.content_uri_template, "content_uri_template")?;
    require_template(&options.subtree_uri_template, "subtree_uri_template")?;

    let root_region = TileCoord::root().tiles_region(&source.bounds)?.as_array();
    let available_levels = u32::from(source.max_level) + 1;

    Ok(Tileset {
        asset: Asset {
            version: "1.1".to_string(),
        },
        geometric_error: options.root_geometric_error_m,
        root: Tile {
            bounding_volume: BoundingVolume {
                region: root_region,
            },
            transform: MeshFrame::from_source_bounds(&source.bounds).enu_to_ecef_transform(),
            geometric_error: options.root_geometric_error_m,
            refine: Refine::Replace,
            content: TileContent {
                uri: options.content_uri_template.clone(),
            },
            implicit_tiling: ImplicitTiling {
                subdivision_scheme: SubdivisionScheme::Quadtree,
                available_levels,
                subtree_levels: u32::from(source.subtree_levels),
                subtrees: SubtreeTemplate {
                    uri: options.subtree_uri_template.clone(),
                },
            },
        },
    })
}

fn validate_root_geometric_error(
    root_geometric_error_m: f64,
    has_descendants: bool,
) -> Result<(), ConfigError> {
    if !root_geometric_error_m.is_finite() || root_geometric_error_m < 0.0 {
        return Err(ConfigError::Validation(
            "root_geometric_error_m must be finite and nonnegative".to_string(),
        ));
    }

    if has_descendants && root_geometric_error_m == 0.0 {
        return Err(ConfigError::Validation(
            "root_geometric_error_m must be positive when the tileset has descendants".to_string(),
        ));
    }

    Ok(())
}

#[derive(Debug, Serialize, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct Tileset {
    pub asset: Asset,
    pub geometric_error: f64,
    pub root: Tile,
}

#[derive(Debug, Serialize, PartialEq)]
pub struct Asset {
    pub version: String,
}

#[derive(Debug, Serialize, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct Tile {
    pub bounding_volume: BoundingVolume,
    pub transform: [f64; 16],
    pub geometric_error: f64,
    pub refine: Refine,
    pub content: TileContent,
    pub implicit_tiling: ImplicitTiling,
}

#[derive(Debug, Serialize, PartialEq)]
pub struct BoundingVolume {
    pub region: [f64; 6],
}

#[derive(Debug, Serialize, PartialEq)]
pub struct TileContent {
    pub uri: String,
}

#[derive(Debug, Serialize, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct ImplicitTiling {
    pub subdivision_scheme: SubdivisionScheme,
    pub available_levels: u32,
    pub subtree_levels: u32,
    pub subtrees: SubtreeTemplate,
}

#[derive(Debug, Serialize, PartialEq)]
pub struct SubtreeTemplate {
    pub uri: String,
}

#[derive(Debug, Serialize, PartialEq)]
#[serde(rename_all = "SCREAMING_SNAKE_CASE")]
pub enum Refine {
    Replace,
}

#[derive(Debug, Serialize, PartialEq)]
#[serde(rename_all = "SCREAMING_SNAKE_CASE")]
pub enum SubdivisionScheme {
    Quadtree,
}

fn require_template(template: &str, field: &str) -> Result<(), ConfigError> {
    if template.trim().is_empty() {
        return Err(ConfigError::Validation(format!(
            "{field} must not be empty"
        )));
    }

    for variable in ["{level}", "{x}", "{y}"] {
        if !template.contains(variable) {
            return Err(ConfigError::Validation(format!(
                "{field} must contain {variable}"
            )));
        }
    }

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::source::SourceCatalog;

    fn fixture_source() -> SourceConfig {
        let mut catalog =
            SourceCatalog::from_yaml_str(include_str!("../../../config/poc-sources.yaml"))
                .expect("fixture config should load");
        catalog
            .sources
            .remove("poc_buildings")
            .expect("poc source should exist")
    }

    #[test]
    fn generates_minimal_implicit_tileset_json() {
        let source = fixture_source();
        let json = generate_tileset_json(&source, &TilesetOptions::default())
            .expect("tileset should generate");
        let expected = include_str!("../tests/golden/poc_buildings_tileset.json").trim_end();

        assert_eq!(json, expected);
    }

    #[test]
    fn generated_tileset_contains_required_implicit_fields() {
        let source = fixture_source();
        let tileset =
            generate_tileset(&source, &TilesetOptions::default()).expect("tileset should generate");

        assert_eq!(tileset.asset.version, "1.1");
        assert_eq!(tileset.root.bounding_volume.region.len(), 6);
        assert_eq!(tileset.root.content.uri, "content/{level}/{x}/{y}.glb");
        assert_eq!(
            tileset.root.implicit_tiling.subdivision_scheme,
            SubdivisionScheme::Quadtree
        );
        assert_eq!(tileset.root.implicit_tiling.available_levels, 17);
        assert_eq!(tileset.root.implicit_tiling.subtree_levels, 4);
        assert_eq!(
            tileset.root.implicit_tiling.subtrees.uri,
            "subtrees/{level}/{x}/{y}.subtree"
        );
    }

    #[test]
    fn configured_root_error_drives_implicit_lod_sequence() {
        let mut source = fixture_source();
        source.tileset.root_geometric_error_m = 256.0;
        source.tileset.content_uri_template = "tiles/{level}/{x}/{y}.glb".to_string();
        source.tileset.subtree_uri_template = "trees/{level}/{x}/{y}.subtree".to_string();
        let options = TilesetOptions::from_source(&source);
        let tileset = generate_tileset(&source, &options).expect("tileset should generate");

        assert_eq!(tileset.geometric_error, 256.0);
        assert_eq!(tileset.root.geometric_error, 256.0);
        assert_eq!(options.geometric_error_at_level(0).expect("level 0"), 256.0);
        assert_eq!(options.geometric_error_at_level(1).expect("level 1"), 128.0);
        assert_eq!(options.geometric_error_at_level(4).expect("level 4"), 16.0);
        assert_eq!(
            options
                .geometric_error_at_level(source.max_level)
                .expect("leaf level"),
            0.00390625
        );
        assert_eq!(tileset.root.refine, Refine::Replace);
        assert_eq!(tileset.root.content.uri, "tiles/{level}/{x}/{y}.glb");
        assert_eq!(
            tileset.root.implicit_tiling.subtrees.uri,
            "trees/{level}/{x}/{y}.subtree"
        );
        assert_eq!(tileset.root.implicit_tiling.available_levels, 17);
        assert_eq!(tileset.root.implicit_tiling.subtree_levels, 4);
    }

    #[test]
    fn rejects_zero_error_for_a_tileset_with_descendants() {
        let source = fixture_source();
        let options = TilesetOptions {
            root_geometric_error_m: 0.0,
            ..TilesetOptions::default()
        };

        let error = generate_tileset(&source, &options).expect_err("error should be rejected");
        assert!(error.to_string().contains("positive"));
    }

    #[test]
    fn rejects_uri_templates_without_tile_variables() {
        let source = fixture_source();
        let options = TilesetOptions {
            content_uri_template: "content/root.glb".to_string(),
            ..TilesetOptions::default()
        };

        let error = generate_tileset(&source, &options).expect_err("template should be rejected");
        assert!(
            error.to_string().contains("content_uri_template"),
            "unexpected error: {error}"
        );
    }
}
