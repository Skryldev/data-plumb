//! Read path: L1 (plaintext) → L2 (blob) → disk, BLAKE3 verification after
//! disk loads, N-chunk read-ahead prefetch, full and range retrieval.

use bytes::Bytes;
use futures::Stream;
use std::sync::Arc;
use std::time::Duration;
use uuid::Uuid;

use crate::cache::CacheLayer;
use crate::config::AppConfig;
use crate::core::compressor::Compressor;
use crate::core::hasher::verify;
use crate::error::{not_found, StorageError};
use crate::metrics;
use crate::replication::replicator::Replicator;
use crate::security::encryption::Encryptor;
use crate::storage::block_store::BlockStore;
use crate::storage::index_db::IndexDb;
use crate::storage::manifest::{ChunkRef, Manifest};

/// How long a read-ahead task may linger in the background.
const PREFETCH_TIMEOUT: Duration = Duration::from_secs(10);

/// Default read-ahead window (chunks N+1..N+4).
const READ_AHEAD: usize = 4;

#[derive(Clone)]
pub struct RetrievalEngine {
    index: Arc<IndexDb>,
    blocks: Arc<BlockStore>,
    cache: Arc<CacheLayer>,
    compressor: Arc<Compressor>,
    encryptor: Option<Arc<Encryptor>>,
    replicator: Arc<Replicator>,
    read_ahead: usize,
}

impl RetrievalEngine {
    pub fn new(
        index: Arc<IndexDb>,
        blocks: Arc<BlockStore>,
        cache: Arc<CacheLayer>,
        compressor: Arc<Compressor>,
        encryptor: Option<Arc<Encryptor>>,
        replicator: Arc<Replicator>,
        _cfg: &AppConfig,
    ) -> Self {
        Self {
            index,
            blocks,
            cache,
            compressor,
            encryptor,
            replicator,
            read_ahead: READ_AHEAD,
        }
    }

    /// Fresh manifest lookup (L3 cache → redb). Soft-deleted files read as
    /// not found.
    pub async fn manifest(&self, file_id: &Uuid) -> Result<Arc<Manifest>, StorageError> {
        let id = *file_id;
        let index = self.index.clone();
        let m = self
            .cache
            .manifests
            .get_with(&id, move || async move { index.get_manifest(&id).await })
            .await?
            .ok_or_else(|| not_found(format!("file {file_id}")))?;
        if m.is_deleted() {
            return Err(not_found(format!("file {file_id}")));
        }
        Ok(m)
    }

    /// Full-content stream of the file's plaintext bytes.
    pub fn stream_full(&self, m: Arc<Manifest>) -> impl Stream<Item = Result<Bytes, StorageError>> {
        let engine = self.clone();
        let chunks = m.chunks.clone();
        let tenant = m.tenant_id;
        async_stream::try_stream! {
            for (i, cr) in chunks.iter().enumerate() {
                if engine.read_ahead > 0 {
                    let window_end = (i + 1 + engine.read_ahead).min(chunks.len());
                    engine.spawn_prefetch(&tenant, &chunks[i + 1..window_end]);
                }
                let data = engine.load_plain(&tenant, cr).await?;
                yield data;
            }
        }
    }

    /// Byte range `[start, end)` (end exclusive, clamped to file size).
    /// Yields exactly `end - start` bytes.
    pub fn stream_range(
        &self,
        m: Arc<Manifest>,
        start: u64,
        end: u64,
    ) -> impl Stream<Item = Result<Bytes, StorageError>> {
        let engine = self.clone();
        let size = m.size_original;
        let tenant = m.tenant_id;
        let chunks: Vec<ChunkRef> = m.chunks_in_range(start, end).into_iter().cloned().collect();
        async_stream::try_stream! {
            if start >= size {
                Err(StorageError::RangeNotSatisfiable(size))?;
            }
            let end = end.min(size);
            for (i, cr) in chunks.iter().enumerate() {
                if engine.read_ahead > 0 {
                    let window_end = (i + 1 + engine.read_ahead).min(chunks.len());
                    engine.spawn_prefetch(&tenant, &chunks[i + 1..window_end]);
                }
                let data = engine.load_plain(&tenant, cr).await?;
                let from = (start.max(cr.offset) - cr.offset) as usize;
                let to = (end.min(cr.offset + cr.size_original) - cr.offset) as usize;
                yield data.slice(from..to.max(from));
            }
        }
    }

