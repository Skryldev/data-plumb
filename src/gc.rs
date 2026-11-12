//! Garbage collection.
//!
//! * `refcount` mode (primary): soft-deleted manifests past the grace period
//!   are purged; their chunks' refcounts drop; zero-refcount blobs are
//!   removed. An orphan sweeper deletes blobs with no metadata older than
//!   `orphan_min_age`.
//! * `reachability` mode (replica): live manifests define the reachable set;
//!   blobs outside it (and old enough) are deleted.

use serde::Serialize;
use std::collections::HashSet;
use std::sync::Arc;
use std::time::Duration;

use crate::config::GcMode;
use crate::error::StorageError;
use crate::metrics;
use crate::replication::replicator::Replicator;
use crate::storage::block_store::BlockStore;
use crate::storage::index_db::{now_ms, IndexDb};

#[derive(Debug, Default, Clone, Serialize)]
pub struct GcReport {
    pub manifests_purged: u64,
    pub chunks_removed: u64,
    pub orphans_removed: u64,
    pub bytes_reclaimed: u64,
}

pub struct GarbageCollector {
    index: Arc<IndexDb>,
    blocks: Arc<BlockStore>,
    replicator: Arc<Replicator>,
    mode: GcMode,
    /// C-02 guard: refcount GC must never run on a replica.
    role: crate::config::NodeRole,
    grace: Duration,
    orphan_min_age: Duration,
    batch_limit: usize,
    /// M-01: in-process mutual exclusion between GC passes and reconcile
    /// (rule 16: never run both at once). Cross-process exclusion is the
    /// flock on `<root>/../run/reconcile.lock`.
    busy: tokio::sync::Mutex<()>,
    data_root: std::path::PathBuf,
}

impl GarbageCollector {
    pub fn new(
        index: Arc<IndexDb>,
        blocks: Arc<BlockStore>,
        replicator: Arc<Replicator>,
        mode: GcMode,
        role: crate::config::NodeRole,
        grace: Duration,
        orphan_min_age: Duration,
    ) -> Self {
        // M-01: the lock file lives next to the storage root (root is the
        // CAS dir, e.g. `<data>/blocks` → `<data>/run/reconcile.lock`).
        let data_root = blocks
            .root()
            .parent()
            .map(|p| p.to_path_buf())
            .unwrap_or_else(|| std::path::PathBuf::from("."));
        Self {
            index,
            blocks,
            replicator,
            mode,
            role,
            grace,
            orphan_min_age,
            batch_limit: 5_000,
            busy: tokio::sync::Mutex::new(()),
            data_root,
        }
    }

    /// Runs one collection pass in the configured mode. M-01: mutually
    /// excluded from reconcile passes (the reconcile holds the same lock).
    pub async fn run_once(&self) -> Result<GcReport, StorageError> {
        let _guard = self.busy.lock().await;
        if self.mode == GcMode::Disabled {
            return Ok(GcReport::default());
        }
        self.run_pass(self.mode).await
    }

    /// Runs one collection pass with an explicit mode override (admin API).
    pub async fn run_once_with(&self, mode: GcMode) -> Result<GcReport, StorageError> {
        let _guard = self.busy.lock().await;
        self.run_pass(mode).await
    }

    /// One collection pass (caller holds `busy`).
    async fn run_pass(&self, mode: GcMode) -> Result<GcReport, StorageError> {
        let mut report = GcReport::default();
        // C-02 guard: refcount semantics only exist on primaries. Replica
        // chunk rows carry refcount 0 by construction, so a refcount sweep
        // on a replica would treat every live chunk as garbage.
        if self.role == crate::config::NodeRole::Replica && mode == GcMode::Refcount {
            tracing::error!(
                "refcount GC requested on a REPLICA (role=replica, gc_mode=refcount);                  refusing — replicas reclaim via reachability GC only"
            );
            return Ok(report);
        }
        match mode {
            GcMode::Refcount => {
                self.purge_soft_deleted(&mut report).await?;
                self.sweep_orphans_vs_index(&mut report).await?;
            }
            GcMode::Reachability => {
                self.reachability_pass(&mut report).await?;
            }
            GcMode::Disabled => {}
        }
        if report.manifests_purged > 0 || report.chunks_removed > 0 || report.orphans_removed > 0 {
            metrics::record_gc(
                report.manifests_purged,
                report.bytes_reclaimed,
                report.orphans_removed,
            );
        }
        Ok(report)
    }

