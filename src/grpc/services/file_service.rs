//! `file_storage.v1.FileService` — upload (client-streaming), download
//! (server-streaming), manifest, listing and deletion. Bridges onto the
//! same `UploadPipeline` / `RetrievalEngine` the REST API uses, so both
//! transports share one storage core.

use bytes::Bytes;
use futures::StreamExt as _;
use tokio_stream::wrappers::ReceiverStream;
use tonic::{Request, Response, Status, Streaming};

use crate::api::middleware::tenant::check_file_access;
use crate::error::StorageError;
use crate::grpc::convert::{manifest_to_proto, ts_from_ms, uuid_from_str};
use crate::grpc::generated::pb;
use crate::grpc::generated::FileService;
use crate::grpc::proto_auth::{context_of, request_id_of, status_from_error};
use crate::security::audit::{Actor, AuditAction};
use crate::security::tenant::TenantStore;
use crate::state::AppState;
use crate::storage::manifest::Manifest;

/// Channel depth for streamed downloads — bounds server memory per stream
/// and applies backpressure through HTTP/2 flow control.
const DOWNLOAD_CHANNEL_DEPTH: usize = 32;

/// Byte pieces emitted by the download stream.
const DOWNLOAD_PIECE_SIZE: usize = 256 * 1024;

pub struct FileServiceImpl {
    state: AppState,
}

impl FileServiceImpl {
    pub fn new(state: AppState) -> Self {
        Self { state }
    }
}

/// Streaming download tasks post results through this channel wrapper.
pub type DownloadStream = ReceiverStream<Result<pb::DownloadChunk, Status>>;

#[tonic::async_trait]
impl FileService for FileServiceImpl {
    /// Server-streaming download wire type.
    type DownloadStream = DownloadStream;

    /// Client-streaming upload. The first message must carry the
    /// `UploadHeader`; the rest are raw byte pieces. The shared upload
    /// pipeline performs CDC chunking, hashing, compression and (optional)
    /// encryption, and commits the manifest atomically.
    async fn upload(
        &self,
        request: Request<Streaming<pb::UploadChunk>>,
    ) -> Result<Response<pb::UploadResponse>, Status> {
        let ctx = context_of(&self.state, &request)?;
        ctx.require_write().map_err(|e| status_from_error(&e))?;
        let request_id = request_id_of(&request);
        let mut inbound = request.into_inner();

        // 1. Header — must be exactly the first message.
        let first = inbound
            .message()
            .await?
            .ok_or_else(|| Status::invalid_argument("empty upload stream"))?;
        let header = match first.payload {
            Some(pb::upload_chunk::Payload::Header(h)) => h,
            Some(pb::upload_chunk::Payload::Data(_)) | None => {
                return Err(Status::invalid_argument(
                    "first message must be the upload header",
                ))
            }
        };

        let name = header.name.trim();
        if name.is_empty() || name.len() > 255 {
            return Err(Status::invalid_argument(
                "name must be 1..=255 bytes after trim",
            ));
        }
        let name = name.to_string();

        // 2. Tenant selection: callers stay in their own tenant; admins may
        //    target any tenant via the header override (created on demand).
        let tenant_id = if header.tenant_id.trim().is_empty() {
            ctx.tenant_id
        } else {
            let requested =
                uuid_from_str(&header.tenant_id, "tenant_id").map_err(|e| status_from_error(&e))?;
            if requested != ctx.tenant_id && !ctx.role.is_admin() {
                return Err(Status::permission_denied(
                    "cross-tenant upload requires the admin role",
                ));
            }
            TenantStore::new(self.state.0.index.clone())
                .ensure(&requested, &format!("tenant-{requested}"))
                .await
                .map_err(|e| status_from_error(&e))?;
            requested
        };

        let mime = if header.mime_type.trim().is_empty() {
            "application/octet-stream".to_string()
        } else {
            header.mime_type.trim().to_string()
        };
        let mut tags: Vec<String> = Vec::new();
        for t in header.tags {
            let t = t.trim().to_string();
            if !t.is_empty() && !tags.contains(&t) {
                tags.push(t);
            }
        }
        let expected = (header.expected_size > 0).then_some(header.expected_size);

        // 3. Bridge the gRPC stream into the pipeline input stream. The
        //    bridge enforces expected_size and the global size cap so the
        //    server never buffers an unbounded body.
        let max_size = self.state.0.cfg.limits.max_file_size;
        let input = async_stream::try_stream! {
            let mut seen: u64 = 0;
            while let Some(msg) = inbound.message().await.map_err(|e| {
                StorageError::Internal(format!("upload stream failed: {e}"))
            })? {
                match msg.payload {
                    Some(pb::upload_chunk::Payload::Data(data)) => {
                        seen += data.len() as u64;
                        if seen > max_size {
                            Err(StorageError::TooLarge(format!(
                                "stream exceeded the {max_size}-byte limit"
                            )))?;
                        }
                        if let Some(exp) = expected {
                            if seen > exp {
                                Err(StorageError::TooLarge(format!(
                                    "stream exceeded the declared expected_size ({exp})"
                                )))?;
                            }
                        }
                        yield Bytes::from(data);
                    }
                    Some(pb::upload_chunk::Payload::Header(_)) => {
                        Err(StorageError::Validation("duplicate upload header".to_string()))?;
                    }
                    None => {}
                }
            }
        };
        let input = Box::pin(input);

        // 4. Same pipeline as REST: chunk → hash → compress → encrypt →
        //    store → commit manifest (single redb transaction).
        let outcome = self
            .state
            .0
            .pipeline
            .process(tenant_id, &name, &mime, tags, input)
            .await
            .map_err(|e| status_from_error(&e))?;

        let manifest = &outcome.manifest;
        let stored_bytes = manifest.size_stored;
        let compression_ratio = if stored_bytes > 0 {
            manifest.size_original as f64 / stored_bytes as f64
        } else {
            1.0
        };

        tracing::info!(
            request_id,
            file_id = %manifest.file_id,
            size = manifest.size_original,
            chunks = manifest.chunk_count(),
            "grpc upload ok"
        );

        Ok(Response::new(pb::UploadResponse {
            manifest: Some(manifest_to_proto(manifest, true)),
            deduped_bytes: outcome.dedup_bytes,
            stored_bytes,
            dedup_chunks: outcome.dedup_chunks as u32,
            compression_ratio,
        }))
    }