    /// Loads one chunk's plaintext through the cache hierarchy with
    /// read-back verification on the disk path.
    pub async fn load_plain(&self, tenant: &Uuid, cr: &ChunkRef) -> Result<Bytes, StorageError> {
        if let Some(b) = self.cache.chunks.get_plain(&cr.hash).await {
            metrics::record_cache_hit("l1");
            return Ok(b);
        }
        let blob = self
            .cache
            .chunks
            .get_or_load_blob(cr.hash, || {
                let blocks = self.blocks.clone();
                let hash = cr.hash;
                let replicator = self.replicator.clone();
                async move {
                    match blocks.read(&hash).await {
                        Ok(b) => Ok(b),
                        Err(e @ StorageError::NotFound(_)) => {
                            // Replica on-miss repair: pull the blob from the
                            // upstream primary when configured.
                            if replicator.has_upstream() {
                                let blob = replicator.fetch_blob(&hash).await?;
                                blocks.write(&hash, &blob).await?;
                                Ok(blob)
                            } else {
                                Err(e)
                            }
                        }
                        Err(other) => Err(other),
                    }
                }
            })
            .await?;

        let plain = self.decode(tenant, cr, &blob).await?;
        if let Err(e) = verify(&plain, &cr.plain_hash, "chunk") {
            metrics::record_hash_mismatch();
            return Err(e);
        }
        self.cache.chunks.put_plain(cr.hash, plain.clone()).await;
        Ok(plain)
    }

    /// decrypt + decompress according to chunk flags.
    async fn decode(
        &self,
        tenant: &Uuid,
        cr: &ChunkRef,
        blob: &[u8],
    ) -> Result<Bytes, StorageError> {
        let raw = if cr.encrypted {
            match &self.encryptor {
                Some(enc) => Bytes::from(enc.decrypt(tenant, blob)?),
                None => {
                    return Err(StorageError::Encryption(
                        "chunk is encrypted but encryption is disabled on this node".into(),
                    ))
                }
            }
        } else {
            Bytes::copy_from_slice(blob)
        };
        if cr.compressed {
            self.compressor
                .decompress(&raw, true, cr.size_original as usize)
        } else if raw.len() as u64 != cr.size_original {
            Err(StorageError::Corruption(format!(
                "chunk size {} != expected {}",
                raw.len(),
                cr.size_original
            )))
        } else {
            Ok(raw)
        }
    }

    /// Fire-and-forget read-ahead: warms L2 for upcoming chunks.
    fn spawn_prefetch(&self, tenant: &Uuid, upcoming: &[ChunkRef]) {
        for cr in upcoming {
            if self.cache.chunks.peek_blob(&cr.hash) {
                continue;
            }
            let engine = self.clone();
            let tenant = *tenant;
            let cr = cr.clone();
            tokio::spawn(async move {
                let _ = tokio::time::timeout(PREFETCH_TIMEOUT, async {
                    // Only warm the stored blob; decryption stays on the hot path.
                    let _ = engine
                        .cache
                        .chunks
                        .get_or_load_blob(cr.hash, || {
                            let blocks = engine.blocks.clone();
                            let hash = cr.hash;
                            async move { blocks.read(&hash).await }
                        })
                        .await;
                    let _ = tenant; // tenant kept for future per-tenant prefetch policies
                })
                .await;
            });
        }
    }

    /// Retrieval stats hook (cache tier sizes) for /metrics diagnostics.
    pub async fn cache_stats(&self) -> (u64, u64, u64, u64) {
        self.cache.chunks.stats().await
    }
}
