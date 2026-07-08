use axum::extract::{Path as AxumPath, State};
use axum::http::StatusCode;
use axum::response::Response;
use tokio_postgres::NoTls;

use lucy_core::SourceConfig;
use lucy_core::glb::encode_content_tile_glb;
use lucy_core::mesh::{MeshFrame, wkb_footprint_to_extruded_mesh};
use lucy_core::tile::TileCoord;

use crate::error::RouteError;
use crate::postgis::{TileFeatureWkb, query_tile_geometry_wkb};
use crate::response::bytes_response;
use crate::state::AppState;

use super::tile_path::parse_tile_path;

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
    let mut meshes = Vec::with_capacity(features.len());
    for feature in features {
        let (base_height_m, height_m) = feature_heights(&feature)?;
        meshes.push(wkb_footprint_to_extruded_mesh(
            &feature.geometry_wkb,
            frame,
            base_height_m,
            height_m,
        )?);
    }

    Ok(bytes_response(
        StatusCode::OK,
        "model/gltf-binary",
        encode_content_tile_glb(&meshes)?,
    ))
}

fn feature_heights(feature: &TileFeatureWkb) -> Result<(f32, f32), RouteError> {
    let base_height_m = parse_optional_feature_f32(feature, "base_height_m")?.unwrap_or(0.0);
    let height_m = parse_required_feature_f32(feature, "height_m")?;
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

fn resolve_connection_string(connection: &str) -> Result<String, RouteError> {
    let trimmed = connection.trim();
    if trimmed == "${DATABASE_URL}" {
        std::env::var("DATABASE_URL").map_err(|error| {
            RouteError::config(format!(
                "DATABASE_URL is required by source connection: {error}"
            ))
        })
    } else {
        Ok(trimmed.to_string())
    }
}