    /// Server-streaming download: header first, then ordered plaintext
    /// pieces of the requested range. Range semantics match the proto
    /// (`start` inclusive, `end` exclusive; both optional).
    async fn download(
        &self,
        request: Request<pb::DownloadRequest>,
    ) -> Result<Response<DownloadStream>, Status> {
        let ctx = context_of(&self.state, &request)?;
        let req = request.into_inner();
        let file_id = uuid_from_str(&req.file_id, "file_id").map_err(|e| status_from_error(&e))?;

        let manifest = self
            .state
            .0
            .retrieval
            .manifest(&file_id)
            .await
            .map_err(|e| status_from_error(&e))?;
        check_file_access(&ctx, &manifest.tenant_id).map_err(|e| status_from_error(&e))?;

        let total = manifest.size_original;
        let start = req.start.unwrap_or(0);
        let end = req.end.unwrap_or(total); // exclusive
        if end < start {
            return Err(Status::invalid_argument(
                "end must be >= start (end is exclusive)",
            ));
        }
        if total == 0 && (req.start.is_some() || req.end.is_some()) {
            // Range on an empty file is unsatisfiable, matching REST.
            return Err(status_from_error(&StorageError::RangeNotSatisfiable(0)));
        }
        if start >= total {
            return Err(status_from_error(&StorageError::RangeNotSatisfiable(total)));
        }
        let end = end.min(total);
        let length = end - start;

        let (tx, rx) = tokio::sync::mpsc::channel(DOWNLOAD_CHANNEL_DEPTH);
        let username = ctx.username.clone();
        let state = self.state.clone();
        tokio::spawn(async move {
            let header = pb::DownloadChunk {
                payload: Some(pb::download_chunk::Payload::Header(pb::DownloadHeader {
                    file_id: file_id.to_string(),
                    size: length,
                    total_size: total,
                    mime_type: manifest.mime.clone(),
                    original_hash: manifest.original_hash.to_vec(),
                    start,
                    end,
                })),
            };
            if tx.send(Ok(header)).await.is_err() {
                return; // client went away before the first frame
            }

            let started = std::time::Instant::now();
            let stream: std::pin::Pin<
                Box<dyn futures::Stream<Item = Result<Bytes, StorageError>> + Send>,
            > = if start == 0 && end == total {
                Box::pin(state.0.retrieval.stream_full(manifest.clone()))
            } else {
                Box::pin(state.0.retrieval.stream_range(manifest.clone(), start, end))
            };

            let mut sent: u64 = 0;
            let mut failure: Option<StorageError> = None;
            futures::pin_mut!(stream);
            'outer: while let Some(item) = stream.next().await {
                match item {
                    Ok(data) => {
                        for piece in split_bytes(data, DOWNLOAD_PIECE_SIZE) {
                            let frame = pb::DownloadChunk {
                                payload: Some(pb::download_chunk::Payload::Data(piece.to_vec())),
                            };
                            if tx.send(Ok(frame)).await.is_err() {
                                break 'outer; // client disconnected
                            }
                            sent += piece.len() as u64;
                        }
                    }
                    Err(e) => {
                        failure = Some(e);
                        break;
                    }
                }
            }

            match failure {
                None => {
                    crate::metrics::record_download(
                        started.elapsed().as_secs_f64(),
                        "ok",
                        sent,
                        "grpc",
                    );
                    audit_download(&state, &username, &manifest.name, "ok").await;
                }
                Some(e) => {
                    let _ = tx.send(Err(status_from_error(&e))).await;
                    audit_download(&state, &username, &manifest.name, "error").await;
                }
            }
        });

