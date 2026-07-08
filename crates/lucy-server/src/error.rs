use std::fmt;

use axum::Json;
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use serde::Serialize;

use lucy_core::source::ConfigError;
use lucy_core::glb::GlbError;
use lucy_core::mesh::MeshError;
use lucy_core::tile::TileCoordError;

use crate::ConfigLoadError;
use crate::postgis::TileQueryError;

#[derive(Debug)]
pub enum ServerError {
    Config(ConfigError),
    ConfigLoad(ConfigLoadError),
    Io(std::io::Error),
    Runtime(String),
}

impl fmt::Display for ServerError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            ServerError::Config(error) => write!(f, "{error}"),
            ServerError::ConfigLoad(error) => write!(f, "{error}"),
            ServerError::Io(error) => write!(f, "{error}"),
            ServerError::Runtime(message) => write!(f, "{message}"),
        }
    }
}

impl std::error::Error for ServerError {}

impl From<ConfigError> for ServerError {
    fn from(error: ConfigError) -> Self {
        Self::Config(error)
    }
}

impl From<ConfigLoadError> for ServerError {
    fn from(error: ConfigLoadError) -> Self {
        Self::ConfigLoad(error)
    }
}

impl From<std::io::Error> for ServerError {
    fn from(error: std::io::Error) -> Self {
        Self::Io(error)
    }
}

#[derive(Debug)]
pub struct RouteError {
    status: StatusCode,
    code: &'static str,
    message: String,
}

impl RouteError {
    pub(crate) fn bad_request(message: impl Into<String>) -> Self {
        Self {
            status: StatusCode::BAD_REQUEST,
            code: "bad_request",
            message: message.into(),
        }
    }

    pub(crate) fn config(message: impl Into<String>) -> Self {
        Self {
            status: StatusCode::INTERNAL_SERVER_ERROR,
            code: "config_error",
            message: message.into(),
        }
    }

    pub(crate) fn not_found(message: impl Into<String>) -> Self {
        Self {
            status: StatusCode::NOT_FOUND,
            code: "not_found",
            message: message.into(),
        }
    }

    fn internal(code: &'static str, message: impl Into<String>) -> Self {
        Self {
            status: StatusCode::INTERNAL_SERVER_ERROR,
            code,
            message: message.into(),
        }
    }
}

impl IntoResponse for RouteError {
    fn into_response(self) -> Response {
        (
            self.status,
            Json(ErrorBody {
                error: ErrorDetail {
                    code: self.code,
                    message: self.message,
                },
            }),
        )
            .into_response()
    }
}

impl From<ConfigError> for RouteError {
    fn from(error: ConfigError) -> Self {
        Self::config(error.to_string())
    }
}

impl From<GlbError> for RouteError {
    fn from(error: GlbError) -> Self {
        Self::internal("glb_error", error.to_string())
    }
}

impl From<MeshError> for RouteError {
    fn from(error: MeshError) -> Self {
        Self::internal("mesh_error", error.to_string())
    }
}

impl From<TileCoordError> for RouteError {
    fn from(error: TileCoordError) -> Self {
        Self::bad_request(error.to_string())
    }
}

impl From<TileQueryError> for RouteError {
    fn from(error: TileQueryError) -> Self {
        Self::internal("postgis_error", error.to_string())
    }
}

impl From<tokio_postgres::Error> for RouteError {
    fn from(error: tokio_postgres::Error) -> Self {
        Self::from(TileQueryError::from(error))
    }
}

#[derive(Serialize)]
struct ErrorBody {
    error: ErrorDetail,
}

#[derive(Serialize)]
struct ErrorDetail {
    code: &'static str,
    message: String,
}
