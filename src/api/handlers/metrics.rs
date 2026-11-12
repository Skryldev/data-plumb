//! `GET /metrics` — Prometheus exposition.

use axum::extract::State;
use axum::http::header;
use axum::response::{IntoResponse, Response};

use crate::state::AppState;

pub async fn metrics(State(state): State<AppState>) -> Response {
    // The uptime gauge is set per scrape (dashboards panel it directly).
    crate::metrics::set_uptime(state.uptime_secs() as f64);
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