        Ok(Response::new(ReceiverStream::new(rx)))
    }

    /// Manifest lookup without content. Soft-deleted files read as
    /// not-found (same as REST).
    async fn get_manifest(
        &self,
        request: Request<pb::GetManifestRequest>,
    ) -> Result<Response<pb::Manifest>, Status> {
        let ctx = context_of(&self.state, &request)?;
        let req = request.into_inner();
        let file_id = uuid_from_str(&req.file_id, "file_id").map_err(|e| status_from_error(&e))?;
        let manifest = self
            .state
            .0
            .retrieval
            .manifest(&file_id)
            .await
            .map_err(|e| status_from_error(&e))?;
        check_file_access(&ctx, &manifest.tenant_id).map_err(|e| status_from_error(&e))?;
        Ok(Response::new(manifest_to_proto(
            &manifest,
            req.include_chunks,
        )))
    }

    /// Tenant-scoped listing with tag/name filters, mirroring the REST
    /// list semantics (tag precedence, exact-name match).
    async fn list_files(
        &self,
        request: Request<pb::ListFilesRequest>,
    ) -> Result<Response<pb::ListFilesResponse>, Status> {
        let ctx = context_of(&self.state, &request)?;
        let req = request.into_inner();
        let limit = if req.limit == 0 {
            50
        } else {
            (req.limit as usize).clamp(1, 1000)
        };
        let offset = req.offset as usize;
        let include_deleted = req.include_deleted && ctx.role.is_admin();

        let index = &self.state.0.index;
        let tag = non_empty(&req.tag);
        let name = non_empty(&req.name);

        let manifests: Vec<Manifest> = if let Some(tag) = tag {
            let ids = index.list_ids_by_tag(&ctx.tenant_id, tag).await?;
            let mut out = Vec::new();
            for id in ids {
                if let Some(m) = index.get_manifest(&id).await? {
                    if include_deleted || !m.is_deleted() {
                        out.push(m);
                    }
                }
            }
            out
        } else if let Some(name) = name {
            match index.get_by_name(&ctx.tenant_id, name).await? {
                Some(m) if include_deleted || !m.is_deleted() => vec![m],
                _ => Vec::new(),
            }
        } else {
            index
                .list_by_tenant(&ctx.tenant_id, include_deleted, limit, offset)
                .await?
        };

        let total = manifests.len().min(u32::MAX as usize) as u32;
        let files: Vec<pb::Manifest> = manifests
            .iter()
            .take(limit)
            .map(|m| manifest_to_proto(m, false))
            .collect();

        Ok(Response::new(pb::ListFilesResponse { files, total }))
    }

    /// Soft delete by default; `purge` (admin) hard-deletes, dropping
    /// chunk refcounts and replicating the removal.
    async fn delete_file(
        &self,
        request: Request<pb::DeleteFileRequest>,
    ) -> Result<Response<pb::DeleteFileResponse>, Status> {
        let ctx = context_of(&self.state, &request)?;
        ctx.require_write().map_err(|e| status_from_error(&e))?;
        let req = request.into_inner();
        let file_id = uuid_from_str(&req.file_id, "file_id").map_err(|e| status_from_error(&e))?;

        let manifest = self
            .state
            .0
            .index
            .get_manifest(&file_id)
            .await
            .map_err(|e| status_from_error(&e))?
            .ok_or_else(|| Status::not_found(format!("file {file_id}")))?;
        check_file_access(&ctx, &manifest.tenant_id).map_err(|e| status_from_error(&e))?;
        if req.purge {
            ctx.require_admin().map_err(|e| status_from_error(&e))?;
        }

        let mut deleted_at: Option<prost_types::Timestamp> = None;
        let mut purged = false;
        if req.purge {
            purge_file(&self.state, &manifest)
                .await
                .map_err(|e| status_from_error(&e))?;
            purged = true;
            self.state
                .0
                .audit
                .record(
                    AuditAction::Purge,
                    Actor::User(ctx.username.clone()),
                    Some(manifest.tenant_id),
                    &manifest.name,
                    "",
                    "ok",
                    &format!("file_id={file_id} grpc"),
                )
                .await;
        } else {
            let removed = self
                .state
                .0
                .index
                .soft_delete_manifest(&file_id)
                .await
                .map_err(|e| status_from_error(&e))?
                .ok_or_else(|| Status::not_found(format!("file {file_id}")))?;
            self.state.0.cache.manifests.invalidate(&file_id).await;
            self.state
                .0
                .replicator
                .enqueue_manifest(file_id)
                .await
                .map_err(|e| status_from_error(&e))?;
            deleted_at = removed.deleted_at_ms.map(ts_from_ms);
            self.state
                .0
                .audit
                .record(
                    AuditAction::Delete,
                    Actor::User(ctx.username.clone()),
                    Some(manifest.tenant_id),
                    &manifest.name,
                    "",
                    "ok",
                    &format!("file_id={file_id} soft grpc"),
                )
                .await;
        }

        Ok(Response::new(pb::DeleteFileResponse { deleted_at, purged }))
    }
}

