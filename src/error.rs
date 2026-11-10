//! Unified error type with HTTP status mapping.

use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use serde_json::json;

/// Errors produced by every layer of the service.
#[derive(Debug, thiserror::Error)]
pub enum StorageError {
    #[error("not found: {0}")]
    NotFound(String),

    #[error("validation failed: {0}")]
    Validation(String),

    #[error("unauthorized: {0}")]
    Unauthorized(String),

    #[error("forbidden: {0}")]
    Forbidden(String),

    #[error("conflict: {0}")]
    Conflict(String),

    #[error("payload too large: {0}")]
    TooLarge(String),

    #[error("range not satisfiable (size={0})")]
    RangeNotSatisfiable(u64),

    #[error("hash mismatch: expected {expected}, got {actual}")]
    HashMismatch { expected: String, actual: String },

    #[error("corruption: {0}")]
    Corruption(String),

    #[error("compression error: {0}")]
    Compression(String),

    #[error("encryption error: {0}")]
    Encryption(String),

    #[error("authentication backend error: {0}")]
    AuthBackend(String),

    #[error("io error: {0}")]
    Io(#[from] std::io::Error),

    #[error("database error: {0}")]
    Database(String),

    #[error("serialization error: {0}")]
    Serialization(String),

    #[error("replication error: {0}")]
    Replication(String),

    #[error("service unavailable: {0}")]
    Unavailable(String),

    #[error("internal error: {0}")]
    Internal(String),
}

impl StorageError {
    /// HTTP status code this error maps to.
    pub fn status_code(&self) -> StatusCode {
        match self {
            StorageError::NotFound(_) => StatusCode::NOT_FOUND,
            StorageError::Validation(_) => StatusCode::BAD_REQUEST,
            StorageError::Unauthorized(_) => StatusCode::UNAUTHORIZED,
            StorageError::Forbidden(_) => StatusCode::FORBIDDEN,
            StorageError::Conflict(_) => StatusCode::CONFLICT,
            StorageError::TooLarge(_) => StatusCode::PAYLOAD_TOO_LARGE,
            StorageError::RangeNotSatisfiable(_) => StatusCode::RANGE_NOT_SATISFIABLE,
            StorageError::HashMismatch { .. } => StatusCode::INTERNAL_SERVER_ERROR,
            StorageError::Corruption(_) => StatusCode::INTERNAL_SERVER_ERROR,
            StorageError::Compression(_) => StatusCode::INTERNAL_SERVER_ERROR,
            StorageError::Encryption(_) => StatusCode::INTERNAL_SERVER_ERROR,
            StorageError::AuthBackend(_) => StatusCode::INTERNAL_SERVER_ERROR,
            StorageError::Io(_) => StatusCode::INTERNAL_SERVER_ERROR,
            StorageError::Database(_) => StatusCode::INTERNAL_SERVER_ERROR,
            StorageError::Serialization(_) => StatusCode::INTERNAL_SERVER_ERROR,
            StorageError::Replication(_) => StatusCode::BAD_GATEWAY,
            StorageError::Unavailable(_) => StatusCode::SERVICE_UNAVAILABLE,
            StorageError::Internal(_) => StatusCode::INTERNAL_SERVER_ERROR,
        }
    }

    /// Stable machine-readable error code for API clients.
    pub fn code(&self) -> &'static str {
        match self {
            StorageError::NotFound(_) => "not_found",
            StorageError::Validation(_) => "validation_failed",
            StorageError::Unauthorized(_) => "unauthorized",
            StorageError::Forbidden(_) => "forbidden",
            StorageError::Conflict(_) => "conflict",
            StorageError::TooLarge(_) => "payload_too_large",
            StorageError::RangeNotSatisfiable(_) => "range_not_satisfiable",
            StorageError::HashMismatch { .. } => "hash_mismatch",
            StorageError::Corruption(_) => "corruption",
            StorageError::Compression(_) => "compression_error",
            StorageError::Encryption(_) => "encryption_error",
            StorageError::AuthBackend(_) => "auth_backend_error",
            StorageError::Io(_) => "io_error",
            StorageError::Database(_) => "database_error",
            StorageError::Serialization(_) => "serialization_error",
            StorageError::Replication(_) => "replication_error",
            StorageError::Unavailable(_) => "service_unavailable",
            StorageError::Internal(_) => "internal_error",
        }
    }

