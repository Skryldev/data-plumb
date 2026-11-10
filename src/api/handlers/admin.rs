//! Admin operations: GC trigger, file resync, audit listing.

use axum::extract::{Path, Query, State};
use axum::http::StatusCode;
use axum::response::IntoResponse;
use axum::Extension;
use axum::Json;
use uuid::Uuid;

use crate::api::dto::{AuditEntryDto, GcReportDto, ResyncDto};
use crate::api::middleware::auth::require_admin;
use crate::error::StorageError;
use crate::replication::client::{ChunkPayload, ReplicationClient};
use crate::security::auth::AuthContext;
use crate::security::tenant::check_access;
use crate::state::AppState;

pub async fn trigger_gc(
    State(state): State<AppState>,
    Extension(ctx): Extension<AuthContext>,
) -> Result<impl IntoResponse, StorageError> {
    require_admin(&ctx)?;
    let report = state.0.gc.run_once().await?;
    Ok(Json(GcReportDto {
        manifests_purged: report.manifests_purged,
        chunks_removed: report.chunks_removed,
        orphans_removed: report.orphans_removed,
        bytes_reclaimed: report.bytes_reclaimed,
    }))
}

#[derive(Debug, serde::Deserialize, Default)]
pub struct AuditQuery {
    pub limit: Option<usize>,
}

pub async fn list_audit(
    State(state): State<AppState>,
    Extension(ctx): Extension<AuthContext>,
    Query(q): Query<AuditQuery>,
) -> Result<impl IntoResponse, StorageError> {
    require_admin(&ctx)?;
    let limit = q.limit.unwrap_or(100).clamp(1, 1000);
    let events = state.0.audit.recent(limit).await?;
    Ok(Json(
        events
            .iter()
            .map(|e| AuditEntryDto {
                id: e.id,
                ts_ms: e.ts_ms,
                actor: e.actor.clone(),
                tenant_id: e.tenant_id,
                action: e.action.clone(),
                object: e.object.clone(),
                ip: e.ip.clone(),
                status: e.status.clone(),
                detail: e.detail.clone(),
            })
            .collect::<Vec<_>>(),
    ))
}

/// Pushes a file (manifest + all chunk blobs) to the replication target.
pub async fn resync_file(
    State(state): State<AppState>,
    Extension(ctx): Extension<AuthContext>,
    Path(file_id): Path<Uuid>,
) -> Result<impl IntoResponse, StorageError> {
    require_admin(&ctx)?;
    let manifest = state.0.retrieval.manifest(&file_id).await?;
    check_access(&ctx.tenant_id, &manifest.tenant_id, true)?;

    let mut payloads: Vec<ChunkPayload> = Vec::with_capacity(manifest.chunks.len());
    for cr in &manifest.chunks {
        let blob = state.0.blocks.read(&cr.hash).await?;
        payloads.push(ChunkPayload {
            hash: cr.hash,
            plain_hash: cr.plain_hash,
            size_original: cr.size_original,
            size_stored: cr.size_stored,
            compressed: cr.compressed,
            encrypted: cr.encrypted,
            blob,
        });
    }
    let bundle = crate::replication::client::encode_file_bundle(&manifest, &payloads)?;
    state.0.replicator.push_bundle(&bundle, &file_id).await?;
    Ok((
        StatusCode::OK,
        Json(ResyncDto {
            file_id,
            replicated: true,
        }),
    ))
}

/// Helper shared with the replica apply path.
pub async fn ship_bundle_to(
    base_url: &str,
    api_key: &str,
    bundle: &[u8],
    file_id: &Uuid,
) -> Result<(), StorageError> {
    let client = ReplicationClient::new(base_url, api_key, std::time::Duration::from_secs(30))?;
    client.put_file_bundle(file_id, bundle).await
}