/// Hard-purge implementation mirroring the REST `hard_purge` helper.
pub async fn purge_file(state: &AppState, manifest: &Manifest) -> Result<(), StorageError> {
    let hashes: Vec<[u8; 32]> = manifest.chunks.iter().map(|c| c.hash).collect();
    state.0.index.remove_manifest(&manifest.file_id).await?;
    state.0.cache.manifests.invalidate(&manifest.file_id).await;
    let zeroed = state.0.index.decr_refcounts(&hashes).await?;
    for h in &zeroed {
        state.0.blocks.delete(h).await?;
    }
    state.0.replicator.enqueue_purge(manifest.file_id).await?;
    Ok(())
}

async fn audit_download(state: &AppState, username: &str, object: &str, status: &str) {
    state
        .0
        .audit
        .record(
            AuditAction::Download,
            Actor::User(username.to_string()),
            None,
            object,
            "",
            status,
            "grpc",
        )
        .await;
}

/// Returns the trimmed string when non-empty.
fn non_empty(s: &str) -> Option<&str> {
    let t = s.trim();
    (!t.is_empty()).then_some(t)
}

/// Splits a byte buffer into ≤ `max` pieces without copying when possible.
fn split_bytes(data: Bytes, max: usize) -> Vec<Bytes> {
    if data.len() <= max {
        return vec![data];
    }
    data.chunks(max).map(Bytes::copy_from_slice).collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn split_respects_max() {
        let data = Bytes::from(vec![7u8; 10_000]);
        let pieces = split_bytes(data.clone(), 256 * 1024);
        assert_eq!(pieces.len(), 1);
        let pieces = split_bytes(data, 3_000);
        assert_eq!(pieces.len(), 4);
        assert_eq!(pieces.iter().map(|p| p.len()).sum::<usize>(), 10_000);
    }
}