    /// Maps this error onto the semantically matching gRPC status code.
    /// The authoritative table lives in
    /// `grpc::proto_auth::status_from_error`; this wrapper keeps call sites
    /// readable.
    pub fn to_grpc_status(&self) -> tonic::Status {
        crate::grpc::proto_auth::status_from_error(self)
    }
}

/// Helper that lifts any `Display`-able error into [`StorageError::Database`].
pub fn dbe<E: std::fmt::Display>(e: E) -> StorageError {
    StorageError::Database(e.to_string())
}

/// Helper that lifts any `Display`-able error into [`StorageError::Serialization`].
pub fn ser<E: std::fmt::Display>(e: E) -> StorageError {
    StorageError::Serialization(e.to_string())
}

/// Helper that lifts any `Display`-able error into [`StorageError::Internal`].
pub fn internal<E: std::fmt::Display>(e: E) -> StorageError {
    StorageError::Internal(e.to_string())
}

impl From<redb::Error> for StorageError {
    fn from(e: redb::Error) -> Self {
        dbe(e)
    }
}

impl From<bincode::Error> for StorageError {
    fn from(e: bincode::Error) -> Self {
        ser(e)
    }
}

impl From<axum::extract::multipart::MultipartError> for StorageError {
    fn from(e: axum::extract::multipart::MultipartError) -> Self {
        StorageError::Validation(format!("multipart: {e}"))
    }
}

/// JSON body returned for every failed request.
#[derive(Debug, serde::Serialize)]
pub struct ErrorBody {
    pub error: ErrorDetail,
}

#[derive(Debug, serde::Serialize)]
pub struct ErrorDetail {
    pub code: &'static str,
    pub message: String,
}

impl IntoResponse for StorageError {
    fn into_response(self) -> Response {
        let status = self.status_code();
        if status.is_server_error() {
            tracing::error!(error = %self, code = self.code(), "request failed");
        } else {
            tracing::warn!(error = %self, code = self.code(), "request rejected");
        }
        let body = ErrorBody {
            error: ErrorDetail {
                code: self.code(),
                message: self.to_string(),
            },
        };
        (status, axum::Json(body)).into_response()
    }
}

/// Convenience constructor used across the code base.
pub fn not_found(what: impl std::fmt::Display) -> StorageError {
    StorageError::NotFound(what.to_string())
}

/// Builds a JSON error body for non-`StorageError` exits (kept for completeness).
pub fn error_json(code: &'static str, message: impl Into<String>) -> serde_json::Value {
    json!({ "error": { "code": code, "message": message.into() } })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn status_mapping() {
        assert_eq!(
            StorageError::NotFound("x".into()).status_code(),
            StatusCode::NOT_FOUND
        );
        assert_eq!(
            StorageError::Validation("x".into()).status_code(),
            StatusCode::BAD_REQUEST
        );
        assert_eq!(
            StorageError::Unauthorized("x".into()).status_code(),
            StatusCode::UNAUTHORIZED
        );
        assert_eq!(
            StorageError::Forbidden("x".into()).status_code(),
            StatusCode::FORBIDDEN
        );
        assert_eq!(
            StorageError::TooLarge("x".into()).status_code(),
            StatusCode::PAYLOAD_TOO_LARGE
        );
        assert_eq!(
            StorageError::RangeNotSatisfiable(10).status_code(),
            StatusCode::RANGE_NOT_SATISFIABLE
        );
        assert_eq!(
            StorageError::HashMismatch {
                expected: "a".into(),
                actual: "b".into()
            }
            .status_code(),
            StatusCode::INTERNAL_SERVER_ERROR
        );
        assert_eq!(
            StorageError::Unavailable("x".into()).status_code(),
            StatusCode::SERVICE_UNAVAILABLE
        );
        assert_eq!(
            StorageError::Replication("x".into()).status_code(),
            StatusCode::BAD_GATEWAY
        );
    }

    #[test]
    fn error_body_shape() {
        let resp = StorageError::NotFound("file x".into()).into_response();
        assert_eq!(resp.status(), StatusCode::NOT_FOUND);
    }

    #[test]
    fn codes_are_snake_case() {
        for c in [
            StorageError::Conflict("c".into()),
            StorageError::Corruption("c".into()),
            StorageError::Encryption("c".into()),
        ] {
            assert!(c
                .code()
                .chars()
                .all(|ch| ch == '_' || ch.is_ascii_lowercase()));
        }
    }
}
