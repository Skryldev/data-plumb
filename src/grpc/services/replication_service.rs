//! `file_storage.v1.ReplicationService` — the internal peer protocol
//! (guarded by `x-api-key`): batch replication ingestion, chunk pulls for
//! repair and manifest pulls.
//!
//! The built-in WAL shipper still uses the plain-HTTP `/internal/*`
//! endpoints; this service exposes the same apply logic over gRPC so
//! external tooling and future shipper generations can use one protocol.

use bytes::Bytes;
use tokio_stream::wrappers::ReceiverStream;
use tonic::{Request, Response, Status, Streaming};

use crate::error::StorageError;
use crate::grpc::convert::{manifest_from_proto, manifest_to_proto};
use crate::grpc::generated::pb;
use crate::grpc::generated::ReplicationService;
use crate::grpc::proto_auth::{require_api_key, status_from_error};
use crate::metrics;
use crate::security::audit::{Actor, AuditAction, AuditLog};
use crate::security::tenant::TenantStore;
use crate::state::AppState;

/// Streaming piece size for PullChunk responses.
const PULL_PIECE_SIZE: usize = 64 * 1024;

pub struct ReplicationServiceImpl {
    state: AppState,
}

impl ReplicationServiceImpl {
    pub fn new(state: AppState) -> Self {
        Self { state }
    }
}

/// The bidirectional ReplicateBatch stream type.
pub type ReplicateStream = ReceiverStream<Result<pb::ReplicationAck, Status>>;

/// The PullChunk server-stream type.
pub type PullStream = ReceiverStream<Result<pb::PullChunkResponse, Status>>;

#[tonic::async_trait]
impl ReplicationService for ReplicationServiceImpl {
    /// Wire types for the two server-streaming directions.
    type ReplicateBatchStream = ReplicateStream;

    type PullChunkStream = PullStream;

    /// Applies streamed ops until the Flush sentinel, then acks the batch.
    /// All applies are idempotent (atomic blob writes + metadata-only-if-
    /// absent), so replaying a batch after a crash is safe.
    async fn replicate_batch(
        &self,
        request: Request<Streaming<pb::ReplicationOp>>,
    ) -> Result<Response<ReplicateStream>, Status> {
        require_api_key(&self.state, &request)?;
        let mut inbound = request.into_inner();

        let (tx, rx) = tokio::sync::mpsc::channel::<Result<pb::ReplicationAck, Status>>(8);
        let state = self.state.clone();
        tokio::spawn(async move {
            let mut applied: u32 = 0;
            let mut failure: Option<Status> = None;
            loop {
                match inbound.message().await {
                    Ok(Some(op)) => match op.op {
                        Some(pb::replication_op::Op::PutChunk(put)) => {
                            match apply_put_chunk(&state, put).await {
                                Ok(()) => applied += 1,
                                Err(e) => {
                                    failure = Some(status_from_error(&e));
                                    break;
                                }
                            }
                        }
                        Some(pb::replication_op::Op::PutManifest(put)) => {
                            match apply_put_manifest(&state, put).await {
                                Ok(()) => applied += 1,
                                Err(e) => {
                                    failure = Some(status_from_error(&e));
                                    break;
                                }
                            }
                        }
                        Some(pb::replication_op::Op::DeleteFile(del)) => {
                            match apply_delete_file(&state, del).await {
                                Ok(()) => applied += 1,
                                Err(e) => {
                                    failure = Some(status_from_error(&e));
                                    break;
                                }
                            }
                        }
                        Some(pb::replication_op::Op::Flush(_)) => break,
                        None => break, // stream ended without a Flush
                    },
                    Ok(None) => break,
                    Err(e) => {
                        failure = Some(e);
                        break;
                    }
                }
            }

            let ack = match failure {
                None => pb::ReplicationAck {
                    ops_acked: applied,
                    success: true,
                    error: String::new(),
                },
                Some(e) => pb::ReplicationAck {
                    ops_acked: applied,
                    success: false,
                    error: e.message().to_string(),
                },
            };
            let success = ack.success;
            let _ = tx.send(Ok(ack)).await;
            if success {
                metrics::record_replication_applied(applied as u64);
            }
        });

        Ok(Response::new(ReceiverStream::new(rx)))
    }

    /// Streams the raw on-disk blob (compressed / encrypted) for one chunk
    /// — used for repair and on-demand fetch by replicas.
    async fn pull_chunk(
        &self,
        request: Request<pb::PullChunkRequest>,
    ) -> Result<Response<Self::PullChunkStream>, Status> {
        require_api_key(&self.state, &request)?;
        let req = request.into_inner();
        let hash: [u8; 32] = req
            .hash
            .as_slice()
            .try_into()
            .map_err(|_| Status::invalid_argument("hash must be 32 bytes"))?;

        let blob = self
            .state
            .0
            .blocks
            .read(&hash)
            .await
            .map_err(|e| status_from_error(&e))?;
        let size = blob.len() as u64;

        let (tx, rx) = tokio::sync::mpsc::channel::<Result<pb::PullChunkResponse, Status>>(8);
        tokio::spawn(async move {
            let header = pb::PullChunkResponse {
                payload: Some(pb::pull_chunk_response::Payload::Header(
                    pb::PullChunkHeader {
                        hash: hash.to_vec(),
                        size_stored: size,
                        compressed: false,
                        encrypted: false,
                    },
                )),
            };
            if tx.send(Ok(header)).await.is_err() {
                return;
            }
            for piece in blob.chunks(PULL_PIECE_SIZE.max(1)) {
                let frame = pb::PullChunkResponse {
                    payload: Some(pb::pull_chunk_response::Payload::Data(piece.to_vec())),
                };
                if tx.send(Ok(frame)).await.is_err() {
                    return;
                }
            }
        });

        Ok(Response::new(ReceiverStream::new(rx)))
    }

