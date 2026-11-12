//! Upload pipeline: validate → chunk → hash → compress → encrypt → store.
//!
//! Concurrency model: chunking and whole-file hashing are strictly sequential
//! (`then` polls in stream order), while compression/encryption/blob-write run
//! `PIPELINE_CONCURRENCY`-wide via `buffer_unordered`. Chunk metadata is then
//! registered in ONE atomic redb transaction (single fsync), so dedup
//! refcounts are race-free; blobs are written before that transaction, so a
//! crash can only leave an orphan blob which GC sweeps later.

use bytes::Bytes;
use futures::{Stream, StreamExt};
use std::collections::BTreeMap;
use std::sync::atomic::{AtomicU64, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use uuid::Uuid;

use crate::config::AppConfig;
use crate::core::chunker;
use crate::core::compressor::{CompressOutcome, Compressor};
use crate::core::hasher::{hash_bytes, StreamingHasher};
use crate::core::validator::FileValidator;
use crate::error::StorageError;
use crate::metrics;
use crate::replication::client::ChunkPayload;
use crate::replication::replicator::Replicator;
use crate::security::audit::{Actor, AuditAction, AuditLog};
use crate::security::encryption::Encryptor;
use crate::storage::block_store::BlockStore;
use crate::storage::index_db::{now_ms, IndexDb};
use crate::storage::manifest::{ChunkRef, Manifest};

/// Max in-flight chunk processing tasks.
pub const PIPELINE_CONCURRENCY: usize = 8;

/// A chunk after hashing but before storage.
struct RawChunk {
    idx: usize,
    offset: u64,
    data: Bytes,
    plain_hash: [u8; 32],
    storage_hash: [u8; 32],
}

/// A chunk after the FULL per-chunk stage (H-01): blob written, registered
/// (verified), replication enqueued. Only metadata is retained — the
/// compressed bytes never outlive their chunk's processing.
struct StoredChunk {
    idx: usize,
    offset: u64,
    plain_hash: [u8; 32],
    storage_hash: [u8; 32],
    compressed: bool,
    encrypted: bool,
    size_original: u64,
    size_stored: u64,
    /// C-03/H-01: whether the fused registration dedup-hit.
    dedup_hit: bool,
}

pub struct UploadPipeline {
    index: Arc<IndexDb>,
    blocks: Arc<BlockStore>,
    compressor: Arc<Compressor>,
    encryptor: Option<Arc<Encryptor>>,
    replicator: Arc<Replicator>,
    audit: Arc<AuditLog>,
    /// H-03: the magic-byte validator, wired into the upload path.
    validator: Arc<FileValidator>,
    validation_mode: crate::config::ValidationMode,
    chunking: crate::config::ChunkingConfig,
    max_size: u64,
    sync_replication: bool,
}

/// Result of a completed upload.
#[derive(Debug, Clone)]
pub struct UploadOutcome {
    pub manifest: Manifest,
    pub dedup_chunks: usize,
    pub new_chunks: usize,
    pub dedup_bytes: u64,
}

impl UploadPipeline {
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        index: Arc<IndexDb>,
        blocks: Arc<BlockStore>,
        compressor: Arc<Compressor>,
        encryptor: Option<Arc<Encryptor>>,
        replicator: Arc<Replicator>,
        audit: Arc<AuditLog>,
        validator: Arc<FileValidator>,
        cfg: &AppConfig,
    ) -> Self {
        let sync_replication =
            cfg.replication.enabled && cfg.replication.mode == crate::config::ReplicationMode::Sync;
        Self {
            index,
            blocks,
            compressor,
            encryptor,
            replicator,
            audit,
            validator,
            validation_mode: cfg.validation.mode,
            chunking: cfg.chunking.clone(),
            max_size: cfg.limits.max_file_size,
            sync_replication,
        }
    }

    /// Processes an entire upload stream into a committed manifest.
    pub async fn process<S>(
        &self,
        tenant_id: Uuid,
        name: &str,
        mime: &str,
        tags: Vec<String>,
        input: S,
    ) -> Result<UploadOutcome, StorageError>
    where
        S: Stream<Item = Result<Bytes, StorageError>> + Unpin,
    {
        let started = std::time::Instant::now();
        let chunked = chunker::chunk_stream(input, &self.chunking);
        let chunked = Box::pin(chunked);

        // Shared sequential state (guarded because async closures capture by
        // value; access order is still strictly sequential thanks to `then`).
        let file_hasher = Arc::new(Mutex::new(StreamingHasher::new()));
        let size_original = Arc::new(AtomicU64::new(0));
        let idx_counter = Arc::new(AtomicUsize::new(0));
        let encryptor = self.encryptor.clone();
        let tenant = tenant_id;
        let hasher_captured = file_hasher.clone();
        let size_captured = size_original.clone();

        // Sequential hashing stage: `map` closures run synchronously at poll
        // time, in stream order; the returned futures are then driven
        // concurrently by `buffer_unordered`.
        // H-03: the declared MIME is checked against the sniffed magic bytes
        // of the FIRST plaintext chunk, before any persistence. Enforce is
        // the default; `off` is an explicit operator opt-out.
        let validator = self.validator.clone();
        let validation_mode = self.validation_mode;
        let mime_for_check = Arc::new(mime.to_string());
        let index = self.index.clone();
        let replicator = self.replicator.clone();
        let processed = chunked.map(move |res| {
            let compressor = self.compressor.clone();
            let blocks = self.blocks.clone();
            let encryptor = encryptor.clone();
            let file_hasher = hasher_captured.clone();
            let size_original = size_captured.clone();
            let idx_counter = idx_counter.clone();
            let validator = validator.clone();
            let mime_for_check = mime_for_check.clone();
            let index = index.clone();
            let replicator = replicator.clone();
            let compressor = compressor.clone();
            let encryptor = encryptor.clone();
            let blocks = blocks.clone();
            async move {
                let (offset, data) = res?;
                if offset == 0 && validation_mode == crate::config::ValidationMode::Enforce {
                    validator
                        .validate_head(&mime_for_check, &data)
                        .map_err(|e| StorageError::UnsupportedMediaType(e.to_string()))?;
                }
                file_hasher.lock().expect("hasher lock").update(&data);
                size_original.fetch_add(data.len() as u64, Ordering::Relaxed);
                let plain_hash = hash_bytes(&data);
                let storage_hash = match &encryptor {
                    Some(_) => Encryptor::storage_key(&tenant, &plain_hash),
                    None => plain_hash,
                };
                let raw = RawChunk {
                    idx: idx_counter.fetch_add(1, Ordering::Relaxed),
                    offset,
                    data,
                    plain_hash,
                    storage_hash,
                };
                store_chunk(
                    raw, compressor, encryptor, blocks, index, replicator, tenant,
                )
                .await
            }
        });

        // H-01: per-chunk fused stage. Each chunk's compressed bytes live
        // only inside its own processing future: write blob → verified
        // registration (C-03 self-heal) → replication enqueue → retain
        // ChunkRef metadata only.
        let mut stored: BTreeMap<usize, StoredChunk> = BTreeMap::new();
        {
            let mut inflight = Box::pin(processed.buffer_unordered(PIPELINE_CONCURRENCY));
            while let Some(item) = inflight.next().await {
                let sc = item?;
                if size_original.load(Ordering::Relaxed) > self.max_size {
                    return Err(StorageError::TooLarge(format!(
                        "stream exceeded the {}-byte limit",
                        self.max_size
                    )));
                }
                stored.insert(sc.idx, sc);
            }
        }

        let original_hash = file_hasher.lock().expect("hasher lock").finalize();

        // Assemble ordered chunk refs from the retained metadata ONLY
        // (H-01: no blob bytes survive their chunk's processing stage).
        let dedup_chunks = stored.values().filter(|sc| sc.dedup_hit).count();
        let new_chunks = stored.len() - dedup_chunks;
        let dedup_bytes: u64 = stored
            .values()
            .filter(|sc| sc.dedup_hit)
            .map(|sc| sc.size_stored)
            .sum();
        let chunks: Vec<ChunkRef> = stored
            .into_values()
            .map(|sc| ChunkRef {
                hash: sc.storage_hash,
                plain_hash: sc.plain_hash,
                offset: sc.offset,
                size_original: sc.size_original,
                size_stored: sc.size_stored,
                compressed: sc.compressed,
                encrypted: sc.encrypted,
            })
            .collect();

        // Replace any previous file with the same name (soft-delete keeps its
        // chunk refcounts until GC purge).
        let previous = self.index.get_by_name(&tenant_id, name).await?;
        if let Some(old) = previous.filter(|m| !m.is_deleted()) {
            self.index.soft_delete_manifest(&old.file_id).await?;
        }

        let now = now_ms();
        let mut manifest = Manifest {
            file_id: Uuid::now_v7(),
            name: name.to_string(),
            tenant_id,
            mime: mime.to_string(),
            size_original: size_original.load(Ordering::Relaxed),
            size_stored: 0,
            original_hash,
            chunks,
            tags,
            created_at_ms: now,
            updated_at_ms: now,
            deleted_at_ms: None,
            version: 1,
        };
        manifest.recompute_stored_size();
        self.index.put_manifest(&manifest).await?;

        // Replication: chunk ops were enqueued per chunk (H-01); the
        // manifest op follows them (queue order preserved), then the
        // sync-mode barrier when configured.
        self.replicator.enqueue_manifest(manifest.file_id).await?;
        if self.sync_replication {
            self.replicator.wait_flushed().await?;
        }

        metrics::record_chunks_stored(manifest.chunk_count() as u64, dedup_chunks as u64);
        metrics::record_upload(
            started.elapsed().as_secs_f64(),
            "ok",
            size_original.load(Ordering::Relaxed),
            dedup_bytes,
        );
        self.audit
            .record(
                AuditAction::Upload,
                Actor::System,
                Some(tenant_id),
                name,
                "",
                "ok",
                &format!(
                    "file_id={} chunks={} dedup={dedup_chunks}",
                    manifest.file_id,
                    manifest.chunk_count()
                ),
            )
            .await;

        Ok(UploadOutcome {
            manifest,
            dedup_chunks,
            new_chunks,
            dedup_bytes,
        })
    }
}

