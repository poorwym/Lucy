use axum::Json;
use axum::extract::State;
use serde::Serialize;

use crate::state::AppState;

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
