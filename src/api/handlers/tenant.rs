//! `/api/v1/tenants`: admin-managed tenant registration.

use axum::extract::State;
use axum::http::StatusCode;
use axum::response::IntoResponse;
use axum::Extension;
use axum::Json;

use crate::api::dto::{CreateTenantRequest, TenantDto};
use crate::api::middleware::auth::require_admin;
use crate::error::StorageError;
use crate::security::audit::{Actor, AuditAction};
use crate::security::auth::AuthContext;
use crate::state::AppState;

fn to_dto(t: crate::security::tenant::Tenant) -> TenantDto {
    TenantDto {
        id: t.id,
        name: t.name,
        created_at_ms: t.created_at_ms,
        file_count: t.file_count,
        bytes_used: t.bytes_used,
    }
}

pub async fn create_tenant(
    State(state): State<AppState>,
    Extension(ctx): Extension<AuthContext>,
    Json(req): Json<CreateTenantRequest>,
) -> Result<impl IntoResponse, StorageError> {
    require_admin(&ctx)?;
    let tenant = state.0.tenants.create(&req.name).await?;
    state
        .0
        .audit
        .record(
            AuditAction::TenantCreate,
            Actor::User(ctx.username.clone()),
            Some(tenant.id),
            &tenant.name,
            "",
            "ok",
            "",
        )
        .await;
    Ok((StatusCode::CREATED, Json(to_dto(tenant))))
}

pub async fn list_tenants(
    State(state): State<AppState>,
    Extension(ctx): Extension<AuthContext>,
) -> Result<impl IntoResponse, StorageError> {
    require_admin(&ctx)?;
    let list = state.0.tenants.list().await?;
    Ok(Json(
        list.iter().map(|t| to_dto(t.clone())).collect::<Vec<_>>(),
    ))
}