    /// Refcount mode step 1: purge manifests soft-deleted before the cutoff.
    async fn purge_soft_deleted(&self, report: &mut GcReport) -> Result<(), StorageError> {
        let cutoff = now_ms() - self.grace.as_millis() as i64;
        let manifests = self.index.all_manifests().await?;
        let to_purge: Vec<_> = manifests
            .iter()
            .filter(|m| m.deleted_at_ms.map(|d| d < cutoff).unwrap_or(false))
            .take(self.batch_limit)
            .collect();
        for m in to_purge {
            let hashes: Vec<[u8; 32]> = m.chunks.iter().map(|c| c.hash).collect();
            // remove manifest first (usage counters adjusted inside)
            if self.index.remove_manifest(&m.file_id).await?.is_some() {
                // C-03: decr keeps rows at refcount 0; sweep_victims
                // re-checks references, unlinks FIRST, then finalizes the
                // row only when still unreferenced.
                let zeroed = self.index.decr_refcounts(&hashes).await?;
                self.sweep_victims(zeroed, report).await?;
                self.replicator.enqueue_purge(m.file_id).await?;
                report.manifests_purged += 1;
            }
        }
        Ok(())
    }

    /// Refcount mode step 2: blobs on disk with no metadata row.
    async fn sweep_orphans_vs_index(&self, report: &mut GcReport) -> Result<(), StorageError> {
        let known = self.index.all_chunk_hashes().await?;
        let known: HashSet<[u8; 32]> = known.into_iter().collect();
        let on_disk = self.blocks.list_all().await?;
        let min_age = now_ms() - self.orphan_min_age.as_millis() as i64;
        let mut victims: Vec<[u8; 32]> = Vec::new();
        for h in on_disk {
            if known.contains(&h) {
                continue;
            }
            // Skip young files: they may be mid-write (blob before index txn).
            if self.blocks.modified_ms(&h).await?.unwrap_or(i64::MAX) > min_age {
                continue;
            }
            victims.push(h);
            if victims.len() >= self.batch_limit {
                break;
            }
        }
        let removed = self.sweep_victims(victims, report).await? as u64;
        report.orphans_removed += removed;
        Ok(())
    }

    /// Reachability mode (replica): purge expired tombstones, then delete
    /// blobs not referenced by any live manifest.
    async fn reachability_pass(&self, report: &mut GcReport) -> Result<(), StorageError> {
        let cutoff = now_ms() - self.grace.as_millis() as i64;
        let manifests = self.index.all_manifests().await?;

        // 1. tombstone purge
        let mut reachable: HashSet<[u8; 32]> = HashSet::new();
        for m in &manifests {
            if m.deleted_at_ms.map(|d| d < cutoff).unwrap_or(false) {
                if self.index.remove_manifest(&m.file_id).await?.is_some() {
                    report.manifests_purged += 1;
                }
            } else {
                for c in &m.chunks {
                    reachable.insert(c.hash);
                }
            }
        }

        // 2. orphan sweep against the reachable set
        let on_disk = self.blocks.list_all().await?;
        let min_age = now_ms() - self.orphan_min_age.as_millis() as i64;
        let mut victims: Vec<[u8; 32]> = Vec::new();
        for h in on_disk {
            if reachable.contains(&h) {
                continue;
            }
            // Skip young files: they may be mid-write (blob before index txn).
            if self.blocks.modified_ms(&h).await?.unwrap_or(i64::MAX) > min_age {
                continue;
            }
            victims.push(h);
            if victims.len() >= self.batch_limit {
                break;
            }
        }
        let removed = self.sweep_victims(victims, report).await? as u64;
        report.orphans_removed += removed;
        Ok(())
    }

    /// C-03: victim processing with the SAFE two-phase ordering.
    ///
    /// (a) in-transaction re-check: an upload may have re-registered the
    ///     chunk since the victim list was built — skip it;
    /// (b) finalize the row FIRST, inside its own transaction, only while
    ///     still unreferenced;
    /// (c) ATOMIC FS CLAIM: rename the blob aside (`.gc-pending`) — an
    ///     upload's verified registration sees the canonical path vanish
    ///     and self-heals by rewriting instead of committing a dangling
    ///     manifest;
    /// (d) resurrection re-check: an upload that registered between (b)
    ///     and (c) holds a live row again — RESTORE the claimed blob;
    /// (e) only now the deletion is final.
    ///
    /// The Wave-1 form re-checked references once BEFORE the unlink, which
    /// left the (register-between-recheck-and-unlink) interleaving open —
    /// `race_gc_upload_x100` caught it under load as
    /// `regression(W1-C-03)`. Used by refcount purge, reachability sweep
    /// and hard purge alike; row-less orphans fall through (b)/(d) as
    /// no-ops.
    async fn sweep_victims(
        &self,
        victims: Vec<[u8; 32]>,
        report: &mut GcReport,
    ) -> Result<usize, StorageError> {
        let mut removed = 0usize;
        for hash in victims {
            if self.index.chunk_is_referenced(&hash).await? {
                continue; // resurrected by an upload — keep the blob
            }
            let size = self.blocks.size(&hash).await?;
            // (b) finalize the row while still unreferenced (row-less
            // orphans finalize as no-ops).
            let finalized = self.index.finalize_sweep(&hash).await?;
            // (c) atomic claim.
            let claimed = self.blocks.claim_for_gc(&hash).await?;
            // (d) resurrection re-check AFTER the claim.
            if self.index.chunk_is_referenced(&hash).await? {
                if claimed {
                    self.blocks.restore_claimed(&hash).await?;
                }
                continue;
            }
            // (e) deletion is final: no row, blob claimed.
            if claimed {
                self.blocks.delete_claimed(&hash).await?;
                removed += 1;
                if let Some(s) = size {
                    report.bytes_reclaimed += s;
                }
            }
            if finalized {
                report.chunks_removed += 1;
            }
        }
        Ok(removed)
    }

