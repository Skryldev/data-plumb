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

/// A chunk after compress/encrypt/blob-write.
struct StoredChunk {
    idx: usize,
    offset: u64,
    plain_hash: [u8; 32],
    storage_hash: [u8; 32],
    compressed: bool,
    encrypted: bool,
    size_original: u64,
    size_stored: u64,
    blob: Bytes,
}

pub struct UploadPipeline {
    index: Arc<IndexDb>,
    blocks: Arc<BlockStore>,
    compressor: Arc<Compressor>,
    encryptor: Option<Arc<Encryptor>>,
    replicator: Arc<Replicator>,
    audit: Arc<AuditLog>,
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
        let processed = chunked.map(move |res| {
            let compressor = self.compressor.clone();
            let blocks = self.blocks.clone();
            let encryptor = encryptor.clone();
            let file_hasher = hasher_captured.clone();
            let size_original = size_captured.clone();
            let idx_counter = idx_counter.clone();
            async move {
                let (offset, data) = res?;
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
                store_chunk(raw, &compressor, &encryptor, &blocks, &tenant).await
            }
        });

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

        // Assemble ordered chunk refs.
        let mut chunks: Vec<ChunkRef> = Vec::with_capacity(stored.len());
        let mut register_items: Vec<([u8; 32], u64, String)> = Vec::with_capacity(stored.len());
        let mut payloads: Vec<ChunkPayload> = Vec::with_capacity(stored.len());
        for sc in stored.into_values() {
            let path = self.blocks.relative_path(&sc.storage_hash);
            register_items.push((sc.storage_hash, sc.size_stored, path));
            payloads.push(ChunkPayload {
                hash: sc.storage_hash,
                plain_hash: sc.plain_hash,
                size_original: sc.size_original,
                size_stored: sc.size_stored,
                compressed: sc.compressed,
                encrypted: sc.encrypted,
                blob: sc.blob,
            });
            chunks.push(ChunkRef {
                hash: sc.storage_hash,
                plain_hash: sc.plain_hash,
                offset: sc.offset,
                size_original: sc.size_original,
                size_stored: sc.size_stored,
                compressed: sc.compressed,
                encrypted: sc.encrypted,
            });
        }

        // One atomic transaction for all chunk refcounts.
        let report = if register_items.is_empty() {
            Vec::new()
        } else {
            self.index.register_chunks(register_items).await?
        };
        let dedup_chunks = report.iter().filter(|hit| **hit).count();
        let new_chunks = report.len() - dedup_chunks;
        let dedup_bytes: u64 = report
            .iter()
            .zip(chunks.iter())
            .filter(|(hit, _)| **hit)
            .map(|(_, c)| c.size_stored)
            .sum();

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

        // Replication: chunk blobs first, then the manifest (order preserved
        // by the queue), then a sync-mode barrier when configured.
        for p in &payloads {
            self.replicator.enqueue_chunk(p.clone()).await?;
        }
        self.replicator.enqueue_manifest(manifest.file_id).await?;
        if self.sync_replication {
            self.replicator.wait_flushed().await?;
        }

        metrics::record_chunks_stored(payloads.len() as u64, dedup_chunks as u64);
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
                    payloads.len()
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
async fn store_chunk(
    raw: RawChunk,
    compressor: &Compressor,
    encryptor: &Option<Arc<Encryptor>>,
    blocks: &BlockStore,
    tenant: &Uuid,
) -> Result<StoredChunk, StorageError> {
    let CompressOutcome {
        data: comp_data,
        compressed,
        size_original,
        ..
    } = compressor.compress(&raw.data);
    let (blob, encrypted) = match encryptor {
        Some(enc) => (Bytes::from(enc.encrypt(tenant, &comp_data)?), true),
        None => (comp_data, false),
    };
    // Blob lands on disk before the index transaction that references it;
    // a crash in between leaves a GC-able orphan, never a dangling ref.
    blocks.write(&raw.storage_hash, &blob).await?;
    Ok(StoredChunk {
        idx: raw.idx,
        offset: raw.offset,
        plain_hash: raw.plain_hash,
        storage_hash: raw.storage_hash,
        compressed,
        encrypted,
        size_original,
        size_stored: blob.len() as u64,
        blob,
    })
}
