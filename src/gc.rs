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
    grace: Duration,
    orphan_min_age: Duration,
    batch_limit: usize,
}

impl GarbageCollector {
    pub fn new(
        index: Arc<IndexDb>,
        blocks: Arc<BlockStore>,
        replicator: Arc<Replicator>,
        mode: GcMode,
        grace: Duration,
        orphan_min_age: Duration,
    ) -> Self {
        Self {
            index,
            blocks,
            replicator,
            mode,
            grace,
            orphan_min_age,
            batch_limit: 5_000,
        }
    }

    /// Runs one collection pass in the configured mode.
    pub async fn run_once(&self) -> Result<GcReport, StorageError> {
        let mut report = GcReport::default();
        if self.mode == GcMode::Disabled {
            return Ok(report);
        }
        match self.mode {
            GcMode::Refcount => {
                self.purge_soft_deleted(&mut report).await?;
                self.sweep_orphans_vs_index(&mut report).await?;
            }
            GcMode::Reachability => {
                self.reachability_pass(&mut report).await?;
            }
            GcMode::Disabled => unreachable!(),
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

    /// Runs one collection pass with an explicit mode override (admin API).
    pub async fn run_once_with(&self, mode: GcMode) -> Result<GcReport, StorageError> {
        if mode == self.mode || mode == GcMode::Disabled {
            return self.run_once().await;
        }
        let mut report = GcReport::default();
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
                let zeroed = self.index.decr_refcounts(&hashes).await?;
                for h in zeroed {
                    if let Some(size) = self.blocks.size(&h).await? {
                        report.bytes_reclaimed += size;
                    }
                    self.blocks.delete(&h).await?;
                    report.chunks_removed += 1;
                }
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
        let mut removed = 0u64;
        for h in on_disk {
            if known.contains(&h) {
                continue;
            }
            // Skip young files: they may be mid-write (blob before index txn).
            if self.blocks.modified_ms(&h).await?.unwrap_or(i64::MAX) > min_age {
                continue;
            }
            if let Some(size) = self.blocks.size(&h).await? {
                report.bytes_reclaimed += size;
            }
            self.blocks.delete(&h).await?;
            removed += 1;
            if removed >= self.batch_limit as u64 {
                break;
            }
        }
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
        let mut removed = 0u64;
        for h in on_disk {
            if reachable.contains(&h) {
                continue;
            }
            if self.blocks.modified_ms(&h).await?.unwrap_or(i64::MAX) > min_age {
                continue;
            }
            if let Some(size) = self.blocks.size(&h).await? {
                report.bytes_reclaimed += size;
            }
            self.blocks.delete(&h).await?;
            removed += 1;
            if removed >= self.batch_limit as u64 {
                break;
            }
        }
        report.orphans_removed += removed;
        Ok(())
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
}
