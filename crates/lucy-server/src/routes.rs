use axum::Json;
use axum::extract::{Path as AxumPath, State};
use axum::http::StatusCode;
use axum::response::Response;
use serde::Serialize;
use tokio_postgres::NoTls;

use lucy_core::SourceConfig;
use lucy_core::glb::encode_content_tile_glb;
use lucy_core::mesh::{MeshFrame, wkb_footprint_to_extruded_mesh};
use lucy_core::subtree::generate_root_subtree_bytes;
use lucy_core::tile::TileCoord;
use lucy_core::tileset::{TilesetOptions, generate_tileset_json};

use crate::error::RouteError;
use crate::postgis::{TileFeatureWkb, query_tile_geometry_wkb};
use crate::response::bytes_response;
use crate::state::AppState;

const PHASE_0_REPORT: &str = include_str!("../../../docs/phase-0-report.md");

pub(crate) async fn root_status(State(state): State<AppState>) -> Json<RootStatusBody> {
    Json(RootStatusBody {
        name: "lucy-server",
        default_source_id: state.default_source_id().to_string(),
        source_count: state.source_count(),
        routes: vec![
            "/health",
            "/metrics",
            "/sources/{source_id}/tileset.json",
            "/sources/{source_id}/subtrees/{level}/{x}/{y}.subtree",
            "/sources/{source_id}/content/{level}/{x}/{y}.glb",
        ],
    })
}

pub(crate) async fn health(State(state): State<AppState>) -> Json<HealthBody> {
    Json(HealthBody {
        status: "ok",
        source_count: state.source_count(),
    })
}

pub(crate) async fn metrics(State(state): State<AppState>) -> Json<MetricsBody> {
    Json(MetricsBody {
        source_count: state.source_count(),
        default_source_id: state.default_source_id().to_string(),
        config_path: state.config_path(),
    })
}

pub(crate) async fn phase_zero_report() -> Response {
    bytes_response(
        StatusCode::OK,
        "text/markdown; charset=utf-8",
        PHASE_0_REPORT.as_bytes().to_vec(),
    )
}

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

fn parse_tile_path(
    level: &str,
    x: &str,
    y_file: &str,
    suffix: &'static str,
) -> Result<TileCoord, RouteError> {
    let y = y_file
        .strip_suffix(suffix)
        .ok_or_else(|| RouteError::bad_request(format!("tile path must end in {suffix}")))?;

    TileCoord::new(
        parse_u8(level, "level")?,
        parse_u32(x, "x")?,
        parse_u32(y, "y")?,
    )
    .map_err(RouteError::from)
}

fn parse_u8(value: &str, field: &str) -> Result<u8, RouteError> {
    value.parse::<u8>().map_err(|error| {
        RouteError::bad_request(format!("{field} must be an unsigned integer: {error}"))
    })
}

fn parse_u32(value: &str, field: &str) -> Result<u32, RouteError> {
    value.parse::<u32>().map_err(|error| {
        RouteError::bad_request(format!("{field} must be an unsigned integer: {error}"))
    })
}

#[derive(Serialize)]
pub(crate) struct RootStatusBody {
    name: &'static str,
    default_source_id: String,
    source_count: usize,
    routes: Vec<&'static str>,
}

#[derive(Serialize)]
pub(crate) struct HealthBody {
    status: &'static str,
    source_count: usize,
}

#[derive(Serialize)]
pub(crate) struct MetricsBody {
    source_count: usize,
    default_source_id: String,
    config_path: Option<String>,
}

#[cfg(test)]
mod tests {
    use std::path::Path;

    use axum::Router;
    use axum::body::{Body, to_bytes};
    use axum::http::{HeaderValue, Request, StatusCode, header};
    use axum::response::Response;
    use lucy_core::SourceCatalog;
    use serde_json::Value;
    use tower::ServiceExt;

    use crate::server::build_app;

