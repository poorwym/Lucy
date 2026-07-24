use std::net::SocketAddr;
use std::path::Path;
use std::sync::Arc;

use axum::Router;
use axum::extract::{MatchedPath, Request};
use axum::http::{HeaderValue, header};
use axum::middleware::{self, Next};
use axum::response::Response;
use axum::routing::get;
use tokio_postgres::NoTls;
use tower::ServiceBuilder;
use tower_http::request_id::{MakeRequestUuid, PropagateRequestIdLayer, SetRequestIdLayer};
use tower_http::trace::TraceLayer;
use tracing::{Level, Span, error, info, warn};

use lucy_core::source::{ConfigError, SourceCatalog, StartupValidation};

use crate::error::ServerError;
use crate::postgis::{SourceValidationError, validate_source, validate_source_metadata};
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
                            let path = request.uri().path();
                            let route = request
                                .extensions()
                                .get::<MatchedPath>()
                                .map(MatchedPath::as_str)
                                .unwrap_or(path);
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
                                http.path = path,
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
    validate_catalog_sources(&catalog).await?;
    let app = build_app_with_settings(
        catalog,
        ServerSettings {
            config_path: Some(Arc::from(config_path.display().to_string())),
        },
    )?;
    let listener = tokio::net::TcpListener::bind(addr).await?;

    info!(address = %addr, config_path = %config_path.display(), "Lucy server listening");
    axum::serve(listener, app)
        .with_graceful_shutdown(shutdown_signal())
        .await?;
    Ok(())
}

async fn shutdown_signal() {
    let ctrl_c = async {
        tokio::signal::ctrl_c()
            .await
            .expect("failed to install Ctrl+C handler");
    };

    #[cfg(unix)]
    let terminate = async {
        tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate())
            .expect("failed to install SIGTERM handler")
            .recv()
            .await;
    };

    #[cfg(not(unix))]
    let terminate = std::future::pending::<()>();

    tokio::select! {
        () = ctrl_c => {},
        () = terminate => {},
    }

    info!("shutdown signal received; draining HTTP connections");
}

pub async fn validate_catalog_sources(catalog: &SourceCatalog) -> Result<(), ServerError> {
    validate_catalog_sources_with_mode(catalog, catalog.validation.startup, None).await
}

pub async fn validate_catalog_sources_with_mode(
    catalog: &SourceCatalog,
    mode: StartupValidation,
    source_filter: Option<&str>,
) -> Result<(), ServerError> {
    if let Some(source_id) = source_filter
        && !catalog.sources.contains_key(source_id)
    {
        return Err(ConfigError::Validation(format!(
            "validation source {source_id:?} is not present in sources"
        ))
        .into());
    }
    if mode == StartupValidation::None {
        info!(?source_filter, "PostGIS source validation disabled");
        return Ok(());
    }

    for (source_id, source) in &catalog.sources {
        if source_filter.is_some_and(|filter| filter != source_id) {
            continue;
        }
        let started = std::time::Instant::now();
        let connection_string = resolve_startup_connection(source_id, &source.connection)?;
        let (client, connection) = tokio_postgres::connect(&connection_string, NoTls)
            .await
            .map_err(|source_error| SourceValidationError::Database {
                source_id: source_id.clone(),
                stage: "connection",
                source: source_error,
            })?;
        let logged_source_id = source_id.clone();
        tokio::spawn(async move {
            if let Err(connection_error) = connection.await {
                error!(
                    source_id = %logged_source_id,
                    error = %connection_error,
                    "PostGIS validation connection failed"
                );
            }
        });

        match mode {
            StartupValidation::Metadata => {
                let profile = validate_source_metadata(&client, source_id, source).await?;
                info!(
                    source_id,
                    elapsed_ms = started.elapsed().as_secs_f64() * 1_000.0,
                    declared_geometry_type = ?profile.declared_geometry_type,
                    declared_srid = ?profile.declared_srid,
                    declared_dimensions = ?profile.declared_dimensions,
                    id_not_null = profile.id_not_null,
                    geometry_not_null = profile.geometry_not_null,
                    id_unique = profile.id_unique,
                    "PostGIS source metadata and transform contract validated"
                );
                if !profile.id_not_null || !profile.id_unique || !profile.geometry_not_null {
                    warn!(
                        source_id,
                        id_not_null = profile.id_not_null,
                        geometry_not_null = profile.geometry_not_null,
                        id_unique = profile.id_unique,
                        "source constraints do not prove the complete row-level contract; use full validation for a data scan"
                    );
                }
                if profile
                    .declared_geometry_type
                    .as_deref()
                    .is_none_or(|geometry_type| geometry_type.starts_with("Geometry"))
                    || profile.declared_srid.is_none()
                    || profile.declared_dimensions.is_none()
                {
                    warn!(
                        source_id,
                        "generic geometry typmod does not prove the configured geometry contract; request-time validation remains active"
                    );
                }
            }
            StartupValidation::Full => {
                let profile = validate_source(&client, source_id, source).await?;
                info!(
                    source_id,
                    elapsed_ms = started.elapsed().as_secs_f64() * 1_000.0,
                    row_count = profile.row_count,
                    srids = ?profile.srids,
                    geometry_types = ?profile.geometry_types,
                    zm_flags = ?profile.zm_flags,
                    "PostGIS source geometry contract fully validated"
                );
            }
            StartupValidation::None => unreachable!("none mode returns before connecting"),
        }
    }
    Ok(())
}

fn resolve_startup_connection(
    source_id: &str,
    connection: &str,
) -> Result<String, SourceValidationError> {
    let trimmed = connection.trim();
    if trimmed == "${DATABASE_URL}" {
        std::env::var("DATABASE_URL").map_err(|error| SourceValidationError::ConnectionConfig {
            source_id: source_id.to_string(),
            message: format!("DATABASE_URL is required: {error}"),
        })
    } else {
        Ok(trimmed.to_string())
    }
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

    use lucy_core::source::{SourceCatalog, StartupValidation};

    use super::{build_app, validate_catalog_sources_with_mode};

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

    #[tokio::test]
    async fn none_validation_mode_does_not_connect_to_sources() {
        let catalog =
            SourceCatalog::from_yaml_str(include_str!("../../../config/poc-sources.yaml"))
                .expect("fixture config should load");

        validate_catalog_sources_with_mode(&catalog, StartupValidation::None, None)
            .await
            .expect("none mode should only validate parsed configuration");
    }

    #[tokio::test]
    async fn validation_rejects_an_unknown_source_filter_before_connecting() {
        let catalog =
            SourceCatalog::from_yaml_str(include_str!("../../../config/poc-sources.yaml"))
                .expect("fixture config should load");
        let error = validate_catalog_sources_with_mode(
            &catalog,
            StartupValidation::Metadata,
            Some("missing"),
        )
        .await
        .expect_err("unknown source filter should fail");

        assert!(error.to_string().contains("missing"));
    }
}
