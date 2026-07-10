use axum::extract::{Path as AxumPath, State};
use axum::http::StatusCode;
use axum::response::Response;
use tokio_postgres::NoTls;

use lucy_core::source::SourceConfig;
use lucy_core::subtree::generate_subtree_bytes_with_availability;
use lucy_core::tile::TileCoord;

use crate::error::RouteError;
use crate::postgis::query_subtree_availability;
use crate::response::bytes_response;
use crate::state::AppState;

use super::util::{parse_tile_path, resolve_connection_string};

pub(crate) async fn source_subtree(
    State(state): State<AppState>,
    AxumPath((source_id, level, x, y_file)): AxumPath<(String, String, String, String)>,
) -> Result<Response, RouteError> {
    let source = state.source(&source_id)?;
    subtree_response(&source, parse_tile_path(&level, &x, &y_file, ".subtree")?).await
}

pub(crate) async fn default_subtree(
    State(state): State<AppState>,
    AxumPath((level, x, y_file)): AxumPath<(String, String, String)>,
) -> Result<Response, RouteError> {
    let source = state.default_source()?;
    subtree_response(&source, parse_tile_path(&level, &x, &y_file, ".subtree")?).await
}

async fn subtree_response(source: &SourceConfig, tile: TileCoord) -> Result<Response, RouteError> {
    if tile != TileCoord::root() {
        return Err(RouteError::not_found(format!(
            "source only serves the root subtree at level={} x={} y={}",
            tile.level, tile.x, tile.y
        )));
    }

    let connection = resolve_connection_string(&source.connection)?;
    let (client, connection_task) = tokio_postgres::connect(&connection, NoTls).await?;
    tokio::spawn(async move {
        if let Err(error) = connection_task.await {
            eprintln!("PostGIS connection error: {error}");
        }
    });
    let availability = query_subtree_availability(&client, source, tile).await?;

    Ok(bytes_response(
        StatusCode::OK,
        "application/octet-stream",
        generate_subtree_bytes_with_availability(source, tile, &availability)?,
    ))
}
