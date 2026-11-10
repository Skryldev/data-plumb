//! `GET /files/:id/meta` — manifest metadata (JSON).

use axum::extract::{Path, State};
use axum::response::IntoResponse;
use axum::Extension;
use axum::Json;
use uuid::Uuid;

use crate::api::dto::FileMetaDto;
use crate::api::middleware::tenant::check_file_access;
use crate::error::StorageError;
use crate::security::auth::AuthContext;
use crate::state::AppState;

#[derive(Debug, serde::Deserialize, Default)]
pub struct MetaQuery {
    pub chunks: Option<bool>,
}

pub async fn file_meta(
    State(state): State<AppState>,
    Extension(ctx): Extension<AuthContext>,
    Path(file_id): Path<Uuid>,
    axum::extract::Query(q): axum::extract::Query<MetaQuery>,
) -> Result<impl IntoResponse, StorageError> {
    let manifest = state.0.retrieval.manifest(&file_id).await?;
    check_file_access(&ctx, &manifest.tenant_id)?;
    let include_chunks = q.chunks.unwrap_or(true);
    Ok(Json(FileMetaDto::from_manifest(&manifest, include_chunks)))
}
