use axum::extract::{Path as AxumPath, State};
use axum::http::StatusCode;
use axum::response::Response;
use tokio_postgres::NoTls;
use tracing::{debug, error};

use lucy_core::source::SourceConfig;
use lucy_core::subtree::generate_subtree_bytes_with_availability;
use lucy_core::tile::TileCoord;

use crate::error::RouteError;
use crate::postgis::query_subtree_availability;
use crate::response::bytes_response;
use crate::state::AppState;

use super::util::{
    ensure_configured_level, ensure_subtree_root, parse_tile_path, resolve_connection_string,
};

pub(crate) async fn source_subtree(
    State(state): State<AppState>,
    AxumPath((source_id, level, x, y_file)): AxumPath<(String, String, String, String)>,
) -> Result<Response, RouteError> {
    let source = state.source(&source_id)?;
    subtree_response(
        &source_id,
        &source,
        parse_tile_path(&level, &x, &y_file, ".subtree")?,
    )
    .await
}

pub(crate) async fn default_subtree(
    State(state): State<AppState>,
    AxumPath((level, x, y_file)): AxumPath<(String, String, String)>,
) -> Result<Response, RouteError> {
    let source = state.default_source()?;
    subtree_response(
        state.default_source_id(),
        &source,
        parse_tile_path(&level, &x, &y_file, ".subtree")?,
    )
    .await
}

#[tracing::instrument(
    name = "subtree",
    skip(source),
    fields(tile.level = tile.level, tile.x = tile.x, tile.y = tile.y)
)]
async fn subtree_response(
    source_id: &str,
    source: &SourceConfig,
    tile: TileCoord,
) -> Result<Response, RouteError> {
    ensure_configured_level(source, tile)?;
    ensure_subtree_root(source, tile)?;

    let connection = resolve_connection_string(&source.connection)?;
    let started = Instant::now();
    let (client, connection_task) = tokio_postgres::connect(&connection, NoTls).await?;
    debug!(
        duration_ms = started.elapsed().as_secs_f64() * 1_000.0,
        "PostGIS connection established"
    );
    tokio::spawn(async move {
        if let Err(error) = connection_task.await {
            error!(error = %error, "PostGIS connection task failed");
        }
    });
    let started = Instant::now();
    let availability = query_subtree_availability(&client, source, tile).await?;
    debug!(
        duration_ms = started.elapsed().as_secs_f64() * 1_000.0,
        available_tile_count = availability.tile.iter().filter(|&&value| value).count(),
        available_content_count = availability.content.iter().filter(|&&value| value).count(),
        available_child_subtree_count = availability
            .child_subtree
            .iter()
            .filter(|&&value| value)
            .count(),
        "subtree availability queried"
    );
    if !availability.tile[0] {
        return Err(RouteError::not_found(format!(
            "subtree level={} x={} y={} has no available tiles",
            tile.level, tile.x, tile.y
        )));
    }

    let started = Instant::now();
    let bytes = generate_subtree_bytes_with_availability(source, tile, &availability)?;
    debug!(
        duration_ms = started.elapsed().as_secs_f64() * 1_000.0,
        response_bytes = bytes.len(),
        "subtree encoded"
    );
    Ok(bytes_response(
        StatusCode::OK,
        "application/octet-stream",
        bytes,
    ))
}
use std::time::Instant;
