//! `DELETE /files/:id` — soft delete (default) or hard purge (?purge=true).

use axum::extract::{Path, Query, State};
use axum::http::StatusCode;
use axum::response::IntoResponse;
use axum::Extension;
use uuid::Uuid;

use crate::api::middleware::tenant::check_file_access;
use crate::error::StorageError;
use crate::security::audit::{Actor, AuditAction};
use crate::security::auth::AuthContext;
use crate::state::AppState;
use crate::storage::manifest::Manifest;

#[derive(Debug, serde::Deserialize, Default)]
pub struct DeleteQuery {
    pub purge: Option<bool>,
}

pub async fn delete_file(
    State(state): State<AppState>,
    Extension(ctx): Extension<AuthContext>,
    Path(file_id): Path<Uuid>,
    Query(q): Query<DeleteQuery>,
) -> Result<impl IntoResponse, StorageError> {
    let purge = q.purge.unwrap_or(false);
    // Raw lookup: purging an already-soft-deleted file must still work.
    let manifest = state
        .0
        .index
        .get_manifest(&file_id)
        .await?
        .ok_or_else(|| StorageError::NotFound(format!("file {file_id}")))?;
    check_file_access(&ctx, &manifest.tenant_id)?;
    // Write permission required to delete anything.
    ctx.require_write()?;
    if purge {
        ctx.require_admin()?;
    }

    if purge {
        hard_purge(&state, &manifest).await?;
        state
            .0
            .audit
            .record(
                AuditAction::Purge,
                Actor::User(ctx.username.clone()),
                Some(manifest.tenant_id),
                &manifest.name,
                "",
                "ok",
                &format!("file_id={file_id}"),
            )
            .await;
    } else {
        if state
            .0
            .index
            .soft_delete_manifest(&file_id)
            .await?
            .is_none()
        {
            // race: already deleted by someone else
            return Err(StorageError::NotFound(format!("file {file_id}")));
        }
        state.0.cache.manifests.invalidate(&file_id).await;
        state.0.replicator.enqueue_manifest(file_id).await?;
        state
            .0
            .audit
            .record(
                AuditAction::Delete,
                Actor::User(ctx.username.clone()),
                Some(manifest.tenant_id),
                &manifest.name,
                "",
                "ok",
                &format!("file_id={file_id} soft"),
            )
            .await;
    }
    Ok(StatusCode::NO_CONTENT)
}

/// Removes the manifest and drops its chunks' refcounts, deleting blobs
/// that reach refcount zero.
pub async fn hard_purge(state: &AppState, manifest: &Manifest) -> Result<(), StorageError> {
    let hashes: Vec<[u8; 32]> = manifest.chunks.iter().map(|c| c.hash).collect();
    state.0.index.remove_manifest(&manifest.file_id).await?;
    state.0.cache.manifests.invalidate(&manifest.file_id).await;
    // C-03: route through the GC sweep helper so the safe ordering
    // (re-check -> unlink -> finalize) applies to request-path purges too.
    state.0.gc.purge_chunks_now(hashes).await?;
    state.0.replicator.enqueue_purge(manifest.file_id).await?;
    Ok(())
}
