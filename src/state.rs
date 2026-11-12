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
    /// H-01: admission control for uploads (both transports).
    pub upload_gate: Arc<crate::core::gate::UploadGate>,
    /// M-05: THE shared per-tenant governor — the unary interceptor AND
    /// the streaming handlers (Upload/Download) take tokens here, so the
    /// streaming bypass is structurally closed.
    pub rate_limiter: crate::grpc::interceptors::rate_limit::RateLimiter,
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
    let compressor = Arc::new(Compressor::with_max_plain(
        cfg.compression.level,
        cfg.compression.min_size,
        cfg.chunking.max as u64,
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
        validator.clone(),
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

    // H-03: an explicit opt-out is loud, never silent.
    if cfg.validation.mode == crate::config::ValidationMode::Off {
        tracing::warn!(
            "validation.mode=off: declared content-type is NOT checked against              magic bytes at upload; safe serving headers still apply"
        );
    }

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

    // H-01: upload admission control (semaphore + byte budget).
    let upload_gate = Arc::new(crate::core::gate::UploadGate::new(
        cfg.limits.max_concurrent_uploads,
        cfg.limits.max_in_flight_bytes,
    ));

    // M-05: one shared per-tenant governor for unary + streaming RPCs.
    let rate_limiter = crate::grpc::interceptors::rate_limit::RateLimiter::from_cfg(
        cfg.rate_limit.enabled,
        cfg.rate_limit.per_second,
        cfg.rate_limit.burst,
    );

    let gc = Arc::new(GarbageCollector::new(
        index.clone(),
        blocks.clone(),
        replicator.clone(),
        cfg.retention.gc_mode,
        cfg.replication.role,
        std::time::Duration::from_secs(cfg.retention.grace_period_secs),
        std::time::Duration::from_secs(cfg.retention.orphan_min_age_secs),
    ));
    gc.clone().spawn(std::time::Duration::from_secs(
        cfg.retention.gc_interval_secs.max(1),
    ));

    // M-01: periodic drift reconciliation (default hourly; 0 disables —
    // the test config opts out and exercises reconcile via its gates).
    gc.clone().spawn_reconcile(std::time::Duration::from_secs(
        cfg.retention.reconcile_interval_secs,
    ));

    // M-06: boot-time sweep of crash leftovers. `.tmp-*` blobs (from
    // interrupted atomic writes) and multipart spool files older than the
    // orphan window are deleted BEFORE listeners bind, so crash loops no
    // longer grow disk monotonically. Fresh files are left alone.
    let boot_swept = boot_sweep_tmp(
        &cfg,
        std::time::Duration::from_secs(cfg.retention.orphan_min_age_secs),
    )
    .await?;
    if boot_swept > 0 {
        tracing::info!(files = boot_swept, "boot sweep removed stale tmp files");
    }
    crate::metrics::record_startup_tmp_swept(boot_swept);

    // M-07: boot-time quarantine sweep — repair-rejected bytes older than
    // the retention window are evicted (ops evidence ages out).
    let quarantine_swept = crate::storage::quarantine::Quarantine::new(
        cfg.storage.quarantine_dir(),
        cfg.storage.quarantine_max_bytes,
    )
    .sweep_expired(std::time::Duration::from_secs(
        cfg.storage.quarantine_retention_secs,
    ))
    .await
    .unwrap_or(0);
    if quarantine_swept > 0 {
        tracing::info!(
            files = quarantine_swept,
            "boot sweep removed expired quarantine files"
        );
    }
    crate::metrics::record_startup_quarantine_swept(quarantine_swept);

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
        upload_gate,
        rate_limiter,
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

/// M-06: removes stale `.tmp-*` blobs and spool files older than `min_age`.
/// Returns the number of files removed. Walk errors on a missing dir are
/// tolerated (fresh deployments have no spool dir yet).
async fn boot_sweep_tmp(
    cfg: &AppConfig,
    min_age: std::time::Duration,
) -> Result<u64, StorageError> {
    use std::time::SystemTime;
    let cutoff = SystemTime::now()
        .duration_since(SystemTime::UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis() as i64
        - min_age.as_millis() as i64;
    let mut removed: u64 = 0;
    async fn sweep_dir(dir: &std::path::Path, cutoff: i64, removed: &mut u64) {
        let mut rd = match tokio::fs::read_dir(dir).await {
            Ok(rd) => rd,
            Err(_) => return,
        };
        while let Ok(Some(entry)) = rd.next_entry().await {
            let path = entry.path();
            if !path.is_dir() {
                let name = path.file_name().map(|n| n.to_string_lossy().to_string());
                // W1-C-03 barrier: claimed blobs (.gc-pending) orphaned by a
                // crash between claim and delete/restore are pure garbage
                // (their row was unreferenced at claim time).
                let stale_by_name = name
                    .map(|n| n.starts_with(".tmp-") || n.ends_with("gc-pending"))
                    .unwrap_or(false);
                let meta = match entry.metadata().await {
                    Ok(m) => m,
                    Err(_) => continue,
                };
                let stale_by_age = meta
                    .modified()
                    .ok()
                    .and_then(|t| t.duration_since(SystemTime::UNIX_EPOCH).ok())
                    .map(|d| (d.as_millis() as i64) < cutoff)
                    .unwrap_or(false);
                if stale_by_age
                    && (stale_by_name || !dir.ends_with("blocks"))
                    && tokio::fs::remove_file(&path).await.is_ok()
                {
                    *removed += 1;
                }
            }
        }
    }
    sweep_dir(&cfg.storage.root, cutoff, &mut removed).await;
    sweep_dir(&cfg.storage.temp_dir, cutoff, &mut removed).await;
    Ok(removed)
}
