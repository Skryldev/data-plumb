//! Request-id generation/propagation (X-Request-Id) + HTTP metrics middleware.

use axum::http::Request;
use axum::middleware::Next;
use axum::response::Response;
use tower_http::request_id::{MakeRequestUuid, PropagateRequestIdLayer, SetRequestIdLayer};

pub const REQUEST_ID_HEADER: &str = "x-request-id";

/// The concrete layer pair (set on requests, propagate to responses).
pub fn build_request_id_layers() -> (SetRequestIdLayer<MakeRequestUuid>, PropagateRequestIdLayer) {
    (
        SetRequestIdLayer::x_request_id(MakeRequestUuid),
        PropagateRequestIdLayer::x_request_id(),
    )
}

/// Per-request HTTP metrics + structured access log.
pub async fn metrics_middleware(req: Request<axum::body::Body>, next: Next) -> Response {
    let method = req.method().to_string();
    let route = crate::metrics::normalize_path(req.uri().path());
    let request_id = req
        .headers()
        .get(REQUEST_ID_HEADER)
        .and_then(|v| v.to_str().ok())
        .unwrap_or("-")
        .to_string();
    let started = std::time::Instant::now();
    let response = next.run(req).await;
    let dur = started.elapsed().as_secs_f64();
    crate::metrics::record_http_request(&method, &route, response.status().as_u16(), dur);
    tracing::info!(
        method = %method,
        path = %route,
        status = response.status().as_u16(),
        duration_ms = (dur * 1000.0).round() as u64,
        request_id = %request_id,
        "request"
    );
    response
}
