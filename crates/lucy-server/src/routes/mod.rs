mod content;
mod status;
mod subtree;
mod tileset;
mod util;

pub(crate) use content::{default_content, source_content};
pub(crate) use status::{health, metrics, root_status};
pub(crate) use subtree::{default_subtree, source_subtree};
pub(crate) use tileset::{default_tileset, source_tileset};

#[cfg(test)]
mod tests {
    use std::path::Path;

    use axum::Router;
    use axum::body::{Body, to_bytes};
    use axum::http::{HeaderValue, Request, StatusCode, header};
    use axum::response::Response;
    use lucy_core::source::SourceCatalog;
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
    async fn routes_source_scoped_tileset_subtree_and_legacy_aliases() {
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
