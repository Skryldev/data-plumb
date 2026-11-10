//! Metadata-based authentication helpers for gRPC handlers.
//!
//! tonic interceptors run for **unary** RPCs only, so streaming handlers
//! authenticate themselves with the helpers here; unary services get the
//! same `AuthContext` via interceptors that insert it into request
//! extensions. Both paths converge on [`bearer_context`] /
//! [`require_api_key`].

use tonic::metadata::AsciiMetadataValue;
use tonic::{Request, Status};

use crate::config::Role;
use crate::error::StorageError;
use crate::replication::client::API_KEY_HEADER;
use crate::security::auth::AuthContext;
use crate::state::AppState;

/// Metadata key carrying the JWT (`authorization: Bearer <jwt>`).
pub const AUTHORIZATION_KEY: &str = "authorization";

/// Metadata key carrying an explicit request id (propagated when present).
pub const REQUEST_ID_KEY: &str = "x-request-id";

/// Extracts `Bearer <jwt>` from the `authorization` metadata of any request.
pub fn bearer_token<T>(req: &Request<T>) -> Result<String, Status> {
    let raw = req
        .metadata()
        .get(AUTHORIZATION_KEY)
        .and_then(|v| v.to_str().ok())
        .ok_or_else(|| Status::unauthenticated("missing authorization metadata"))?;
    let token = raw
        .strip_prefix("Bearer ")
        .or_else(|| raw.strip_prefix("bearer "))
        .ok_or_else(|| Status::unauthenticated("authorization metadata must be 'Bearer <token>'"))?
        .trim();
    if token.is_empty() {
        return Err(Status::unauthenticated("empty bearer token"));
    }
    Ok(token.to_string())
}

/// Verifies the bearer token of a request and returns its auth context.
/// Streaming handlers call this directly; unary handlers usually receive
/// the context from the interceptor instead.
pub fn bearer_context<T>(state: &AppState, req: &Request<T>) -> Result<AuthContext, Status> {
    let token = bearer_token(req)?;
    state
        .0
        .auth
        .verify_access(&token)
        .map_err(|e| status_from_error(&e))
}

/// Reads the context injected by the auth interceptor; falls back to
/// verifying the metadata for streaming RPCs where no interceptor ran.
pub fn context_of<T>(state: &AppState, req: &Request<T>) -> Result<AuthContext, Status> {
    if let Some(ctx) = req.extensions().get::<AuthContext>() {
        return Ok(ctx.clone());
    }
    bearer_context(state, req)
}

/// Enforces the cluster API key (`x-api-key` metadata) for the internal
/// replication service. Constant-time comparison, mirroring the REST guard.
pub fn require_api_key<T>(state: &AppState, req: &Request<T>) -> Result<(), Status> {
    let provided = req
        .metadata()
        .get(API_KEY_HEADER)
        .and_then(|v| v.to_str().ok())
        .ok_or_else(|| Status::unauthenticated("missing x-api-key metadata"))?;
    let expected = state.0.cfg.replication.api_key.as_bytes();
    if !constant_time_eq(provided.as_bytes(), expected) {
        return Err(Status::unauthenticated("invalid x-api-key"));
    }
    Ok(())
}

/// Auth context from extensions (admin gate for unary RPCs).
pub fn require_admin_ctx<T>(req: &Request<T>) -> Result<AuthContext, Status> {
    let ctx = req
        .extensions()
        .get::<AuthContext>()
        .cloned()
        .ok_or_else(|| Status::unauthenticated("missing auth context"))?;
    ctx.require_admin().map_err(|e| status_from_error(&e))?;
    Ok(ctx)
}

/// Request id carried by the caller, or a freshly generated one.
pub fn request_id_of<T>(req: &Request<T>) -> String {
    req.metadata()
        .get(REQUEST_ID_KEY)
        .and_then(|v| v.to_str().ok())
        .filter(|s| !s.is_empty())
        .map(str::to_string)
        .unwrap_or_else(|| uuid::Uuid::now_v7().to_string())
}

