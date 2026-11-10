//! Panic-recovery layer: converts handler panics into a gRPC `INTERNAL`
//! response instead of tearing down the HTTP/2 connection.
//!
//! Applied as the outermost tower layer of the tonic router. Panics inside
//! spawned streaming tasks are NOT caught here — the channel drops and the
//! client observes `UNAVAILABLE` (documented in the streaming semantics).

use tonic::body::BoxBody;
use tower_http::catch_panic::CatchPanicLayer;

/// The panic handler function type accepted by `CatchPanicLayer`.
pub type PanicHandler = fn(Box<dyn std::any::Any + Send + 'static>) -> http::Response<BoxBody>;

/// Builds the layer with a gRPC-flavoured panic response.
pub fn layer() -> CatchPanicLayer<PanicHandler> {
    CatchPanicLayer::custom(panic_response as PanicHandler)
}

/// Produces a headers-only response carrying `grpc-status: 13`. Tonic
/// clients accept `grpc-status` in headers for trailers-only replies.
fn panic_response(_panic: Box<dyn std::any::Any + Send + 'static>) -> http::Response<BoxBody> {
    tracing::error!("gRPC handler panicked; returning INTERNAL status");
    http::Response::builder()
        .status(200)
        .header("content-type", "application/grpc")
        .header("grpc-status", "13")
        .header("grpc-message", "internal error: handler panicked")
        .body(tonic::body::empty_body())
        .unwrap_or_else(|_| {
            // Building a static response cannot fail; fall back defensively.
            http::Response::new(tonic::body::empty_body())
        })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn panic_response_carries_grpc_status() {
        let resp = panic_response(Box::new(()));
        assert_eq!(resp.headers().get("grpc-status").unwrap(), "13");
        assert_eq!(
            resp.headers().get("grpc-message").unwrap(),
            "internal error: handler panicked"
        );
    }
}
