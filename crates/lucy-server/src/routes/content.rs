use std::collections::BTreeMap;

use axum::extract::{Path as AxumPath, State};
use axum::http::StatusCode;
use axum::response::Response;
use tokio_postgres::NoTls;

use lucy_core::glb::{ContentFeature, encode_feature_content_tile_glb};
use lucy_core::mesh::{MeshFrame, wkb_footprint_to_extruded_mesh};
use lucy_core::source::{DEFAULT_BASE_HEIGHT_M, SourceConfig};
use lucy_core::tile::TileCoord;

use crate::error::RouteError;
use crate::postgis::{TileFeatureWkb, query_tile_geometry_wkb};
use crate::response::bytes_response;
use crate::state::AppState;

use super::util::{ensure_configured_level, parse_tile_path, resolve_connection_string};

pub(crate) async fn source_content(
    State(state): State<AppState>,
    AxumPath((source_id, level, x, y_file)): AxumPath<(String, String, String, String)>,
) -> Result<Response, RouteError> {
    let source = state.source(&source_id)?;
    content_tile_response(&source, parse_tile_path(&level, &x, &y_file, ".glb")?).await
}

pub(crate) async fn default_content(
    State(state): State<AppState>,
    AxumPath((level, x, y_file)): AxumPath<(String, String, String)>,
) -> Result<Response, RouteError> {
    let source = state.default_source()?;
    content_tile_response(&source, parse_tile_path(&level, &x, &y_file, ".glb")?).await
}

async fn content_tile_response(
    source: &SourceConfig,
    tile: TileCoord,
) -> Result<Response, RouteError> {
    ensure_configured_level(source, tile)?;
    let connection = resolve_connection_string(&source.connection)?;
    let (client, connection_task) = tokio_postgres::connect(&connection, NoTls).await?;
    tokio::spawn(async move {
        if let Err(error) = connection_task.await {
            eprintln!("PostGIS connection error: {error}");
        }
    });

    let features = query_tile_geometry_wkb(&client, source, tile).await?;
    if features.is_empty() {
        return Err(RouteError::not_found(format!(
            "tile level={} x={} y={} has no fixture features",
            tile.level, tile.x, tile.y
        )));
    }

    let frame = MeshFrame::from_source_bounds(&source.bounds);
    let mut content_features = Vec::with_capacity(features.len());
    for feature in features {
        let (base_height_m, height_m) = feature_heights(source, &feature)?;
        let mesh =
            wkb_footprint_to_extruded_mesh(&feature.geometry_wkb, frame, base_height_m, height_m)?;
        let base_color = feature_base_color(source, &feature)?;
        let properties = source
            .attributes
            .iter()
            .map(|attribute| {
                (
                    attribute.clone(),
                    feature.attributes.get(attribute).cloned().unwrap_or(None),
                )
            })
            .collect::<BTreeMap<_, _>>();
        content_features.push(ContentFeature {
            id: feature.id,
            mesh,
            base_color,
            properties,
        });
    }

    Ok(bytes_response(
        StatusCode::OK,
        "model/gltf-binary",
        encode_feature_content_tile_glb(&content_features)?,
    ))
}

fn feature_base_color(
    source: &SourceConfig,
    feature: &TileFeatureWkb,
) -> Result<[f32; 4], RouteError> {
    let Some(color_column) = source.material.color_column.as_deref() else {
        return Ok(source.material.default_base_color);
    };
    let Some(value) = feature
        .attributes
        .get(color_column)
        .and_then(Option::as_deref)
    else {
        return Ok(source.material.default_base_color);
    };

    parse_hex_color(value).map_err(|message| {
        RouteError::config(format!(
            "feature {} material color {color_column}={value:?} is invalid: {message}",
            feature.id
        ))
    })
}

fn parse_hex_color(value: &str) -> Result<[f32; 4], &'static str> {
    let value = value.trim();
    let hex = value
        .strip_prefix('#')
        .ok_or("expected #RRGGBB or #RRGGBBAA")?;
    if hex.len() != 6 && hex.len() != 8 {
        return Err("expected #RRGGBB or #RRGGBBAA");
    }

    let parse_channel = |offset: usize| {
        u8::from_str_radix(&hex[offset..offset + 2], 16)
            .map(|channel| f32::from(channel) / 255.0)
            .map_err(|_| "color contains a non-hexadecimal channel")
    };

    Ok([
        parse_channel(0)?,
        parse_channel(2)?,
        parse_channel(4)?,
        if hex.len() == 8 {
            parse_channel(6)?
        } else {
            1.0
        },
    ])
}

