//! `GET /files` — tenant file listing with tag/name filters and pagination.

use axum::extract::{Query, State};
use axum::response::IntoResponse;
use axum::Extension;
use axum::Json;

use crate::api::dto::{FileListDto, FileSummaryDto, ListQuery};
use crate::core::validation::ensure_can_list_deleted;
use crate::error::StorageError;
use crate::security::auth::AuthContext;
use crate::state::AppState;

const MAX_LIMIT: usize = 1000;

pub async fn list_files(
    State(state): State<AppState>,
    Extension(ctx): Extension<AuthContext>,
    Query(q): Query<ListQuery>,
) -> Result<impl IntoResponse, StorageError> {
    let limit = q.limit.unwrap_or(50).clamp(1, MAX_LIMIT);
    let offset = q.offset.unwrap_or(0);
    let include_deleted = q.include_deleted.unwrap_or(false);

    // M-04a: admin-only on BOTH transports (parity with gRPC ListFiles).
    // Viewers asking for deleted files get an explicit 403 — no silent
    // coercion, no silent honoring.
    if include_deleted {
        ensure_can_list_deleted(ctx.role)?;
    }

    // M-04d: THE shared list/count implementation (same code gRPC calls).
    let (manifests, total) = state
        .0
        .index
        .list_filtered(
            &ctx.tenant_id,
            q.tag.as_deref(),
            q.name.as_deref(),
            include_deleted,
            limit,
            offset,
        )
        .await?;

    let items: Vec<FileSummaryDto> = manifests
        .iter()
        .take(limit)
        .map(FileSummaryDto::from)
        .collect();

    Ok(Json(FileListDto {
        total,
        limit,
        offset,
        items,
    }))
}
