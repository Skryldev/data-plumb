//! Bearer-JWT interceptor for unary RPCs.
//!
//! Because tonic interceptors run before the handler and cannot be async,
//! this composite guard performs, in order:
//!
//! 1. JWT verification (`authorization: Bearer <jwt>`);
//! 2. optional admin gate;
//! 3. per-tenant rate limiting;
//! 4. request-id propagation (reuse or mint `x-request-id`).
//!
//! Finally it inserts the [`AuthContext`] into request extensions.
//!
//! Streaming RPCs bypass interceptors entirely — they call
//! [`crate::grpc::proto_auth`] helpers inside their handlers instead.

use tonic::{Request, Status};

use crate::grpc::interceptors::rate_limit::RateLimiter;
use crate::grpc::proto_auth::{bearer_context, request_id_of, status_from_error};
use crate::security::auth::AuthContext;
use crate::state::AppState;

/// Composite unary guard for FileService / AdminService.
#[derive(Clone)]
pub struct AuthInterceptor {
    state: AppState,
    limiter: Option<RateLimiter>,
    admin_required: bool,
}

impl AuthInterceptor {
    /// Auth + rate limit + request-id (FileService).
    pub fn file_service(state: AppState) -> Self {
        let cfg = &state.0.cfg.rate_limit;
        Self {
            limiter: Some(RateLimiter::from_cfg(
                cfg.enabled,
                cfg.per_second,
                cfg.burst,
            )),
            state,
            admin_required: false,
        }
    }

    /// Admin-gated variant (AdminService).
    pub fn admin_service(state: AppState) -> Self {
        let cfg = &state.0.cfg.rate_limit;
        Self {
            limiter: Some(RateLimiter::from_cfg(
                cfg.enabled,
                cfg.per_second,
                cfg.burst,
            )),
            state,
            admin_required: true,
        }
    }
}

impl tonic::service::Interceptor for AuthInterceptor {
    fn call(&mut self, mut req: Request<()>) -> Result<Request<()>, Status> {
        let ctx: AuthContext = bearer_context(&self.state, &req)?;
        if self.admin_required {
            ctx.require_admin().map_err(|e| status_from_error(&e))?;
        }
        if let Some(limiter) = &self.limiter {
            if !limiter.allow(ctx.tenant_id) {
                return Err(Status::resource_exhausted("rate limited"));
            }
        }
        // Propagate (or mint) the request id before the handler runs.
        let id = request_id_of(&req);
        if let Ok(v) = tonic::metadata::AsciiMetadataValue::try_from(id.as_str()) {
            req.metadata_mut()
                .insert(crate::grpc::proto_auth::REQUEST_ID_KEY, v);
        }
        req.extensions_mut().insert(ctx);
        Ok(req)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn interceptor_types_are_cloneable() {
        fn assert_clone<T: Clone>() {}
        assert_clone::<AuthInterceptor>();
    }
}
