//! `GET /files` — tenant file listing with tag/name filters and pagination.

use axum::extract::{Query, State};
use axum::response::IntoResponse;
use axum::Extension;
use axum::Json;

use crate::api::dto::{FileListDto, FileSummaryDto, ListQuery};
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

    // Tag filter takes precedence; name filter is an exact match.
    let (manifests, total) =
        if let Some(tag) = q.tag.as_deref().map(str::trim).filter(|t| !t.is_empty()) {
            let ids = state.0.index.list_ids_by_tag(&ctx.tenant_id, tag).await?;
            let mut out = Vec::new();
            for id in ids {
                if let Some(m) = state.0.index.get_manifest(&id).await? {
                    if include_deleted || !m.is_deleted() {
                        out.push(m);
                    }
                }
            }
            let total = out.len() as u64;
            (out, total)
        } else if let Some(name) = q.name.as_deref().map(str::trim).filter(|n| !n.is_empty()) {
            let out: Vec<_> = match state.0.index.get_by_name(&ctx.tenant_id, name).await? {
                Some(m) if include_deleted || !m.is_deleted() => vec![m],
                _ => Vec::new(),
            };
            let total = out.len() as u64;
            (out, total)
        } else {
            let list = state
                .0
                .index
                .list_by_tenant(&ctx.tenant_id, include_deleted, limit, offset)
                .await?;
            let total = state
                .0
                .index
                .count_by_tenant(&ctx.tenant_id, include_deleted)
                .await?;
            (list, total)
        };

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
