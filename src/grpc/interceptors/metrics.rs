//! RPC metrics layer: records duration, gRPC status code and method for
//! every call (unary and streaming) by wrapping the whole tonic router.
//!
//! Error statuses are sent as "Trailers-Only" responses whose `grpc-status`
//! lives in the *headers*; successful calls carry it in trailers. So a
//! header read yields the real code on failures and 0 (OK) on success.

use std::future::Future;
use std::pin::Pin;
use std::task::{Context, Poll};
use std::time::Instant;

use http_body::Body;
use tonic::body::BoxBody;
use tower::{Layer, Service};

/// Grpc status code names for the metric label.
fn code_name(code: i32) -> &'static str {
    match code {
        0 => "ok",
        1 => "canceled",
        2 => "unknown",
        3 => "invalid_argument",
        4 => "deadline_exceeded",
        5 => "not_found",
        6 => "already_exists",
        7 => "permission_denied",
        8 => "resource_exhausted",
        9 => "failed_precondition",
        10 => "aborted",
        11 => "out_of_range",
        12 => "unimplemented",
        13 => "internal",
        14 => "unavailable",
        15 => "data_loss",
        16 => "unauthenticated",
        _ => "unknown",
    }
}

/// Extracts the RPC method path from the URI.
///
/// H-05: only a well-formed `/package.Service/Method` path is recorded
/// verbatim; ANY other value (attacker-controlled HTTP/2 `:path` on the
/// gRPC port) collapses into the single `:unparsed` bucket, so the `rpc`
/// label cardinality is bounded by the service surface, not by input.
fn rpc_of(path: &str) -> String {
    let p = path;
    if p.starts_with('/')
        && p.matches('/').count() == 2
        && p.chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '.' || c == '/' || c == '_')
        && !p.contains("..")
    {
        p.to_string()
    } else {
        ":unparsed".to_string()
    }
}

/// Layer constructor; applied via `Server::builder().layer(...)`.
#[derive(Clone, Default)]
pub struct GrpcMetricsLayer;

impl<S> Layer<S> for GrpcMetricsLayer {
    type Service = GrpcMetricsService<S>;

    fn layer(&self, inner: S) -> Self::Service {
        GrpcMetricsService { inner }
    }
}

/// Wraps a tonic `Routes` service, timing each request.
#[derive(Clone)]
pub struct GrpcMetricsService<S> {
    inner: S,
}

impl<S, B> Service<http::Request<BoxBody>> for GrpcMetricsService<S>
where
    S: Service<http::Request<BoxBody>, Response = http::Response<B>> + Send + 'static,
    S::Future: Send + 'static,
    S::Error: Into<Box<dyn std::error::Error + Send + Sync>> + Send,
    B: Body<Data = bytes::Bytes> + Send + 'static,
{
    type Response = http::Response<B>;
    type Error = S::Error;
    type Future = Pin<Box<dyn Future<Output = Result<Self::Response, Self::Error>> + Send>>;

    fn poll_ready(&mut self, cx: &mut Context<'_>) -> Poll<Result<(), Self::Error>> {
        self.inner.poll_ready(cx)
    }

    fn call(&mut self, req: http::Request<BoxBody>) -> Self::Future {
        let started = Instant::now();
        let rpc = rpc_of(req.uri().path());
        let future = self.inner.call(req);
        Box::pin(async move {
            let response = future.await?;
            let code = response
                .headers()
                .get("grpc-status")
                .and_then(|v| v.to_str().ok())
                .and_then(|v| v.parse::<i32>().ok())
                .unwrap_or(0);
            crate::metrics::record_grpc_request(
                &rpc,
                code_name(code),
                started.elapsed().as_secs_f64(),
            );
            Ok(response)
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn code_names_cover_grpc_table() {
        assert_eq!(code_name(0), "ok");
        assert_eq!(code_name(5), "not_found");
        assert_eq!(code_name(14), "unavailable");
        assert_eq!(code_name(99), "unknown");
    }
}
