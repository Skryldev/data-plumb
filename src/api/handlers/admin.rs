//! Admin operations: GC trigger, file resync, audit listing.

use axum::extract::{Path, Query, State};
use axum::http::StatusCode;
use axum::response::IntoResponse;
use axum::Extension;
use axum::Json;
use uuid::Uuid;

use crate::api::dto::{AuditEntryDto, ResyncDto};
use crate::api::middleware::auth::require_admin;
use crate::error::StorageError;
use crate::gc::ReconcileMode;
use crate::replication::client::{ChunkPayload, ReplicationClient};
use crate::security::audit::{Actor, AuditAction};
use crate::security::auth::AuthContext;
use crate::security::tenant::check_access;
use crate::state::AppState;

/// M-01: `POST /admin/gc?reconcile=true&dry_run=true`.
#[derive(Debug, serde::Deserialize, Default)]
pub struct GcQuery {
    pub reconcile: Option<bool>,
    pub dry_run: Option<bool>,
}

pub async fn trigger_gc(
    State(state): State<AppState>,
    Extension(ctx): Extension<AuthContext>,
    Query(q): Query<GcQuery>,
) -> Result<impl IntoResponse, StorageError> {
    // M-04a parity: admin-only, enforced by require_admin + the router.
    require_admin(&ctx)?;
    if q.reconcile.unwrap_or(false) {
        let mode = if q.dry_run.unwrap_or(false) {
            ReconcileMode::DryRun
        } else {
            ReconcileMode::Repair
        };
        let report = state.0.gc.reconcile(mode).await?;
        // M-01: every admin reconcile trigger is audited with actor + mode
        // + outcome counts.
        state
            .0
            .audit
            .record(
                AuditAction::GcRun,
                Actor::User(ctx.username.clone()),
                None,
                "admin/gc",
                "",
                "ok",
                &format!(
                    "reconcile mode={mode:?} orphans={} tombstoned={} reclaimed={}",
                    report.orphan_chunks, report.tombstoned, report.reclaimed
                ),
            )
            .await;
        return Ok(Json(serde_json::json!({
            "mode": mode,
            "scanned_blobs": report.scanned_blobs,
            "scanned_index_entries": report.scanned_index_entries,
            "referenced_chunks": report.referenced_chunks,
            "orphan_chunks": report.orphan_chunks,
            "phantom_manifests": report.phantom_manifests,
            "tombstoned": report.tombstoned,
            "reclaimed": report.reclaimed,
            "duration_ms": report.duration.as_millis() as u64,
        })));
    }
    let report = state.0.gc.run_once().await?;
    state
        .0
        .audit
        .record(
            AuditAction::GcRun,
            Actor::User(ctx.username.clone()),
            None,
            "admin/gc",
            "",
            "ok",
            "plain sweep",
        )
        .await;
    Ok(Json(serde_json::json!({
        "manifests_purged": report.manifests_purged,
        "chunks_removed": report.chunks_removed,
        "orphans_removed": report.orphans_removed,
        "bytes_reclaimed": report.bytes_reclaimed,
    })))
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
