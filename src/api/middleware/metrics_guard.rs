//! H-05/M-17: guards for the ops endpoints (`/metrics`).
//!
//! `/health` and `/ready` stay public on purpose — load balancers need
//! them and they leak nothing. `/metrics` is gated by `metrics.exposure`:
//! `loopback` (default, fail-closed), `api_key` (constant-time compared),
//! or `public` (dev-only, warns at boot).

use axum::body::Body;
use axum::extract::State;
use axum::http::{Request, StatusCode};
use axum::middleware::Next;
use axum::response::{IntoResponse as _, Response};

use crate::state::AppState;

fn refused() -> Response {
    (
        StatusCode::NOT_FOUND,
        crate::error::error_json(
            "not_found",
            "metrics endpoint is not exposed on this interface (metrics.exposure)",
        )
        .to_string(),
    )
        .into_response()
}

/// Loopback-only guard: non-loopback peers get a 404 (existence hidden).
pub async fn loopback_only(req: Request<Body>, next: Next) -> Response {
    let loopback = req
        .extensions()
        .get::<axum::extract::ConnectInfo<std::net::SocketAddr>>()
        .map(|c| c.0.ip().is_loopback())
        .unwrap_or(false);
    if !loopback {
        return refused();
    }
    next.run(req).await
}

/// Api-key guard reusing the constant-time comparison discipline.
pub async fn api_key_only(
    State(state): State<AppState>,
    req: Request<Body>,
    next: Next,
) -> Response {
    let provided = req
        .headers()
        .get("x-api-key")
        .and_then(|v| v.to_str().ok())
        .unwrap_or("");
    let expected = state.0.cfg.metrics.api_key.as_bytes();
    if !constant_time_eq(provided.as_bytes(), expected) {
        return refused();
    }
    next.run(req).await
}

/// Constant-time comparison (consolidation target: util::cteq, P3).
fn constant_time_eq(a: &[u8], b: &[u8]) -> bool {
    if a.len() != b.len() {
        return false;
    }
    a.iter()
        .zip(b.iter())
        .fold(0u8, |acc, (x, y)| acc | (x ^ y))
        == 0
}