fn feature_heights(
    source: &SourceConfig,
    feature: &TileFeatureWkb,
) -> Result<(f32, f32), RouteError> {
    let base_height_m = match source.base_height_column_or_default() {
        Some(attribute) => {
            parse_optional_feature_f32(feature, attribute)?.unwrap_or(DEFAULT_BASE_HEIGHT_M)
        }
        None => DEFAULT_BASE_HEIGHT_M,
    };
    let height_m = parse_required_feature_f32(feature, &source.height_column)?;
    Ok((base_height_m, height_m))
}

fn parse_required_feature_f32(
    feature: &TileFeatureWkb,
    attribute: &str,
) -> Result<f32, RouteError> {
    parse_optional_feature_f32(feature, attribute)?.ok_or_else(|| {
        RouteError::config(format!(
            "feature {} is missing required attribute {attribute}",
            feature.id
        ))
    })
}

fn parse_optional_feature_f32(
    feature: &TileFeatureWkb,
    attribute: &str,
) -> Result<Option<f32>, RouteError> {
    let Some(value) = feature
        .attributes
        .get(attribute)
        .and_then(|value| value.as_deref())
    else {
        return Ok(None);
    };

    value.parse::<f32>().map(Some).map_err(|error| {
        RouteError::config(format!(
            "feature {} attribute {attribute}={value:?} is not a valid f32: {error}",
            feature.id
        ))
    })
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeMap;

    use lucy_core::source::SourceCatalog;

    use super::*;

    fn fixture_source() -> SourceConfig {
        let mut catalog =
            SourceCatalog::from_yaml_str(include_str!("../../../../config/poc-sources.yaml"))
                .expect("fixture config should load");
        catalog
            .sources
            .remove("poc_buildings")
            .expect("poc source should exist")
    }

    #[test]
    fn feature_heights_use_configured_column_names() {
        let mut source = fixture_source();
        source.base_height_column = Some("bottom_m".to_string());
        source.height_column = "height_delta_m".to_string();

        let feature = TileFeatureWkb {
            id: "42".to_string(),
            geometry_wkb: Vec::new(),
            attributes: BTreeMap::from([
                ("bottom_m".to_string(), Some("7.5".to_string())),
                ("height_delta_m".to_string(), Some("12.25".to_string())),
            ]),
        };

        assert_eq!(
            feature_heights(&source, &feature).expect("heights should parse"),
            (7.5, 12.25)
        );
    }

    #[test]
    fn feature_heights_default_missing_base_height_to_zero() {
        let mut source = fixture_source();
        source.base_height_column = None;
        source.height_column = "height_delta_m".to_string();

        let feature = TileFeatureWkb {
            id: "42".to_string(),
            geometry_wkb: Vec::new(),
            attributes: BTreeMap::from([("height_delta_m".to_string(), Some("12.25".to_string()))]),
        };

        assert_eq!(
            feature_heights(&source, &feature).expect("heights should parse"),
            (DEFAULT_BASE_HEIGHT_M, 12.25)
        );
    }

    #[test]
    fn feature_color_uses_configured_hex_and_optional_alpha() {
        let source = fixture_source();
        let feature = TileFeatureWkb {
            id: "42".to_string(),
            geometry_wkb: Vec::new(),
            attributes: BTreeMap::from([("color".to_string(), Some("#80402080".to_string()))]),
        };

        let color = feature_base_color(&source, &feature).expect("color should parse");
        assert_eq!(
            color,
            [128.0 / 255.0, 64.0 / 255.0, 32.0 / 255.0, 128.0 / 255.0]
        );
    }

    #[test]
    fn feature_color_falls_back_to_configured_default() {
        let source = fixture_source();
        let feature = TileFeatureWkb {
            id: "42".to_string(),
            geometry_wkb: Vec::new(),
            attributes: BTreeMap::from([("color".to_string(), None)]),
        };

        assert_eq!(
            feature_base_color(&source, &feature).expect("default should apply"),
            source.material.default_base_color
        );
    }

    #[test]
    fn feature_color_rejects_malformed_source_values() {
        let source = fixture_source();
        let feature = TileFeatureWkb {
            id: "42".to_string(),
            geometry_wkb: Vec::new(),
            attributes: BTreeMap::from([("color".to_string(), Some("orange".to_string()))]),
        };

        assert!(feature_base_color(&source, &feature).is_err());
        assert_eq!(
            parse_hex_color("orange"),
            Err("expected #RRGGBB or #RRGGBBAA")
        );
    }
}
