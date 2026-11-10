//! L3 manifest cache: hot file descriptors, TTL-bounded so soft-deletes
//! propagate quickly.

use moka::future::Cache;
use std::sync::Arc;
use std::time::Duration;
use uuid::Uuid;

use crate::config::CacheConfig;
use crate::error::StorageError;
use crate::storage::manifest::Manifest;

pub struct ManifestCache {
    inner: Cache<Uuid, Arc<Manifest>>,
}

impl ManifestCache {
    pub fn new(cfg: &CacheConfig) -> Self {
        let inner = Cache::builder()
            .max_capacity(cfg.manifest_capacity.max(1))
            .time_to_live(Duration::from_secs(cfg.manifest_ttl_secs.max(1)))
            .build();
        Self { inner }
    }

    /// Cached lookup with loader; loader results (even `None`) are treated
    /// as cache misses so deletes stay visible.
    pub async fn get_with<F, Fut>(
        &self,
        id: &Uuid,
        loader: F,
    ) -> Result<Option<Arc<Manifest>>, StorageError>
    where
        F: FnOnce() -> Fut,
        Fut: std::future::Future<Output = Result<Option<Manifest>, StorageError>>,
    {
        if let Some(m) = self.inner.get(id).await {
            return Ok(Some(m));
        }
        match loader().await? {
            Some(m) => {
                let arc = Arc::new(m);
                self.inner.insert(*id, arc.clone()).await;
                Ok(Some(arc))
            }
            None => Ok(None),
        }
    }

    pub async fn invalidate(&self, id: &Uuid) {
        self.inner.invalidate(id).await;
    }

    pub async fn entry_count(&self) -> u64 {
        self.inner.run_pending_tasks().await;
        self.inner.entry_count()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::storage::manifest::Manifest;
    use uuid::Uuid;

    fn cfg() -> CacheConfig {
        CacheConfig {
            l1_max_bytes: 1,
            l1_ttl_secs: 1,
            l2_max_bytes: 1,
            l2_ttl_secs: 1,
            manifest_capacity: 100,
            manifest_ttl_secs: 60,
        }
    }

    fn manifest(name: &str) -> Manifest {
        Manifest {
            file_id: Uuid::now_v7(),
            name: name.into(),
            tenant_id: Uuid::now_v7(),
            mime: "a/b".into(),
            size_original: 1,
            size_stored: 1,
            original_hash: [0; 32],
            chunks: vec![],
            tags: vec![],
            created_at_ms: 0,
            updated_at_ms: 0,
            deleted_at_ms: None,
            version: 1,
        }
    }

    #[tokio::test]
    async fn caches_hits_and_misses() {
        let c = ManifestCache::new(&cfg());
        let id = Uuid::now_v7();
        let m = manifest("x");
        let mut loads = 0;
        for _ in 0..3 {
            let got = c
                .get_with(&id, || {
                    loads += 1;
                    let m = m.clone();
                    async move { Ok::<_, StorageError>(Some(m)) }
                })
                .await
                .unwrap();
            assert!(got.is_some());
        }
        assert_eq!(loads, 1);

        let missing = Uuid::now_v7();
        for _ in 0..2 {
            let got = c
                .get_with(&missing, || async { Ok::<_, StorageError>(None) })
                .await
                .unwrap();
            assert!(got.is_none());
        }
    }

    #[tokio::test]
    async fn invalidate_forces_reload() {
        let c = ManifestCache::new(&cfg());
        let id = Uuid::now_v7();
        let m = manifest("y");
        c.get_with(&id, || {
            let m = m.clone();
            async move { Ok::<_, StorageError>(Some(m)) }
        })
        .await
        .unwrap();
        c.invalidate(&id).await;
        assert_eq!(c.entry_count().await, 0);
    }
}