/// Maps a [`StorageError`] onto the tonic status of the same meaning —
/// the single mapping used by every gRPC handler.
pub fn status_from_error(e: &StorageError) -> Status {
    use tonic::Code;
    let code = match e {
        StorageError::NotFound(_) => Code::NotFound,
        StorageError::Validation(_) => Code::InvalidArgument,
        StorageError::Unauthorized(_) => Code::Unauthenticated,
        StorageError::Forbidden(_) => Code::PermissionDenied,
        StorageError::Conflict(_) => Code::AlreadyExists,
        StorageError::TooLarge(_) => Code::ResourceExhausted,
        StorageError::RangeNotSatisfiable(_) => Code::OutOfRange,
        StorageError::HashMismatch { .. } => Code::DataLoss,
        StorageError::Corruption(_) => Code::DataLoss,
        StorageError::Compression(_) => Code::Internal,
        StorageError::Encryption(_) => Code::Internal,
        StorageError::AuthBackend(_) => Code::Internal,
        StorageError::Io(_) => Code::Internal,
        StorageError::Database(_) => Code::Internal,
        StorageError::Serialization(_) => Code::Internal,
        StorageError::Replication(_) => Code::Unavailable,
        StorageError::Unavailable(_) => Code::Unavailable,
        StorageError::Internal(_) => Code::Internal,
    };
    Status::new(code, e.to_string())
}

/// Convenience: converts `Result<T, StorageError>` into tonic responses.
pub fn ok_or_status<T, E: Into<StorageError>>(r: Result<T, E>) -> Result<T, Status> {
    r.map_err(|e| status_from_error(&e.into()))
}

/// Builds an `authorization` metadata value for clients.
pub fn bearer_metadata(token: &str) -> Option<AsciiMetadataValue> {
    let mut v = AsciiMetadataValue::try_from(format!("Bearer {token}").as_str()).ok()?;
    // keep the token private in logs
    v.set_sensitive(true);
    Some(v)
}

impl From<StorageError> for Status {
    fn from(e: StorageError) -> Self {
        status_from_error(&e)
    }
}

fn constant_time_eq(a: &[u8], b: &[u8]) -> bool {
    if a.len() != b.len() {
        return false;
    }
    a.iter()
        .zip(b.iter())
        .fold(0u8, |acc, (x, y)| acc | (x ^ y))
        == 0
}

/// Parses a role string the way tokens carry it.
pub fn role_of(ctx: &AuthContext) -> Role {
    ctx.role
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ct_eq_matches_rest_guard() {
        assert!(constant_time_eq(b"key", b"key"));
        assert!(!constant_time_eq(b"key", b"keY"));
        assert!(!constant_time_eq(b"key", b"keyx"));
    }

    #[test]
    fn status_mapping_covers_every_variant() {
        let samples = [
            StorageError::NotFound("x".into()),
            StorageError::Validation("x".into()),
            StorageError::Unauthorized("x".into()),
            StorageError::Forbidden("x".into()),
            StorageError::Conflict("x".into()),
            StorageError::TooLarge("x".into()),
            StorageError::RangeNotSatisfiable(1),
            StorageError::HashMismatch {
                expected: "a".into(),
                actual: "b".into(),
            },
            StorageError::Corruption("x".into()),
            StorageError::Compression("x".into()),
            StorageError::Encryption("x".into()),
            StorageError::AuthBackend("x".into()),
            StorageError::Io(std::io::Error::other("x")),
            StorageError::Database("x".into()),
            StorageError::Serialization("x".into()),
            StorageError::Replication("x".into()),
            StorageError::Unavailable("x".into()),
            StorageError::Internal("x".into()),
        ];
        for e in samples {
            let s = status_from_error(&e);
            assert_ne!(s.code(), tonic::Code::Unknown, "unmapped variant {e:?}");
        }
    }
}
