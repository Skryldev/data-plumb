//! `/internal/*` — replication endpoints (guarded by X-Api-Key).
//!
//! * `POST /internal/chunk?hash=...&plain_hash=...&...`   body: raw blob
//! * `GET  /internal/chunk/:hash`                         body: raw blob
//! * `POST /internal/manifest`                            body: msgpack manifest
//! * `DELETE /internal/manifest/:id`
//! * `POST /internal/batch`                               body: frame batch
//! * `GET  /internal/file/:id`                            body: file bundle
//! * `POST /internal/file/:id`                            apply file bundle

use axum::body::Body;
use axum::extract::{Path, Query, Request, State};
use axum::http::{header, StatusCode};
use axum::response::{IntoResponse, Response};
use axum::Json;
use bytes::Bytes;
use serde::Deserialize;
use uuid::Uuid;

use crate::error::StorageError;
use crate::metrics;
use crate::replication::client::{self, ChunkPayload};
use crate::security::audit::{Actor, AuditAction, AuditLog};
use crate::security::tenant::TenantStore;
use crate::state::AppState;

#[derive(Debug, Deserialize)]
pub struct ChunkQuery {
    pub hash: String,
    pub plain_hash: Option<String>,
    pub size_original: Option<u64>,
    pub size_stored: Option<u64>,
    pub compressed: Option<bool>,
    pub encrypted: Option<bool>,
}

fn octet(body: impl Into<Body>) -> Response {
    (
        [(header::CONTENT_TYPE, "application/octet-stream")],
        body.into(),
    )
        .into_response()
}

/// POST /internal/chunk
pub async fn internal_put_chunk(
    State(state): State<AppState>,
    Query(q): Query<ChunkQuery>,
    request: Request<Body>,
) -> Result<impl IntoResponse, StorageError> {
    let hash = parse_hash(&q.hash)?;
    let plain_hash = match &q.plain_hash {
        Some(h) => parse_hash(h)?,
        None => hash,
    };
    let blob = read_body(request).await?;
    apply_chunk(
        &state,
        ChunkPayload {
            hash,
            plain_hash,
            size_original: q.size_original.unwrap_or(blob.len() as u64),
            size_stored: q.size_stored.unwrap_or(blob.len() as u64),
            compressed: q.compressed.unwrap_or(false),
            encrypted: q.encrypted.unwrap_or(false),
            blob,
        },
    )
    .await?;
    Ok((StatusCode::CREATED, Json(serde_json::json!({"applied": 1}))))
}

/// GET /internal/chunk/:hash
pub async fn internal_get_chunk(
    State(state): State<AppState>,
    Path(hash_hex): Path<String>,
) -> Result<Response, StorageError> {
    let hash = parse_hash(&hash_hex)?;
    let blob = state.0.blocks.read(&hash).await?;
    Ok(octet(blob))
}

/// POST /internal/manifest (msgpack)
pub async fn internal_put_manifest(
    State(state): State<AppState>,
    request: Request<Body>,
) -> Result<impl IntoResponse, StorageError> {
    let body = read_body(request).await?;
    let manifest = client::decode_manifest(&body)?;
    apply_manifest(&state, manifest).await?;
    Ok((StatusCode::CREATED, Json(serde_json::json!({"applied": 1}))))
}

/// DELETE /internal/manifest/:id
pub async fn internal_purge_manifest(
    State(state): State<AppState>,
    Path(file_id): Path<Uuid>,
) -> Result<impl IntoResponse, StorageError> {
    state.0.index.remove_manifest(&file_id).await?;
    state.0.cache.manifests.invalidate(&file_id).await;
    Ok(StatusCode::NO_CONTENT)
}

