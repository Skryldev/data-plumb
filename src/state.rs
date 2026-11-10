//! Shared application state: all long-lived services assembled from config.

use std::sync::Arc;
use std::time::Instant;

use crate::cache::CacheLayer;
use crate::config::AppConfig;
use crate::core::compressor::Compressor;
use crate::core::pipeline::UploadPipeline;
use crate::core::validator::FileValidator;
use crate::error::StorageError;
use crate::gc::GarbageCollector;
use crate::replication::replicator::Replicator;
use crate::security::audit::AuditLog;
use crate::security::auth::AuthService;
use crate::security::encryption::Encryptor;
use crate::security::tenant::TenantStore;
use crate::storage::block_store::BlockStore;
use crate::storage::index_db::IndexDb;
use crate::storage::retrieval::RetrievalEngine;

/// The assembled service. Cloned freely (all fields are Arc).
#[derive(Clone)]
pub struct AppState(pub Arc<Inner>);

pub struct Inner {
    pub cfg: Arc<AppConfig>,
    pub index: Arc<IndexDb>,
    pub blocks: Arc<BlockStore>,
    pub cache: Arc<CacheLayer>,
    pub validator: Arc<FileValidator>,
    pub compressor: Arc<Compressor>,
    pub encryptor: Option<Arc<Encryptor>>,
    pub pipeline: Arc<UploadPipeline>,
    pub retrieval: Arc<RetrievalEngine>,
    pub auth: Arc<AuthService>,
    pub tenants: Arc<TenantStore>,
    pub audit: Arc<AuditLog>,
    pub replicator: Arc<Replicator>,
    pub gc: Arc<GarbageCollector>,
    pub started_at: Instant,
}

impl AppState {
    pub fn cfg(&self) -> &AppConfig {
        &self.0.cfg
    }

    pub fn uptime_secs(&self) -> u64 {
        self.0.started_at.elapsed().as_secs()
    }
}

/// Builds the full service stack (opens the DB, prepares directories, seeds
/// tenants, starts replication + GC).
pub async fn build_state(cfg: Arc<AppConfig>) -> Result<AppState, StorageError> {
    std::fs::create_dir_all(&cfg.storage.root).map_err(StorageError::from)?;
    std::fs::create_dir_all(&cfg.storage.temp_dir).map_err(StorageError::from)?;
    if let Some(parent) = cfg.database.path.parent() {
        std::fs::create_dir_all(parent).map_err(StorageError::from)?;
    }

    let index = Arc::new(IndexDb::open(&cfg.database.path)?);
    let blocks = Arc::new(BlockStore::new(&cfg.storage.root, cfg.storage.shard_depth));
    blocks.verify_writable().await?;

    let cache = Arc::new(CacheLayer::new(&cfg.cache));
    let compressor = Arc::new(Compressor::new(
        cfg.compression.level,
        cfg.compression.min_size,
    ));
    let validator = Arc::new(FileValidator::new(cfg.limits.max_file_size));
    let encryptor = if cfg.encryption.enabled {
        Some(Arc::new(Encryptor::new(cfg.encryption.keys_dir.clone())))
    } else {
        None
    };
    let auth = Arc::new(AuthService::new(&cfg.auth));
    let tenants = Arc::new(TenantStore::new(index.clone()));
    let audit = Arc::new(AuditLog::new(index.clone()));
    let replicator = Replicator::start(&cfg.replication, index.clone())?;

    let pipeline = Arc::new(UploadPipeline::new(
        index.clone(),
        blocks.clone(),
        compressor.clone(),
        encryptor.clone(),
        replicator.clone(),
        audit.clone(),
        &cfg,
    ));
    let retrieval = Arc::new(RetrievalEngine::new(
        index.clone(),
        blocks.clone(),
        cache.clone(),
        compressor.clone(),
        encryptor.clone(),
        replicator.clone(),
        &cfg,
    ));

    tenants
        .seed(
            &cfg.tenants
                .iter()
                .map(|t| crate::security::tenant::TenantSeed {
                    id: t.id,
                    name: t.name.clone(),
                })
                .collect::<Vec<_>>(),
        )
        .await?;

    let gc = Arc::new(GarbageCollector::new(
        index.clone(),
        blocks.clone(),
        replicator.clone(),
        cfg.retention.gc_mode,
        std::time::Duration::from_secs(cfg.retention.grace_period_secs),
        std::time::Duration::from_secs(cfg.retention.orphan_min_age_secs),
    ));
    gc.clone().spawn(std::time::Duration::from_secs(
        cfg.retention.gc_interval_secs.max(1),
    ));

    // Initialize storage gauges from persisted state.
    if let Ok(stats) = index.global_stats().await {
        tracing::info!(
            files = stats.files,
            chunks = stats.chunks,
            bytes_stored = stats.bytes_stored,
            "index recovered"
        );
    }

    Ok(AppState(Arc::new(Inner {
        cfg,
        index,
        blocks,
        cache,
        validator,
        compressor,
        encryptor,
        pipeline,
        retrieval,
        auth,
        tenants,
        audit,
        replicator,
        gc,
        started_at: Instant::now(),
    })))
}
