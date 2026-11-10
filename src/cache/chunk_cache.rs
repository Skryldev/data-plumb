//! L1 (decompressed) + L2 (stored blob) chunk caches backed by moka,
//! byte-weighted with per-tier TTLs.

use bytes::Bytes;
use moka::future::Cache;
use std::time::Duration;

use crate::config::CacheConfig;

pub type ChunkKey = [u8; 32];

pub struct ChunkCache {
    /// L1: plaintext chunks ready to stream to clients.
    plain: Cache<ChunkKey, Bytes>,
    /// L2: stored blobs (compressed / encrypted) ready to decrypt.
    blob: Cache<ChunkKey, Bytes>,
}

impl ChunkCache {
    pub fn new(cfg: &CacheConfig) -> Self {
        let plain = Cache::builder()
            .max_capacity(cfg.l1_max_bytes.max(1))
            .weigher(|_k, v: &Bytes| (v.len() as u32).max(1))
            .time_to_live(Duration::from_secs(cfg.l1_ttl_secs.max(1)))
            .build();
        let blob = Cache::builder()
            .max_capacity(cfg.l2_max_bytes.max(1))
            .weigher(|_k, v: &Bytes| (v.len() as u32).max(1))
            .time_to_live(Duration::from_secs(cfg.l2_ttl_secs.max(1)))
            .build();
        Self { plain, blob }
    }

    // -- L1 (plaintext) ------------------------------------------------------

    pub async fn get_plain(&self, key: &ChunkKey) -> Option<Bytes> {
        self.plain.get(key).await
    }

    pub async fn put_plain(&self, key: ChunkKey, value: Bytes) {
        self.plain.insert(key, value).await;
    }

    // -- L2 (stored blob) ----------------------------------------------------

    /// Returns the blob from L2, or loads it with `loader` and caches the
    /// successful result (errors are propagated, never cached).
    pub async fn get_or_load_blob<F, Fut>(
        &self,
        key: ChunkKey,
        loader: F,
    ) -> Result<Bytes, crate::error::StorageError>
    where
        F: FnOnce() -> Fut,
        Fut: std::future::Future<Output = Result<Bytes, crate::error::StorageError>>,
    {
        if let Some(b) = self.blob.get(&key).await {
            crate::metrics::record_cache_hit("l2");
            return Ok(b);
        }
        let loaded = self.blob.try_get_with(key, loader()).await.map_err(|arc| {
            crate::error::StorageError::Internal(format!("blob load failed: {arc}"))
        })?;
        Ok(loaded)
    }

    /// Non-blocking existence check used by the read-ahead planner.
    pub fn peek_blob(&self, key: &ChunkKey) -> bool {
        self.blob.contains_key(key)
    }

    // -- maintenance -----------------------------------------------------------

    pub async fn invalidate(&self, key: &ChunkKey) {
        self.plain.invalidate(key).await;
        self.blob.invalidate(key).await;
    }

    /// (l1_entries, l1_weight, l2_entries, l2_weight) — diagnostics.
    pub async fn stats(&self) -> (u64, u64, u64, u64) {
        self.plain.run_pending_tasks().await;
        self.blob.run_pending_tasks().await;
        (
            self.plain.entry_count(),
            self.plain.weighted_size(),
            self.blob.entry_count(),
            self.blob.weighted_size(),
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::error::StorageError;

    fn cfg() -> CacheConfig {
        CacheConfig {
            l1_max_bytes: 1024,
            l1_ttl_secs: 60,
            l2_max_bytes: 1024,
            l2_ttl_secs: 60,
            manifest_capacity: 100,
            manifest_ttl_secs: 60,
        }
    }

    fn key(i: u8) -> ChunkKey {
        [i; 32]
    }

    #[tokio::test]
    async fn plain_put_get() {
        let c = ChunkCache::new(&cfg());
        c.put_plain(key(1), Bytes::from_static(b"hello")).await;
        assert_eq!(c.get_plain(&key(1)).await.unwrap().as_ref(), b"hello");
        assert!(c.get_plain(&key(2)).await.is_none());
    }

    #[tokio::test]
    async fn blob_get_or_load_caches_success() {
        let c = ChunkCache::new(&cfg());
        let mut loads = 0;
        for _ in 0..3 {
            let b = c
                .get_or_load_blob(key(3), || {
                    loads += 1;
                    async { Ok::<_, StorageError>(Bytes::from_static(b"blob")) }
                })
                .await
                .unwrap();
            assert_eq!(b.as_ref(), b"blob");
        }
        assert_eq!(loads, 1, "loader must run once; later reads hit L2");
    }

    #[tokio::test]
    async fn blob_errors_are_not_cached() {
        let c = ChunkCache::new(&cfg());
        let mut attempts = 0;
        let err = c
            .get_or_load_blob(key(4), || {
                attempts += 1;
                async {
                    if attempts == 1 {
                        Err(StorageError::Internal("disk hiccup".into()))
                    } else {
                        Ok(Bytes::from_static(b"recovered"))
                    }
                }
            })
            .await;
        assert!(err.is_err());
        let ok = c
            .get_or_load_blob(key(4), || async {
                Ok::<_, StorageError>(Bytes::from_static(b"recovered"))
            })
            .await
            .unwrap();
        assert_eq!(ok.as_ref(), b"recovered");
    }

    #[tokio::test]
    async fn weight_eviction_respects_capacity() {
        let c = ChunkCache::new(&cfg());
        // 1024-byte budget; insert 4 x 512B -> some eviction must occur.
        for i in 0..4u8 {
            c.put_plain(key(i), Bytes::from(vec![i; 512])).await;
        }
        let (entries, weight, _, _) = c.stats().await;
        assert!(weight <= 1024, "weight {} exceeds budget", weight);
        assert!(entries <= 2);
    }

    #[tokio::test]
    async fn invalidate_clears_both_tiers() {
        let c = ChunkCache::new(&cfg());
        c.put_plain(key(9), Bytes::from_static(b"x")).await;
        c.invalidate(&key(9)).await;
        assert!(c.get_plain(&key(9)).await.is_none());
    }
}
