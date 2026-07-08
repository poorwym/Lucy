use axum::extract::{Path as AxumPath, State};
use axum::http::StatusCode;
use axum::response::Response;

use lucy_core::source::SourceConfig;
use lucy_core::tileset::{TilesetOptions, generate_tileset_json};

use crate::error::RouteError;
use crate::response::bytes_response;
use crate::state::AppState;

pub(crate) async fn source_tileset(
    State(state): State<AppState>,
    AxumPath(source_id): AxumPath<String>,
) -> Result<Response, RouteError> {
    tileset_response(&state.source(&source_id)?)
}

pub(crate) async fn default_tileset(State(state): State<AppState>) -> Result<Response, RouteError> {
    tileset_response(&state.default_source()?)
}

fn tileset_response(source: &SourceConfig) -> Result<Response, RouteError> {
    let json = generate_tileset_json(source, &TilesetOptions::default())?;
    Ok(bytes_response(
        StatusCode::OK,
        "application/json",
        json.into_bytes(),
    ))
}
