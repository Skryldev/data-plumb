//! `GET /metrics` — Prometheus exposition.

use axum::http::header;
use axum::response::{IntoResponse, Response};

pub async fn metrics() -> Response {
    let body = crate::metrics::render();
    (
        [(
            header::CONTENT_TYPE,
            "text/plain; version=0.0.4; charset=utf-8",
        )],
        body,
    )
        .into_response()
}
