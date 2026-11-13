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

/// M-06 (V6-03 REPAIR): removes stale crash leftovers — `.tmp-*` blob
/// writes, `*.gc-pending` claims, and multipart spool files — older than
/// `min_age`. RECURSIVE: the blob artifacts live in SHARD subdirectories
/// (`root/ab/cd/...`), which the old top-level `read_dir` never visited,
/// so crash loops leaked full-size blob copies forever (V6-03) and the
/// W1-C-03 boot-recovery claim was ineffective. The walk is bounded to
/// `shard_depth + 2` levels (shard dirs + file level) plus the flat
/// temp/quarantine dirs; a missing dir is tolerated (fresh deployments).
async fn boot_sweep_tmp(
    cfg: &AppConfig,
    min_age: std::time::Duration,
) -> Result<u64, StorageError> {
    let cutoff = std::time::SystemTime::now()
        .duration_since(std::time::SystemTime::UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis() as i64
        - min_age.as_millis() as i64;
    // Blob tree: shard-depth-limited recursion, name-filtered (only crash
    // artifacts are ever removed from the CAS — never live blobs).
    let depth = cfg.storage.shard_depth as usize + 2;
    // Runs on the blocking pool: the shard walk is sync fs work.
    let root = cfg.storage.root.clone();
    let temp = cfg.storage.temp_dir.clone();
    // The count crosses the blocking-pool boundary via the JoinHandle —
    // u64 is Copy, so mutating a captured copy would silently drop the
    // count (this exact bug made the metric read 0).
    let removed = tokio::task::spawn_blocking(move || {
        let mut n = 0u64;
        sweep_dir_blocking(&root, cutoff, &mut n, depth, true);
        // Temp spool: flat dir, any stale file goes. NOTE: the quarantine
        // dir is deliberately NOT swept here — `Quarantine::sweep_expired`
        // owns it (with its own metric + retention policy) right after;
        // a second sweep here would race it for the count.
        sweep_dir_blocking(&temp, cutoff, &mut n, 1, false);
        n
    })
    .await
    .unwrap_or(0);
    Ok(removed)
}

/// Recursive sweep helper. `cas_mode=true` restricts removal to crash
/// artifacts by name (`.tmp-*`, `*.gc-pending`) so live blobs are never
/// touched; `cas_mode=false` (temp/quarantine) removes any stale file.
fn sweep_dir_blocking(
    dir: &std::path::Path,
    cutoff: i64,
    removed: &mut u64,
    depth_remaining: usize,
    cas_mode: bool,
) {
    let rd = match std::fs::read_dir(dir) {
        Ok(rd) => rd,
        Err(_) => return,
    };
    for entry in rd.flatten() {
        let path = entry.path();
        let Ok(meta) = entry.metadata() else { continue };
        if meta.is_dir() {
            if depth_remaining > 1 {
                sweep_dir_blocking(&path, cutoff, removed, depth_remaining - 1, cas_mode);
            }
            continue;
        }
        let name = path
            .file_name()
            .map(|n| n.to_string_lossy().to_string())
            .unwrap_or_default();
        // W1-C-03 barrier: claimed blobs (.gc-pending) orphaned by a crash
        // between claim and delete/restore are pure garbage — their row was
        // unreferenced at claim time (the resurrection re-check ran before
        // the claim; a restored blob is renamed back to canonical, never
        // left pending). Boot-time GC-pending files are therefore stale by
        // construction: no upload can hold a live registration without the
        // canonical path existing.
        let stale_by_name = name.starts_with(".tmp-") || name.ends_with("gc-pending");
        let stale_by_age = meta
            .modified()
            .ok()
            .and_then(|t| t.duration_since(std::time::SystemTime::UNIX_EPOCH).ok())
            .map(|d| (d.as_millis() as i64) < cutoff)
            .unwrap_or(false);
        let stale = stale_by_age && (if cas_mode { stale_by_name } else { true });
        if stale && std::fs::remove_file(&path).is_ok() {
            *removed += 1;
        }
    }
}

#[cfg(test)]
mod boot_sweep_tests {
    use super::*;
    use std::time::Duration;

    /// V6-03 gate (planned in Wave 1, never landed): stale crash artifacts
    /// in SHARD subdirectories must be swept at boot; fresh ones survive;
    /// the metric counts exactly the swept files.
    #[tokio::test]
    async fn boot_sweeps_stale_tmp_in_shard_dirs() {
        // Recorder FIRST: the sweep metric is recorded inside build_state.
        crate::metrics::init_recorder();
        let dir = tempfile::tempdir_in(std::env::temp_dir()).unwrap();
        let mut cfg = AppConfig::for_tests();
        cfg = cfg.with_data_root(dir.path());
        // Plant the artifacts BEFORE build_state (the sweep runs inside it).
        let shard = cfg.storage.root.join("ab").join("cd");
        std::fs::create_dir_all(&shard).unwrap();
        let stale_tmp = shard.join(".tmp-stale");
        let stale_pending = shard.join(format!("{}.123.gc-pending", "a".repeat(64)));
        let fresh_tmp = shard.join(".tmp-fresh");
        std::fs::write(&stale_tmp, b"stale").unwrap();
        std::fs::write(&stale_pending, b"claimed-blob").unwrap();
        std::fs::write(&fresh_tmp, b"fresh").unwrap();
        // Age the stale pair past orphan_min_age; leave the fresh one now.
        let old = std::time::SystemTime::now() - Duration::from_secs(3600);
        for p in [&stale_tmp, &stale_pending] {
            let f = std::fs::File::options().append(true).open(p).unwrap();
            f.set_times(
                std::fs::FileTimes::new()
                    .set_accessed(old)
                    .set_modified(old),
            )
            .unwrap();
        }

        // A stale spool file in temp_dir (non-CAS dir: any stale file goes).
        let spool = cfg.storage.temp_dir.join("spool-leftover");
        std::fs::create_dir_all(&cfg.storage.temp_dir).unwrap();
        std::fs::write(&spool, b"spool").unwrap();
        let f = std::fs::File::options().append(true).open(&spool).unwrap();
        f.set_times(
            std::fs::FileTimes::new()
                .set_accessed(old)
                .set_modified(old),
        )
        .unwrap();

        let state = build_state(Arc::new(cfg)).await.unwrap();

        assert!(!stale_tmp.exists(), "stale .tmp in shard dir must be swept");
        assert!(!stale_pending.exists(), "stale .gc-pending must be swept");
        assert!(fresh_tmp.exists(), "fresh .tmp must survive");
        assert!(!spool.exists(), "stale spool file must be swept");
        // Metric (process-global recorder): the series must exist with a
        // positive count — this boot swept >= 3 stale files.
        let render = crate::metrics::render();
        let line = render
            .lines()
            .find(|l| l.starts_with("file_storage_startup_tmp_swept_total"))
            .expect("sweep metric must render");
        let value: f64 = line
            .rsplit(' ')
            .next()
            .and_then(|v| v.parse().ok())
            .unwrap_or(0.0);
        assert!(
            value >= 3.0,
            "this boot swept 3 stale files; metric shows {value}: {line}"
        );
        let _ = state; // keep the server alive for the assertion window
    }

    /// CAS safety: a LIVE blob (canonical <hex>.bin in a shard dir) must
    /// NEVER be swept even when old — only named crash artifacts go.
    #[tokio::test]
    async fn boot_sweep_never_touches_live_blobs() {
        let dir = tempfile::tempdir_in(std::env::temp_dir()).unwrap();
        let mut cfg = AppConfig::for_tests();
        cfg = cfg.with_data_root(dir.path());
        cfg.retention.orphan_min_age_secs = 0;
        let blocks = crate::storage::block_store::BlockStore::new(cfg.storage.root.clone(), 2);
        let h = [7u8; 32];
        blocks.write(&h, b"live-blob").await.unwrap();
        // Backdate the live blob itself.
        let (_, path) = blocks.shard_path(&h);
        let old = std::time::SystemTime::now() - Duration::from_secs(7200);
        let f = std::fs::File::options().append(true).open(&path).unwrap();
        f.set_times(
            std::fs::FileTimes::new()
                .set_accessed(old)
                .set_modified(old),
        )
        .unwrap();

        let _state = build_state(Arc::new(cfg)).await.unwrap();
        assert!(
            path.exists(),
            "live canonical blob must survive the boot sweep (CAS-mode name filter)"
        );
    }
}
