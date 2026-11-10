//! gRPC interceptors and service-level middleware.
//!
//! tonic interceptors run for **unary** RPCs only; streaming RPCs enforce
//! auth/api-key inside their handlers via [`crate::grpc::proto_auth`]. The
//! metrics and recovery layers are tower layers applied to the whole router,
//! so they cover unary *and* streaming calls.
//!
//! Unary composition:
//! * [`auth::AuthInterceptor`] — JWT verify (+ optional admin gate) +
//!   per-tenant rate limiting + request-id propagation.
//! * [`api_key::ApiKeyInterceptor`] — cluster API key for the internal
//!   replication service.
//! * [`InterceptorLayer`] — attaches any [`tonic::service::Interceptor`] to
//!   a generated service server while preserving its size limits (tonic's
//!   own wrapper type is private).
//! * [`metrics::GrpcMetricsLayer`] + [`recovery::layer`] — tower layers on
//!   the whole router (unary + streaming).

pub mod api_key;
pub mod auth;
pub mod metrics;
pub mod rate_limit;
pub mod recovery;

use std::future::Future;
use std::pin::Pin;
use std::task::{Context, Poll};

use tower::Service;
use tower_layer::Layer;

/// Layer that wraps a service so every request passes through an
/// [`tonic::service::Interceptor`] before reaching it.
#[derive(Clone)]
pub struct InterceptorLayer<F> {
    f: F,
}

impl<F: Clone> InterceptorLayer<F> {
    pub fn new(f: F) -> Self {
        Self { f }
    }
}

impl<S, F> Layer<S> for InterceptorLayer<F>
where
    F: tonic::service::Interceptor + Clone,
{
    type Service = Intercepted<S, F>;

    fn layer(&self, inner: S) -> Self::Service {
        Intercepted {
            inner,
            f: self.f.clone(),
        }
    }
}

/// A service wrapped in an interceptor middleware (local re-implementation
/// of tonic's private `InterceptedService`, preserving message-size limits
/// set on the inner server).
#[derive(Clone)]
pub struct Intercepted<S, F> {
    inner: S,
    f: F,
}

impl<S, F> Service<http::Request<tonic::body::BoxBody>> for Intercepted<S, F>
where
    F: tonic::service::Interceptor + Clone + Send + 'static,
    S: Service<
            http::Request<tonic::body::BoxBody>,
            Response = http::Response<tonic::body::BoxBody>,
        > + Send
        + 'static,
    S::Future: Send + 'static,
{
    type Response = http::Response<tonic::body::BoxBody>;
    type Error = S::Error;
    type Future = Pin<Box<dyn Future<Output = Result<Self::Response, Self::Error>> + Send>>;

    fn poll_ready(&mut self, cx: &mut Context<'_>) -> Poll<Result<(), Self::Error>> {
        self.inner.poll_ready(cx)
    }

    fn call(&mut self, req: http::Request<tonic::body::BoxBody>) -> Self::Future {
        // Detach the body so the interceptor only sees metadata/extensions.
        let (parts, body) = req.into_parts();
        let uri = parts.uri.clone();
        let method = parts.method.clone();
        let version = parts.version;
        let metadata = tonic::metadata::MetadataMap::from_headers(parts.headers);

        let mut interceptor = self.f.clone();
        let guarded = interceptor.call(tonic::Request::from_parts(metadata, parts.extensions, ()));
        match guarded {
            Ok(tonic_req) => {
                let (metadata, extensions, ()) = tonic_req.into_parts();
                let headers = metadata.into_headers();
                let mut rebuilt = http::Request::builder()
                    .method(method)
                    .uri(uri)
                    .version(version);
                for (k, v) in &headers {
                    rebuilt = rebuilt.header(k.clone(), v.clone());
                }
                let mut rebuilt = match rebuilt.body(body) {
                    Ok(r) => r,
                    // Unreachable for requests that already parsed as
                    // http::Request; fail closed with a gRPC error.
                    Err(_) => {
                        return Box::pin(async move {
                            Ok(status_only_response(&tonic::Status::internal(
                                "request rebuild failed",
                            )))
                        });
                    }
                };
                *rebuilt.extensions_mut() = extensions;

                let future = self.inner.call(rebuilt);
                Box::pin(async move {
                    let response = future.await?;
                    Ok(response.map(tonic::body::boxed))
                })
            }
            Err(status) => {
                // Trailers-only style rejection with the gRPC status.
                Box::pin(async move { Ok(status_only_response(&status)) })
            }
        }
    }
}

/// Builds a headers-only response carrying `grpc-status` / `grpc-message`,
/// the wire format tonic clients accept for failed unary calls.
fn status_only_response(status: &tonic::Status) -> http::Response<tonic::body::BoxBody> {
    let message: String = status.message().chars().filter(|c| c.is_ascii()).collect();
    let code = (status.code() as i32).to_string();
    let headers = status.metadata().clone().into_headers();
    let mut response = http::Response::builder()
        .status(200)
        .header("content-type", "application/grpc")
        .header("grpc-status", &code)
        .header("grpc-message", &message);
    for (k, v) in &headers {
        response = response.header(k.as_str(), v.clone());
    }
    response
        .body(tonic::body::empty_body())
        .unwrap_or_else(|_| http::Response::new(tonic::body::empty_body()))
}

// `NamedService` forwarding keeps `add_service` happy.
impl<S, F> tonic::server::NamedService for Intercepted<S, F>
where
    S: tonic::server::NamedService,
{
    const NAME: &'static str = S::NAME;
}