    /// Request-path hard purge of one manifest's chunks (C-03: both the
    /// REST and gRPC purge handlers route through here so the safe sweep
    /// ordering cannot be bypassed).
    pub async fn purge_chunks_now(&self, hashes: Vec<[u8; 32]>) -> Result<GcReport, StorageError> {
        let mut report = GcReport::default();
        // The owning manifest is already removed; drop its refcounts, then
        // sweep the chunks that reached zero (C-03 safe ordering).
        let zeroed = self.index.decr_refcounts(&hashes).await?;
        self.sweep_victims(zeroed, &mut report).await?;
        Ok(report)
    }

    /// Background loop.
    pub fn spawn(self: Arc<Self>, interval: Duration) {
        tokio::spawn(async move {
            let mut ticker = tokio::time::interval(interval.max(Duration::from_secs(1)));
            ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
            ticker.tick().await; // immediate first tick consumed
            loop {
                ticker.tick().await;
                match self.run_once().await {
                    Ok(r)
                        if r.manifests_purged > 0
                            || r.chunks_removed > 0
                            || r.orphans_removed > 0 =>
                    {
                        tracing::info!(?r, "background gc pass");
                    }
                    Ok(_) => {}
                    Err(e) => tracing::error!(error = %e, "background gc pass failed"),
                }
            }
        });
    }

    /// M-01: periodic reconcile loop (rule 16 semantics; every
    /// `retention.reconcile_interval_secs`, 0 disables).
    pub fn spawn_reconcile(self: Arc<Self>, interval: Duration) {
        if interval.is_zero() {
            return;
        }
        tokio::spawn(async move {
            let mut ticker = tokio::time::interval(interval);
            ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
            ticker.tick().await; // immediate first tick consumed
            loop {
                ticker.tick().await;
                match self.reconcile(ReconcileMode::Repair).await {
                    Ok(r) if r.orphan_chunks > 0 || r.tombstoned > 0 || r.reclaimed > 0 => {
                        tracing::warn!(?r, "reconcile pass reclaimed leaked state");
                    }
                    Ok(_) => {}
                    Err(e) => tracing::error!(error = %e, "reconcile pass failed"),
                }
            }
        });
    }

