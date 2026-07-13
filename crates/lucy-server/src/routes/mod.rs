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
        assert_eq!(
            metrics["source_count"],
            serde_json::json!(fixture_catalog().sources.len())
        );
    }

    #[tokio::test]
    async fn routes_source_scoped_tileset_and_legacy_alias() {
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

        let content_above_max = request("/content/17/0/0.glb").await;
        assert_eq!(content_above_max.status(), StatusCode::NOT_FOUND);
        let body = body_json(content_above_max).await;
        assert_eq!(body["error"]["code"], "not_found");
        assert!(
            body["error"]["message"]
                .as_str()
                .expect("message should be a string")
                .contains("outside configured levels")
        );

        let missing_subtree = request("/sources/poc_buildings/subtrees/1/0/0.subtree").await;
        assert_eq!(missing_subtree.status(), StatusCode::BAD_REQUEST);
        let body = body_json(missing_subtree).await;
        assert_eq!(body["error"]["code"], "bad_request");
        assert!(
            body["error"]["message"]
                .as_str()
                .expect("message should be a string")
                .contains("not a subtree root")
        );

        let invalid_subtree_coord = request("/sources/poc_buildings/subtrees/4/16/0.subtree").await;
        assert_eq!(invalid_subtree_coord.status(), StatusCode::BAD_REQUEST);
        let body = body_json(invalid_subtree_coord).await;
        assert_eq!(body["error"]["code"], "bad_request");

        let subtree_above_max = request("/sources/poc_buildings/subtrees/20/0/0.subtree").await;
        assert_eq!(subtree_above_max.status(), StatusCode::NOT_FOUND);
        let body = body_json(subtree_above_max).await;
        assert_eq!(body["error"]["code"], "not_found");
        assert!(
            body["error"]["message"]
                .as_str()
                .expect("message should be a string")
                .contains("outside configured levels")
        );
    }
}
