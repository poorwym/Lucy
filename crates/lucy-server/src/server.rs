use std::net::SocketAddr;
use std::path::Path;
use std::sync::Arc;

use axum::Router;
use axum::extract::Request;
use axum::http::{HeaderValue, header};
use axum::middleware::{self, Next};
use axum::response::Response;
use axum::routing::get;

use lucy_core::source::SourceCatalog;

use crate::error::ServerError;
use crate::routes;
use crate::settings::ServerSettings;
use crate::state::AppState;

pub const DEFAULT_ADDR: &str = "127.0.0.1:8080";

pub fn build_app(catalog: SourceCatalog) -> Result<Router, ServerError> {
    build_app_with_settings(catalog, ServerSettings::default())
}

pub fn build_app_with_settings(
    catalog: SourceCatalog,
    settings: ServerSettings,
) -> Result<Router, ServerError> {
    let state = AppState::new(catalog, settings)?;

    Ok(Router::new()
        .route("/", get(routes::root_status))
        .route("/health", get(routes::health))
        .route("/metrics", get(routes::metrics))
        .route(
            "/sources/{source_id}/tileset.json",
            get(routes::source_tileset),
        )
        .route(
            "/sources/{source_id}/subtrees/{level}/{x}/{y_file}",
            get(routes::source_subtree),
        )
        .route(
            "/sources/{source_id}/content/{level}/{x}/{y_file}",
            get(routes::source_content),
        )
        .route("/tileset.json", get(routes::default_tileset))
        .route(
            "/subtrees/{level}/{x}/{y_file}",
            get(routes::default_subtree),
        )
        .route(
            "/content/{level}/{x}/{y_file}",
            get(routes::default_content),
        )
        .with_state(state)
        .layer(middleware::from_fn(add_cors)))
}

pub async fn run_server(
    config_path: impl AsRef<Path>,
    addr: SocketAddr,
) -> Result<(), ServerError> {
    let config_path = config_path.as_ref().to_path_buf();
    let catalog = crate::load_source_catalog(&config_path)?;
    let app = build_app_with_settings(
        catalog,
        ServerSettings {
            config_path: Some(Arc::from(config_path.display().to_string())),
        },
    )?;
    let listener = tokio::net::TcpListener::bind(addr).await?;

    eprintln!("Lucy server listening on http://{addr}/");
    axum::serve(listener, app).await?;
    Ok(())
}

async fn add_cors(request: Request, next: Next) -> Response {
    let mut response = next.run(request).await;
    response.headers_mut().insert(
        header::ACCESS_CONTROL_ALLOW_ORIGIN,
        HeaderValue::from_static("*"),
    );
    response
}