    fn fixture_catalog() -> SourceCatalog {
        let config_path =
            Path::new(env!("CARGO_MANIFEST_DIR")).join("../../config/poc-sources.yaml");
        crate::load_source_catalog(config_path).expect("fixture config should load")
    }

    fn fixture_app() -> Router {
        build_app(fixture_catalog()).expect("router should build")
    }

    async fn request(path: &str) -> Response {
        fixture_app()
            .oneshot(
                Request::builder()
                    .uri(path)
                    .body(Body::empty())
                    .expect("request should build"),
            )
            .await
            .expect("request should route")
    }

    async fn body_json(response: Response) -> Value {
        let body = to_bytes(response.into_body(), usize::MAX)
            .await
            .expect("body should read");
        serde_json::from_slice(&body).expect("body should be JSON")
    }

    #[tokio::test]
    async fn routes_health_metrics_and_root_status() {
        let root = request("/").await;
        assert_eq!(root.status(), StatusCode::OK);
        assert_eq!(
            root.headers().get(header::ACCESS_CONTROL_ALLOW_ORIGIN),
            Some(&HeaderValue::from_static("*"))
        );
        let root = body_json(root).await;
        assert_eq!(root["name"], "lucy-server");
        assert_eq!(root["default_source_id"], "poc_buildings");

        let health = request("/health").await;
        assert_eq!(health.status(), StatusCode::OK);
        let health = body_json(health).await;
        assert_eq!(health["status"], "ok");

        let metrics = request("/metrics").await;
        assert_eq!(metrics.status(), StatusCode::OK);
        let metrics = body_json(metrics).await;
        assert_eq!(metrics["source_count"], 1);
    }

    #[tokio::test]
    async fn routes_source_scoped_tileset_subtree_report_and_legacy_aliases() {
        let tileset = request("/sources/poc_buildings/tileset.json").await;
        assert_eq!(tileset.status(), StatusCode::OK);
        assert_eq!(
            tileset.headers().get(header::CONTENT_TYPE),
            Some(&HeaderValue::from_static("application/json"))
        );
        let body = to_bytes(tileset.into_body(), usize::MAX)
            .await
            .expect("body should read");
        assert!(String::from_utf8_lossy(&body).contains("\"implicitTiling\""));

        let legacy_tileset = request("/tileset.json").await;
        assert_eq!(legacy_tileset.status(), StatusCode::OK);

        let subtree = request("/sources/poc_buildings/subtrees/0/0/0.subtree").await;
        assert_eq!(subtree.status(), StatusCode::OK);
        assert_eq!(
            subtree.headers().get(header::CONTENT_TYPE),
            Some(&HeaderValue::from_static("application/octet-stream"))
        );
        let body = to_bytes(subtree.into_body(), usize::MAX)
            .await
            .expect("body should read");
        assert_eq!(&body[0..4], b"subt");

        let report = request("/phase-0-report.md").await;
        assert_eq!(report.status(), StatusCode::OK);
        assert_eq!(
            report.headers().get(header::CONTENT_TYPE),
            Some(&HeaderValue::from_static("text/markdown; charset=utf-8"))
        );
    }

    #[tokio::test]
    async fn route_errors_are_structured_json() {
        let missing_source = request("/sources/missing/tileset.json").await;
        assert_eq!(missing_source.status(), StatusCode::NOT_FOUND);
        let body = body_json(missing_source).await;
        assert_eq!(body["error"]["code"], "not_found");

        let bad_coord = request("/content/nope/0/0.glb").await;
        assert_eq!(bad_coord.status(), StatusCode::BAD_REQUEST);
        let body = body_json(bad_coord).await;
        assert_eq!(body["error"]["code"], "bad_request");

        let missing_subtree = request("/sources/poc_buildings/subtrees/1/0/0.subtree").await;
        assert_eq!(missing_subtree.status(), StatusCode::NOT_FOUND);
    }
}
