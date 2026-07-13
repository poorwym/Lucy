use axum::body::Body;
use axum::http::{StatusCode, header};
use axum::response::{IntoResponse, Response};

pub(crate) fn bytes_response(
    status: StatusCode,
    content_type: &'static str,
    bytes: Vec<u8>,
) -> Response {
    (
        status,
        [(header::CONTENT_TYPE, content_type)],
        Body::from(bytes),
    )
        .into_response()
}