    /// Manifest (metadata only) for repair flows; blobs are fetched with
    /// PullChunk.
    async fn pull_manifest(
        &self,
        request: Request<pb::PullManifestRequest>,
    ) -> Result<Response<pb::ManifestBundle>, Status> {
        require_api_key(&self.state, &request)?;
        let req = request.into_inner();
        let file_id = crate::grpc::convert::uuid_from_str(&req.file_id, "file_id")
            .map_err(|e| status_from_error(&e))?;
        let manifest = self
            .state
            .0
            .index
            .get_manifest(&file_id)
            .await
            .map_err(|e| status_from_error(&e))?
            .ok_or_else(|| Status::not_found(format!("file {file_id}")))?;
        Ok(Response::new(pb::ManifestBundle {
            manifest: Some(manifest_to_proto(&manifest, true)),
        }))
    }
}

/// Applies one PutChunk op (mirrors `/internal/chunk` semantics).
async fn apply_put_chunk(state: &AppState, put: pb::PutChunk) -> Result<(), StorageError> {
    let hash: [u8; 32] = put
        .hash
        .as_slice()
        .try_into()
        .map_err(|_| StorageError::Validation("chunk hash must be 32 bytes".into()))?;
    let plain_hash: [u8; 32] = put
        .plain_hash
        .as_slice()
        .try_into()
        .map_err(|_| StorageError::Validation("plain_hash must be 32 bytes".into()))?;
    let payload = crate::replication::client::ChunkPayload {
        hash,
        plain_hash,
        size_original: put.size_original,
        size_stored: put.size_stored,
        compressed: put.compressed,
        encrypted: put.encrypted,
        blob: Bytes::from(put.data),
    };
    // Blob write is idempotent; the chunk row is created only when absent.
    let path = state.0.blocks.relative_path(&payload.hash);
    state.0.blocks.write(&payload.hash, &payload.blob).await?;
    state
        .0
        .index
        .upsert_chunk_replica(&payload.hash, payload.blob.len() as u64, &path)
        .await?;
    metrics::record_replication_applied(1);
    Ok(())
}

/// Applies one PutManifest op: ensures the tenant exists, stores the
/// manifest, invalidates the manifest cache and audits.
async fn apply_put_manifest(state: &AppState, put: pb::PutManifest) -> Result<(), StorageError> {
    let proto = put
        .manifest
        .ok_or_else(|| StorageError::Validation("PutManifest without manifest".into()))?;
    let manifest = manifest_from_proto(&proto)?;
    TenantStore::new(state.0.index.clone())
        .ensure(
            &manifest.tenant_id,
            &format!("tenant-{}", manifest.tenant_id.simple()),
        )
        .await?;
    state.0.index.put_manifest(&manifest).await?;
    state.0.cache.manifests.invalidate(&manifest.file_id).await;
    metrics::record_replication_applied(1);
    AuditLog::new(state.0.index.clone())
        .record(
            AuditAction::ReplicationApply,
            Actor::System,
            Some(manifest.tenant_id),
            &manifest.name,
            "",
            "ok",
            &format!("file_id={} grpc", manifest.file_id),
        )
        .await;
    Ok(())
}

/// Applies a DeleteFile op: hard purge (`purge=true`) or soft-delete
/// marker propagation.
async fn apply_delete_file(state: &AppState, del: pb::DeleteFile) -> Result<(), StorageError> {
    let file_id = crate::grpc::convert::uuid_from_str(&del.file_id, "file_id")?;
    if del.purge {
        let manifest = state
            .0
            .index
            .get_manifest(&file_id)
            .await?
            .ok_or_else(|| StorageError::NotFound(format!("file {file_id}")))?;
        let hashes: Vec<[u8; 32]> = manifest.chunks.iter().map(|c| c.hash).collect();
        state.0.index.remove_manifest(&file_id).await?;
        state.0.cache.manifests.invalidate(&file_id).await;
        let zeroed = state.0.index.decr_refcounts(&hashes).await?;
        for h in &zeroed {
            state.0.blocks.delete(h).await?;
        }
    } else {
        state
            .0
            .index
            .soft_delete_manifest(&file_id)
            .await?
            .ok_or_else(|| StorageError::NotFound(format!("file {file_id}")))?;
        state.0.cache.manifests.invalidate(&file_id).await;
    }
    Ok(())
}
