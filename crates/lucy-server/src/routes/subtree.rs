use axum::extract::{Path as AxumPath, State};
use axum::http::StatusCode;
use axum::response::Response;

use lucy_core::source::SourceConfig;
use lucy_core::subtree::generate_root_subtree_bytes;
use lucy_core::tile::TileCoord;

use crate::error::RouteError;
use crate::response::bytes_response;
use crate::state::AppState;

use super::util::parse_tile_path;

pub(crate) async fn source_subtree(
    State(state): State<AppState>,
    AxumPath((source_id, level, x, y_file)): AxumPath<(String, String, String, String)>,
) -> Result<Response, RouteError> {
    let source = state.source(&source_id)?;
    subtree_response(&source, parse_tile_path(&level, &x, &y_file, ".subtree")?)
}

pub(crate) async fn default_subtree(
    State(state): State<AppState>,
    AxumPath((level, x, y_file)): AxumPath<(String, String, String)>,
) -> Result<Response, RouteError> {
    let source = state.default_source()?;
    subtree_response(&source, parse_tile_path(&level, &x, &y_file, ".subtree")?)
}

fn subtree_response(source: &SourceConfig, tile: TileCoord) -> Result<Response, RouteError> {
    if tile != TileCoord::root() {
        return Err(RouteError::not_found(format!(
            "source only serves the root subtree at level={} x={} y={}",
            tile.level, tile.x, tile.y
        )));
    }

    Ok(bytes_response(
        StatusCode::OK,
        "application/octet-stream",
        generate_root_subtree_bytes(source)?,
    ))
}