/// POST /internal/batch (binary frames)
pub async fn internal_batch(
    State(state): State<AppState>,
    request: Request<Body>,
) -> Result<impl IntoResponse, StorageError> {
    let body = read_body(request).await?;
    let items = client::decode_batch(&body)?;
    let mut applied = 0u64;
    for p in items {
        apply_chunk(&state, p).await?;
        applied += 1;
    }
    Ok((
        StatusCode::CREATED,
        Json(serde_json::json!({"applied": applied})),
    ))
}

/// GET /internal/file/:id — full bundle (manifest + chunk blobs).
pub async fn internal_get_file(
    State(state): State<AppState>,
    Path(file_id): Path<Uuid>,
) -> Result<Response, StorageError> {
    let manifest = state
        .0
        .index
        .get_manifest(&file_id)
        .await?
        .ok_or_else(|| StorageError::NotFound(format!("file {file_id}")))?;
    let mut payloads = Vec::with_capacity(manifest.chunks.len());
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
    let bundle = client::encode_file_bundle(&manifest, &payloads)?;
    Ok(octet(bundle))
}

/// POST /internal/file/:id — apply a full bundle (pull/resync path).
pub async fn internal_apply_file(
    State(state): State<AppState>,
    Path(file_id): Path<Uuid>,
    request: Request<Body>,
) -> Result<impl IntoResponse, StorageError> {
    let body = read_body(request).await?;
    let (manifest, chunks) = client::decode_file_bundle(&body)?;
    if manifest.file_id != file_id {
        return Err(StorageError::Validation(format!(
            "bundle file_id {} does not match path {file_id}",
            manifest.file_id
        )));
    }
    for p in chunks {
        apply_chunk(&state, p).await?;
    }
    apply_manifest(&state, manifest).await?;
    Ok((
        StatusCode::CREATED,
        Json(serde_json::json!({"applied": true})),
    ))
}

// -- apply helpers -----------------------------------------------------------

async fn apply_chunk(state: &AppState, p: ChunkPayload) -> Result<(), StorageError> {
    let path = state.0.blocks.relative_path(&p.hash);
    // Idempotent atomic write (exists -> no-op), then metadata-only-if-absent.
    state.0.blocks.write(&p.hash, &p.blob).await?;
    state
        .0
        .index
        .upsert_chunk_replica(&p.hash, p.blob.len() as u64, &path)
        .await?;
    metrics::record_replication_applied(1);
    Ok(())
}

async fn apply_manifest(
    state: &AppState,
    m: crate::storage::manifest::Manifest,
) -> Result<(), StorageError> {
    let store = TenantStore::new(state.0.index.clone());
    store
        .ensure(&m.tenant_id, &format!("tenant-{}", m.tenant_id.simple()))
        .await?;
    state.0.index.put_manifest(&m).await?;
    state.0.cache.manifests.invalidate(&m.file_id).await;
    metrics::record_replication_applied(1);
    AuditLog::new(state.0.index.clone())
        .record(
            AuditAction::ReplicationApply,
            Actor::System,
            Some(m.tenant_id),
            &m.name,
            "",
            "ok",
            &format!("file_id={}", m.file_id),
        )
        .await;
    Ok(())
}

fn parse_hash(s: &str) -> Result<[u8; 32], StorageError> {
    let bytes = hex::decode(s.trim())
        .map_err(|e| StorageError::Validation(format!("invalid hash hex: {e}")))?;
    bytes
        .try_into()
        .map_err(|_| StorageError::Validation("hash must be 32 bytes (64 hex chars)".into()))
}

async fn read_body(request: Request<Body>) -> Result<Bytes, StorageError> {
    let body = request.into_body();
    let bytes = axum::body::to_bytes(body, state_body_limit())
        .await
        .map_err(|e| StorageError::Internal(format!("body read: {e}")))?;
    Ok(bytes)
}

/// Internal endpoints accept bundles up to a fixed 2 GiB ceiling.
fn state_body_limit() -> usize {
    2 * 1024 * 1024 * 1024 // 2 GiB hard ceiling for internal transfers
}