    /// M-01: reconcile pass — the PERMANENT guarantee behind the
    /// transaction-sized C-03 crash window.
    ///
    /// Rule 16: `DryRun` is read-only (report only, no tombstones, no
    /// metric claims); `Repair` tombstones-then-sweeps, and every tombstone
    /// decision is made inside ONE `Durability::Immediate` transaction
    /// that re-walks every manifest (defends against the C-03 race
    /// pattern: a manifest committed after the snapshot keeps its chunks).
    ///
    /// Locking: the in-process `busy` mutex excludes concurrent GC passes,
    /// and a flock on `<data>/run/reconcile.lock` excludes other processes
    /// sharing the data dir (`Unavailable` when held).
    pub async fn reconcile(&self, mode: ReconcileMode) -> Result<ReconcileReport, StorageError> {
        let started = std::time::Instant::now();
        let _guard = self.busy.lock().await;
        let _flock = self.acquire_reconcile_lock().await?;

        let mut report = ReconcileReport::default();

        // 1. Advisory snapshot: every hash any manifest references.
        let mut referenced: std::collections::HashSet<[u8; 32]> = Default::default();
        self.index
            .collect_referenced_hashes(&mut referenced)
            .await?;
        report.referenced_chunks = referenced.len() as u64;

        // 2. Walk the blob directory; classify orphans (blob with no live
        //    manifest reference).
        let on_disk = self.blocks.list_all().await?;
        report.scanned_blobs = on_disk.len() as u64;
        let mut orphan_count = 0u64;
        for h in &on_disk {
            if !referenced.contains(h) {
                orphan_count += 1;
            }
        }
        report.orphan_chunks = orphan_count;

        // 3. Phantom manifests: rows whose blob file is missing (counted
        //    in both modes; repair can re-pull them via replication).
        let known: std::collections::HashSet<[u8; 32]> = on_disk.into_iter().collect();
        let mut phantom = 0u64;
        let mut scanned_rows = 0u64;
        for h in self.index.all_chunk_hashes().await? {
            scanned_rows += 1;
            if !known.contains(&h) {
                phantom += 1;
            }
        }
        report.scanned_index_entries = scanned_rows;
        report.phantom_manifests = phantom;

        // 4. Repair: tombstone (in-txn re-verified) + normal sweep + the
        // C-03-safe victim sweep over the tombstoned rows.
        if matches!(mode, ReconcileMode::Repair) {
            let tombstoned = self
                .index
                .tombstone_unreferenced_chunks(&referenced)
                .await?;
            report.tombstoned = tombstoned.len() as u64;
            let sweep = self.run_pass(self.mode).await?;
            let mut victim_report = GcReport::default();
            self.sweep_victims(tombstoned, &mut victim_report).await?;
            report.reclaimed =
                sweep.orphans_removed + sweep.chunks_removed + victim_report.chunks_removed;
        } else {
            // Dry-run makes no repair claims (rule 16).
            report.tombstoned = 0;
            report.reclaimed = 0;
        }

        report.duration = started.elapsed();
        if matches!(mode, ReconcileMode::Repair) && report.orphan_chunks > 0 {
            metrics::record_reconcile_leaked(report.orphan_chunks);
        }
        tracing::info!(
            ?mode,
            scanned_blobs = report.scanned_blobs,
            orphans = report.orphan_chunks,
            phantoms = report.phantom_manifests,
            tombstoned = report.tombstoned,
            reclaimed = report.reclaimed,
            elapsed_ms = report.duration.as_millis() as u64,
            "reconcile pass complete"
        );
        Ok(report)
    }

    /// Cross-process advisory lock. MSRV-safe (1.80) alternative to
    /// `File::try_lock` (1.89): an exclusive-create lock file carrying the
    /// owner pid + timestamp. A crashed holder's lock is stolen after one
    /// hour (reconcile passes are short; the periodic interval is longer).
    /// Returns `Unavailable` when another process holds a live lock.
    async fn acquire_reconcile_lock(&self) -> Result<ReconcileLockGuard, StorageError> {
        const STALE: std::time::Duration = std::time::Duration::from_secs(3600);
        let run_dir = self.data_root.join("run");
        tokio::fs::create_dir_all(&run_dir)
            .await
            .map_err(StorageError::from)?;
        let path = run_dir.join("reconcile.lock");
        if let Ok(meta) = tokio::fs::metadata(&path).await {
            if let Ok(modified) = meta.modified() {
                if modified.elapsed().map(|age| age > STALE).unwrap_or(false) {
                    tracing::warn!("stealing stale reconcile lock (>1h)");
                    let _ = tokio::fs::remove_file(&path).await;
                }
            }
        }
        match std::fs::OpenOptions::new()
            .create_new(true)
            .write(true)
            .open(&path)
        {
            Ok(mut f) => {
                use std::io::Write as _;
                let _ = writeln!(
                    f,
                    "pid={} ts_ms={}",
                    std::process::id(),
                    crate::storage::index_db::now_ms()
                );
                Ok(ReconcileLockGuard { path })
            }
            Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => Err(
                StorageError::Unavailable("reconcile already in progress (lock held)".into()),
            ),
            Err(e) => Err(StorageError::from(e)),
        }
    }
}

/// M-01: dropped when the reconcile pass ends — removes the lock file.
struct ReconcileLockGuard {
    path: std::path::PathBuf,
}

impl Drop for ReconcileLockGuard {
    fn drop(&mut self) {
        let _ = std::fs::remove_file(&self.path);
    }
}

/// M-01 reconcile mode. `DryRun` never deletes (rule 16).
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize)]
#[serde(rename_all = "snake_case")]
pub enum ReconcileMode {
    DryRun,
    Repair,
}

/// M-01 reconcile report (admin API + structured log).
#[derive(Debug, Default, Clone, serde::Serialize)]
pub struct ReconcileReport {
    pub scanned_blobs: u64,
    pub scanned_index_entries: u64,
    pub referenced_chunks: u64,
    /// Blobs on disk with no live manifest reference.
    pub orphan_chunks: u64,
    /// Index rows whose blob file is missing.
    pub phantom_manifests: u64,
    /// Rows tombstoned to refcount 0 (Repair only).
    pub tombstoned: u64,
    /// Blobs reclaimed by the follow-up sweep (Repair only).
    pub reclaimed: u64,
    pub duration: std::time::Duration,
}
