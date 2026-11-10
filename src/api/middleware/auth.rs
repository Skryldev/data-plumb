//! Bearer-token authentication for `/api/v1` (except `/api/v1/auth/*`) and
//! the X-Api-Key guard for `/internal/*`.

use axum::body::Body;
use axum::extract::State;
use axum::http::{Request, StatusCode};
use axum::middleware::Next;
use axum::response::{IntoResponse, Response};

use crate::error::StorageError;
use crate::security::auth::AuthContext;
use crate::state::AppState;

/// Rejects requests without a valid `Authorization: Bearer <jwt>` header.
pub async fn auth_middleware(
    State(state): State<AppState>,
    mut req: Request<Body>,
    next: Next,
) -> Result<Response, StorageError> {
    let header = req
        .headers()
        .get(axum::http::header::AUTHORIZATION)
        .and_then(|v| v.to_str().ok())
        .ok_or_else(|| StorageError::Unauthorized("missing Authorization header".into()))?;

    let token = header
        .strip_prefix("Bearer ")
        .or_else(|| header.strip_prefix("bearer "))
        .ok_or_else(|| {
            StorageError::Unauthorized("Authorization header must be 'Bearer <token>'".into())
        })?
        .trim();

    let ctx = state.0.auth.verify_access(token)?;
    req.extensions_mut().insert(ctx);
    Ok(next.run(req).await)
}

/// Constant-time comparison of two byte strings.
fn constant_time_eq(a: &[u8], b: &[u8]) -> bool {
    if a.len() != b.len() {
        return false;
    }
    a.iter()
        .zip(b.iter())
        .fold(0u8, |acc, (x, y)| acc | (x ^ y))
        == 0
}

/// Guards `/internal/*` with the cluster API key.
pub async fn api_key_middleware(
    State(state): State<AppState>,
    req: Request<Body>,
    next: Next,
) -> Result<Response, StorageError> {
    let provided = req
        .headers()
        .get(crate::replication::client::API_KEY_HEADER)
        .and_then(|v| v.to_str().ok())
        .ok_or_else(|| StorageError::Unauthorized("missing X-Api-Key".into()))?;
    let expected = &state.0.cfg.replication.api_key;
    if !constant_time_eq(provided.as_bytes(), expected.as_bytes()) {
        return Err(StorageError::Unauthorized("invalid X-Api-Key".into()));
    }
    Ok(next.run(req).await)
}

/// Role gate used by admin-only handlers.
pub fn require_admin(ctx: &AuthContext) -> Result<(), StorageError> {
    ctx.require_admin()
}

/// Extracts the client IP from `ConnectInfo`, falling back to `-`.
pub fn client_ip(addr: Option<std::net::SocketAddr>) -> String {
    addr.map(|a| a.ip().to_string())
        .unwrap_or_else(|| "-".into())
}

/// Uniform 401 response (kept for places outside the middleware chain).
pub fn unauthorized_response(msg: &str) -> Response {
    StorageError::Unauthorized(msg.to_string()).into_response()
}

/// Health of the auth middleware (diagnostics; always true when reachable).
pub fn auth_configured(state: &AppState) -> bool {
    state.0.auth.user_count() > 0
}

/// Status code helper used in tests.
pub fn unauthorized_status() -> StatusCode {
    StatusCode::UNAUTHORIZED
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ct_eq() {
        assert!(constant_time_eq(b"abc", b"abc"));
        assert!(!constant_time_eq(b"abc", b"xyz"));
        assert!(!constant_time_eq(b"abc", b"abcde"));
    }

    #[test]
    fn ip_fallback() {
        assert_eq!(client_ip(None), "-");
        assert_eq!(
            client_ip(Some("127.0.0.1:80".parse().unwrap())),
            "127.0.0.1"
        );
    }
}
