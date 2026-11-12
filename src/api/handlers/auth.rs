//! `/api/v1/auth/*`: login, refresh, logout.

use axum::extract::State;
use axum::http::StatusCode;
use axum::response::IntoResponse;
use axum::Json;

use crate::api::dto::{LoginRequest, RefreshRequest, TokenPairDto};
use crate::error::StorageError;
use crate::security::audit::{Actor, AuditAction};
use crate::state::AppState;

fn to_dto(p: crate::security::auth::TokenPair) -> TokenPairDto {
    TokenPairDto {
        access_token: p.access_token,
        refresh_token: p.refresh_token,
        token_type: p.token_type,
        expires_in: p.expires_in,
    }
}

pub async fn login(
    State(state): State<AppState>,
    Json(req): Json<LoginRequest>,
) -> Result<impl IntoResponse, StorageError> {
    match state.0.auth.login(&req.username, &req.password) {
        Ok(pair) => {
            state
                .0
                .audit
                .record(
                    AuditAction::LoginSuccess,
                    Actor::User(req.username.clone()),
                    None,
                    &req.username,
                    "",
                    "ok",
                    "",
                )
                .await;
            Ok((StatusCode::OK, Json(to_dto(pair))))
        }
        Err(e @ StorageError::Throttled(_)) => {
            // M-05: the lockout event is its own audit action (distinct
            // from a plain credential failure) with the retry window.
            state
                .0
                .audit
                .record(
                    AuditAction::LoginThrottled,
                    Actor::User(req.username.clone()),
                    None,
                    &req.username,
                    "",
                    "throttled",
                    &e.to_string(),
                )
                .await;
            Err(e)
        }
        Err(e) => {
            state
                .0
                .audit
                .record(
                    AuditAction::LoginFailure,
                    Actor::User(req.username.clone()),
                    None,
                    &req.username,
                    "",
                    "rejected",
                    "",
                )
                .await;
            Err(e)
        }
    }
}

pub async fn refresh(
    State(state): State<AppState>,
    Json(req): Json<RefreshRequest>,
) -> Result<impl IntoResponse, StorageError> {
    let pair = state.0.auth.refresh(&req.refresh_token)?;
    Ok((StatusCode::OK, Json(to_dto(pair))))
}

/// Logout: invalidates the presented refresh token. Returns 204 whether or
/// not the token existed (idempotent).
pub async fn logout(State(state): State<AppState>, Json(req): Json<RefreshRequest>) -> StatusCode {
    let _ = state.0.auth.logout(&req.refresh_token);
    StatusCode::NO_CONTENT
}
