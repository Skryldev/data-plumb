//! `/health` and `/ready` probes.

use axum::extract::State;
use axum::http::StatusCode;
use axum::response::IntoResponse;
use axum::Json;

use crate::api::dto::{HealthDto, ReadyDto};
use crate::state::AppState;

pub async fn health(State(state): State<AppState>) -> impl IntoResponse {
    Json(HealthDto {
        status: "ok",
        version: env!("CARGO_PKG_VERSION"),
        uptime_secs: state.uptime_secs(),
    })
}

pub async fn ready(State(state): State<AppState>) -> impl IntoResponse {
    let database = state.0.index.ping().await.is_ok();
    let storage = state.0.blocks.verify_writable().await.is_ok();
    let ok = database && storage;
    let status = if ok { "ready" } else { "degraded" };
    let code = if ok {
        StatusCode::OK
    } else {
        StatusCode::SERVICE_UNAVAILABLE
    };
    (
        code,
        Json(ReadyDto {
            status,
            database,
            storage,
        }),
    )
}
