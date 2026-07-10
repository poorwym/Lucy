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
    let json = generate_tileset_json(source, &TilesetOptions::from_source(source))?;
    Ok(bytes_response(
        StatusCode::OK,
        "application/json",
        json.into_bytes(),
    ))
}

#[cfg(test)]
mod tests {
    use axum::body::to_bytes;
    use lucy_core::source::SourceCatalog;

    use super::*;

    #[tokio::test]
    async fn tileset_response_uses_source_geometric_error() {
        let mut catalog =
            SourceCatalog::from_yaml_str(include_str!("../../../../config/poc-sources.yaml"))
                .expect("fixture config should load");
        let mut source = catalog
            .sources
            .remove("poc_buildings")
            .expect("poc source should exist");
        source.tileset.root_geometric_error_m = 128.0;
        source.tileset.content_uri_template = "generated-content/{level}/{x}/{y}.glb".to_string();
        source.tileset.subtree_uri_template =
            "generated-subtrees/{level}/{x}/{y}.subtree".to_string();

        let response = tileset_response(&source).expect("response should generate");
        let body = to_bytes(response.into_body(), usize::MAX)
            .await
            .expect("body should read");
        let document: serde_json::Value =
            serde_json::from_slice(&body).expect("tileset JSON should parse");

        assert_eq!(document["geometricError"], 128.0);
        assert_eq!(document["root"]["geometricError"], 128.0);
        assert_eq!(
            document["root"]["content"]["uri"],
            "generated-content/{level}/{x}/{y}.glb"
        );
        assert_eq!(
            document["root"]["implicitTiling"]["subtrees"]["uri"],
            "generated-subtrees/{level}/{x}/{y}.subtree"
        );
    }
}
