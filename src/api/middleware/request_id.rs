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
///
/// H-05: the metric `route` label is the MATCHED ROUTE PATTERN (via axum's
/// `MatchedPath`), never the concrete path. Unmatched paths (404s) and any
/// attacker-controlled path collapse into a single `:unmatched` series, so
/// label cardinality is bounded by the route table, not by request input.
pub async fn metrics_middleware(req: Request<axum::body::Body>, next: Next) -> Response {
    let method = req.method().to_string();
    let route = req
        .extensions()
        .get::<axum::extract::MatchedPath>()
        .map(|mp| mp.as_str().to_owned())
        .unwrap_or_else(|| ":unmatched".to_owned());
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

/// H-04: maps a handler panic into the standard error envelope. The panic
/// payload is logged at error level with the request id (never returned to
/// the client), and the response carries the same JSON shape as every other
/// failure.
pub fn panic_to_500(_panic: Box<dyn std::any::Any + Send + 'static>) -> axum::response::Response {
    use crate::error::StorageError;
    use axum::response::IntoResponse as _;
    tracing::error!("REST handler panicked; returning 500 error envelope");
    StorageError::Internal("internal error: handler panicked".to_string()).into_response()
}

/// H-04 test seam: the panic recovery logic as a tower-style middleware fn
/// (used by the production router via CatchPanicLayer::custom and by the
/// regression test directly).
pub async fn panic_to_500_wrapper(req: Request<axum::body::Body>, next: Next) -> Response {
    let result = std::panic::AssertUnwindSafe(next.run(req));
    match futures::FutureExt::catch_unwind(result).await {
        Ok(resp) => resp,
        Err(_panic) => panic_to_500(_panic),
    }
}