/// Compress → encrypt → atomic blob write for one chunk.
/// H-01 fused per-chunk stage: compress → encrypt → write blob →
/// verified registration (C-03 self-heal, up to 3 attempts) → replication
/// enqueue. Returns metadata only; the compressed bytes are dropped here.
#[allow(clippy::too_many_arguments)]
async fn store_chunk(
    raw: RawChunk,
    compressor: Arc<Compressor>,
    encryptor: Option<Arc<Encryptor>>,
    blocks: Arc<BlockStore>,
    index: Arc<IndexDb>,
    replicator: Arc<Replicator>,
    tenant: Uuid,
) -> Result<StoredChunk, StorageError> {
    let CompressOutcome {
        data: comp_data,
        compressed,
        size_original,
        ..
    } = compressor.compress(&raw.data);
    let raw_size_original = size_original;
    let (blob, encrypted) = match &encryptor {
        Some(enc) => (Bytes::from(enc.encrypt(&tenant, &comp_data)?), true),
        None => (comp_data, false),
    };
    // Blob lands on disk before the index transaction that references it;
    // a crash in between leaves a GC-able orphan, never a dangling ref.
    blocks.write(&raw.storage_hash, &blob).await?;

    // C-03 verified registration (fused single-chunk variant): a blob lost
    // to a racing GC/purge is rewritten (idempotent CAS) and retried; a
    // final rejection aborts the upload cleanly (409 / FAILED_PRECONDITION).
    let path = blocks.relative_path(&raw.storage_hash);
    let mut outcome = index
        .register_chunk_verified(
            crate::storage::index_db::ChunkRegistration {
                hash: raw.storage_hash,
                size_stored: blob.len() as u64,
                on_disk_path: path.clone(),
            },
            {
                let b = blocks.clone();
                move |p: &str| b.path_exists(p)
            },
        )
        .await?;
    for attempt in 0..2u32 {
        if outcome != crate::storage::index_db::RegisterOne::RejectedMissingBlob {
            break;
        }
        metrics::record_register_race(1);
        tracing::warn!(attempt, "C-03: blob lost to a racing GC/purge; rewriting");
        blocks.write(&raw.storage_hash, &blob).await?;
        outcome = index
            .register_chunk_verified(
                crate::storage::index_db::ChunkRegistration {
                    hash: raw.storage_hash,
                    size_stored: blob.len() as u64,
                    on_disk_path: path.clone(),
                },
                {
                    let b = blocks.clone();
                    move |p: &str| b.path_exists(p)
                },
            )
            .await?;
    }
    if outcome == crate::storage::index_db::RegisterOne::RejectedMissingBlob {
        return Err(StorageError::Conflict(
            "chunk blob repeatedly lost to GC during upload; aborting — client may retry"
                .to_string(),
        ));
    }

    // H-01: replication op ships while the chunk's bytes are still local —
    // nothing is deferred to stream end.
    let size_stored = blob.len() as u64;
    replicator
        .enqueue_chunk(ChunkPayload {
            hash: raw.storage_hash,
            plain_hash: raw.plain_hash,
            size_original: raw_size_original,
            size_stored,
            compressed,
            encrypted,
            blob,
        })
        .await?;

    Ok(StoredChunk {
        idx: raw.idx,
        offset: raw.offset,
        plain_hash: raw.plain_hash,
        storage_hash: raw.storage_hash,
        compressed,
        encrypted,
        size_original: raw_size_original,
        size_stored,
        dedup_hit: outcome.is_dedup_hit(),
    })
}
