//! X-Api-Key interceptor guarding the internal replication service.
//!
//! The replication channel runs between trusted peers, not end users, so it
//! authenticates with the cluster API key (same constant-time comparison as
//! the REST `/internal/*` guard) instead of a JWT.

use tonic::{Request, Status};

use crate::grpc::proto_auth::require_api_key;
use crate::state::AppState;

/// Enforces a valid `x-api-key` metadata entry.
#[derive(Clone)]
pub struct ApiKeyInterceptor {
    state: AppState,
}

impl ApiKeyInterceptor {
    pub fn new(state: AppState) -> Self {
        Self { state }
    }
}

impl tonic::service::Interceptor for ApiKeyInterceptor {
    fn call(&mut self, req: Request<()>) -> Result<Request<()>, Status> {
        require_api_key(&self.state, &req)?;
        Ok(req)
    }
}
