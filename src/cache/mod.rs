//! Multi-tier caching.

pub mod chunk_cache;
pub mod http_cache;
pub mod manifest_cache;

use crate::config::CacheConfig;
use std::sync::Arc;

/// Bundle of all cache tiers, built once at startup.
#[derive(Clone)]
pub struct CacheLayer {
    pub chunks: Arc<chunk_cache::ChunkCache>,
    pub manifests: Arc<manifest_cache::ManifestCache>,
}

impl CacheLayer {
    pub fn new(cfg: &CacheConfig) -> Self {
        Self {
            chunks: Arc::new(chunk_cache::ChunkCache::new(cfg)),
            manifests: Arc::new(manifest_cache::ManifestCache::new(cfg)),
        }
    }
}
