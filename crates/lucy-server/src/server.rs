use std::net::SocketAddr;
use std::path::Path;
use std::sync::Arc;

use axum::Router;
use axum::extract::{MatchedPath, Request};
use axum::http::{HeaderValue, header};
use axum::middleware::{self, Next};
use axum::response::Response;
use axum::routing::get;
use tower::ServiceBuilder;
use tower_http::request_id::{MakeRequestUuid, PropagateRequestIdLayer, SetRequestIdLayer};
use tower_http::trace::TraceLayer;
use tracing::{Level, Span, error, info};

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
        .layer(
            ServiceBuilder::new()
                .layer(SetRequestIdLayer::x_request_id(MakeRequestUuid))
                .layer(
                    TraceLayer::new_for_http()
                        .make_span_with(|request: &Request| {
                            let route = request
                                .extensions()
                                .get::<MatchedPath>()
                                .map(MatchedPath::as_str)
                                .unwrap_or_else(|| request.uri().path());
                            let request_id = request
                                .headers()
                                .get("x-request-id")
                                .and_then(|value| value.to_str().ok())
                                .unwrap_or("unknown");
                            tracing::span!(
                                Level::INFO,
                                "http.request",
                                request_id,
                                http.method = %request.method(),
                                http.route = route,
                                http.status_code = tracing::field::Empty,
                                latency_ms = tracing::field::Empty,
                            )
                        })
                        .on_request(())
                        .on_response(
                            |response: &Response, latency: std::time::Duration, span: &Span| {
                                let status = response.status();
                                span.record("http.status_code", status.as_u16());
                                span.record("latency_ms", latency.as_secs_f64() * 1_000.0);
                                if status.is_server_error() {
                                    error!(parent: span, "request completed");
                                } else if status.as_u16() == 409 {
                                    tracing::warn!(parent: span, "request completed");
                                } else {
                                    info!(parent: span, "request completed");
                                }
                            },
                        ),
                )
                .layer(PropagateRequestIdLayer::x_request_id())
                .layer(middleware::from_fn(add_cors)),
        ))
}

pub async fn run_server(
    config_path: impl AsRef<Path>,
    addr: SocketAddr,
) -> Result<(), ServerError> {
    let config_path = config_path.as_ref().to_path_buf();
    let catalog = crate::load_source_catalog(&config_path)?;
    info!(
        config_path = %config_path.display(),
        source_count = catalog.sources.len(),
        "source catalog loaded"
    );
    let app = build_app_with_settings(
        catalog,
        ServerSettings {
            config_path: Some(Arc::from(config_path.display().to_string())),
        },
    )?;
    let listener = tokio::net::TcpListener::bind(addr).await?;

    info!(address = %addr, config_path = %config_path.display(), "Lucy server listening");
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

#[cfg(test)]
mod tests {
    use axum::body::Body;
    use axum::http::Request;
    use tower::ServiceExt;

    use lucy_core::source::SourceCatalog;

    use super::build_app;

    #[tokio::test]
    async fn assigns_and_propagates_request_id() {
        let catalog =
            SourceCatalog::from_yaml_str(include_str!("../../../config/poc-sources.yaml"))
                .expect("fixture config should load");
        let app = build_app(catalog).expect("app should build");

        let response = app
            .oneshot(
                Request::builder()
                    .uri("/health")
                    .body(Body::empty())
                    .expect("request should build"),
            )
            .await
            .expect("request should succeed");

        let request_id = response
            .headers()
            .get("x-request-id")
            .expect("response should contain a request id");
        assert!(!request_id.as_bytes().is_empty());
    }
}
