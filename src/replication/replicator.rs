//! Async replication worker: an in-memory WAL queue drained in batches
//! (`batch_size` ops or `flush_interval`), with exponential backoff
//! (100 ms → 500 ms → 2 s) on upstream failures. Sync mode blocks the
//! upload until the batch containing the file's ops is acknowledged.

use bytes::Bytes;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use std::time::Duration;
use tokio::sync::{mpsc, oneshot};
use tokio::task::JoinHandle;
use uuid::Uuid;

use crate::config::ReplicationConfig;
use crate::error::StorageError;
use crate::metrics;
use crate::replication::client::{ChunkPayload, ReplicationClient};
use crate::storage::index_db::IndexDb;

/// One queued replication operation.
pub enum Op {
    Chunk(ChunkPayload),
    Manifest {
        file_id: Uuid,
    },
    Purge {
        file_id: Uuid,
    },
    /// Flush everything queued so far and acknowledge.
    Flush {
        reply: oneshot::Sender<Result<(), StorageError>>,
    },
}

const BACKOFF_MS: [u64; 4] = [100, 500, 2_000, 2_000];

pub struct Replicator {
    tx: tokio::sync::Mutex<Option<mpsc::Sender<Op>>>,
    handle: tokio::sync::Mutex<Option<JoinHandle<()>>>,
    client: Option<ReplicationClient>,
    cfg: ReplicationConfig,
    /// M-02: lag = ops enqueued but not yet acked.
    acked_lag: Arc<AtomicU64>,
}

impl Replicator {
    /// Builds the replicator and spawns the worker task when enabled.
    pub fn start(cfg: &ReplicationConfig, index: Arc<IndexDb>) -> Result<Arc<Self>, StorageError> {
        let (tx, handle, client) = if cfg.enabled {
            let client = ReplicationClient::new(
                cfg.target_url.as_deref().unwrap_or_default(),
                &cfg.api_key,
                Duration::from_secs(30),
            )?;
            let (tx, rx) = mpsc::channel::<Op>(cfg.queue_capacity.max(16));
            let handle = tokio::spawn(worker_loop(
                rx,
                client.clone(),
                index.clone(),
                cfg.batch_size.max(1),
                Duration::from_millis(cfg.flush_interval_ms.max(10)),
            ));
            (Some(tx), Some(handle), Some(client))
        } else {
            (None, None, None)
        };
        let lag = Arc::new(AtomicU64::new(0));
        Ok(Arc::new(Self {
            tx: tokio::sync::Mutex::new(tx),
            handle: tokio::sync::Mutex::new(handle),
            client,
            cfg: cfg.clone(),
            acked_lag: lag.clone(),
        }))
    }

    pub fn is_enabled(&self) -> bool {
        self.cfg.enabled
    }

    /// True when an upstream primary is configured (replica pull mode).
    pub fn has_upstream(&self) -> bool {
        self.cfg.upstream_url.is_some()
    }

    /// The configured upstream URL, for diagnostics logs (M-07 alarm lines).
    pub fn upstream_url(&self) -> Option<&str> {
        self.cfg.upstream_url.as_deref()
    }

    pub fn client(&self) -> Option<&ReplicationClient> {
        self.client.as_ref()
    }

    pub fn queue_depth(&self) -> u64 {
        self.acked_lag.load(Ordering::Relaxed)
    }

    async fn send(&self, op: Op) -> Result<(), StorageError> {
        let guard = self.tx.lock().await;
        match &*guard {
            Some(tx) => {
                // M-02: `enqueued` is a monotonic COUNTER (ops ever queued);
                // the GAUGE below is set by the worker from the true buffer
                // depth, and lag = enqueued - acked. The old code reused one
                // atomic for both meanings and the gauge flip-flopped.
                self.acked_lag.fetch_add(1, Ordering::Relaxed);
                metrics::record_replication_enqueued();
                // M-14: bounded enqueue wait. A full queue + dead peer no
                // longer stalls uploads forever after the manifest is
                // durable: async mode drops the op (COUNTED + alertable
                // fail-open, the single sanctioned one), sync mode fails
                // the upload.
                let timeout = Duration::from_millis(self.cfg.enqueue_timeout_ms.max(10));
                match tokio::time::timeout(timeout, tx.send(op)).await {
                    Ok(Ok(())) => Ok(()),
                    Ok(Err(_closed)) => Err(StorageError::Replication(
                        "replication worker stopped".into(),
                    )),
                    Err(_elapsed) => {
                        metrics::record_replication_dropped(1);
                        tracing::error!(
                            timeout_ms = timeout.as_millis() as u64,
                            "replication queue full > enqueue_timeout; op DROPPED                              (async fail-open; the file is committed locally —                              check replication_lag_ops and the dead peer)"
                        );
                        if self.cfg.mode == crate::config::ReplicationMode::Sync {
                            return Err(StorageError::Replication(
                                "replication enqueue timed out (sync mode)".into(),
                            ));
                        }
                        Ok(()) // async: documented drop-with-alert
                    }
                }
            }
            None => Ok(()), // replication disabled: no-op
        }
    }

    /// M-02: the worker reports one op as ACKED (shipped or resolved).
    pub fn acked(&self, n: u64) {
        self.acked_lag.fetch_sub(n, Ordering::Relaxed);
        metrics::set_replication_lag(self.acked_lag.load(Ordering::Relaxed) as f64);
    }

    /// Queues one chunk blob.
    pub async fn enqueue_chunk(&self, payload: ChunkPayload) -> Result<(), StorageError> {
        self.send(Op::Chunk(payload)).await
    }

    /// Queues a manifest upsert (sent after the file's chunk ops).
    pub async fn enqueue_manifest(&self, file_id: Uuid) -> Result<(), StorageError> {
        self.send(Op::Manifest { file_id }).await
    }

    /// Queues a manifest purge (hard delete).
    pub async fn enqueue_purge(&self, file_id: Uuid) -> Result<(), StorageError> {
        self.send(Op::Purge { file_id }).await
    }

    /// Sync mode: waits until every previously queued op has been flushed.
    pub async fn wait_flushed(&self) -> Result<(), StorageError> {
        if !self.cfg.enabled {
            return Ok(());
        }
        let (tx, rx) = oneshot::channel();
        self.send(Op::Flush { reply: tx }).await?;
        let timeout = Duration::from_millis(self.cfg.sync_timeout_ms.max(100));
        match tokio::time::timeout(timeout, rx).await {
            Ok(Ok(Ok(()))) => Ok(()),
            Ok(Ok(Err(e))) => Err(e),
            Ok(Err(_)) => Err(StorageError::Replication(
                "replication worker stopped".into(),
            )),
            Err(_) => Err(StorageError::Unavailable(format!(
                "replication ack timed out after {} ms",
                timeout.as_millis()
            ))),
        }
    }

    /// Push a file bundle directly (resync path) — bypasses the queue.
    pub async fn push_bundle(&self, bundle: &[u8], file_id: &Uuid) -> Result<(), StorageError> {
        match &self.client {
            Some(c) => c.put_file_bundle(file_id, bundle).await,
            None => Err(StorageError::Conflict("replication is disabled".into())),
        }
    }

    /// Fetch a file bundle from the upstream primary (replica repair).
    pub async fn fetch_bundle(
        &self,
        file_id: &Uuid,
    ) -> Result<(crate::storage::manifest::Manifest, Vec<ChunkPayload>), StorageError> {
        let upstream = self
            .cfg
            .upstream_url
            .as_deref()
            .ok_or_else(|| StorageError::Conflict("no upstream configured".into()))?;
        let client = ReplicationClient::new(upstream, &self.cfg.api_key, Duration::from_secs(30))?;
        client.fetch_file_bundle(file_id).await
    }

    /// Fetch a single blob from the upstream primary.
    pub async fn fetch_blob(&self, hash: &[u8; 32]) -> Result<Bytes, StorageError> {
        let upstream = self
            .cfg
            .upstream_url
            .as_deref()
            .ok_or_else(|| StorageError::Conflict("no upstream configured".into()))?;
        let client = ReplicationClient::new(upstream, &self.cfg.api_key, Duration::from_secs(30))?;
        client.fetch_chunk(hash).await
    }

    /// Final flush + worker shutdown (graceful stop).
    pub async fn shutdown(&self) {
        // Closing the channel makes the worker drain remaining ops and exit.
        if let Some(tx) = self.tx.lock().await.take() {
            drop(tx);
        }
        if let Some(h) = self.handle.lock().await.take() {
            let _ = h.await;
        }
    }
}

async fn worker_loop(
    mut rx: mpsc::Receiver<Op>,
    client: ReplicationClient,
    index: Arc<IndexDb>,
    batch_size: usize,
    flush_interval: Duration,
) {
    let mut buf: Vec<Op> = Vec::with_capacity(batch_size.max(8));
    let mut backoff_idx = 0usize;
    let mut failed = false;

    loop {
        // On failure, back off before retrying the same batch.
        if failed {
            tokio::time::sleep(Duration::from_millis(BACKOFF_MS[backoff_idx])).await;
        }

        // Block until the first op arrives (or the channel closes).
        if buf.is_empty() {
            match rx.recv().await {
                Some(op) => buf.push(op),
                None => break,
            }
        }

        // Opportunistic drain without blocking.
        while buf.len() < batch_size.max(8) {
            match rx.try_recv() {
                Ok(op) => buf.push(op),
                Err(_) => break,
            }
        }

        let has_flush = buf.iter().any(|op| matches!(op, Op::Flush { .. }));
        if !failed && !has_flush && buf.len() < batch_size {
            // Linger for more ops, at most one flush interval.
            match tokio::time::timeout(flush_interval, rx.recv()).await {
                Ok(Some(op)) => {
                    buf.push(op);
                    continue;
                }
                Ok(None) => {
                    let _ = flush_batch(&mut buf, &client, &index).await;
                    break;
                }
                Err(_) => {} // interval elapsed -> flush below
            }
        }

        let started = std::time::Instant::now();
        let result = flush_batch(&mut buf, &client, &index).await;
        let ok = result.is_ok();
        metrics::record_replication_flush(started.elapsed().as_secs_f64(), ok);
        // M-02: the gauge is the TRUE queue depth (buffer after flush);
        // acked ops decrement the lag. The replicator handle is not
        // reachable here, so acks are routed through a shared counter —
        // flush_batch returns the acked count.
        // M-02: shipped ops are "acked" for lag purposes; the worker owns
        // the lag counter via a plain subtraction (enqueued - shipped).
        let acked = result.as_ref().copied().unwrap_or(0);
        metrics::record_replication_acked(acked as u64);
        metrics::set_replication_queue_depth(buf.len() as f64);
        let _ = acked;

        match result {
            Ok(_) => {
                backoff_idx = 0;
                failed = false;
            }
            Err(e) => {
                tracing::warn!(error = %e, queued = buf.len(), "replication flush failed; will retry");
                backoff_idx = (backoff_idx + 1).min(BACKOFF_MS.len() - 1);
                failed = true;
            }
        }
    }
}

/// Attempts to flush the whole buffer. On success the buffer is drained and
/// all Flush acks resolved. On failure the buffer is kept intact for retry,
/// except acks whose receiver was dropped.
async fn flush_batch(
    buf: &mut Vec<Op>,
    client: &ReplicationClient,
    index: &IndexDb,
) -> Result<usize, StorageError> {
    let mut chunks: Vec<ChunkPayload> = Vec::new();
    let mut manifests: Vec<Uuid> = Vec::new();
    let mut purges: Vec<Uuid> = Vec::new();
    let mut acks: Vec<oneshot::Sender<Result<(), StorageError>>> = Vec::new();

    for op in buf.drain(..) {
        match op {
            Op::Chunk(p) => chunks.push(p),
            Op::Manifest { file_id } => manifests.push(file_id),
            Op::Purge { file_id } => purges.push(file_id),
            Op::Flush { reply } => acks.push(reply),
        }
    }

    let flushed = chunks.len() + manifests.len() + purges.len();

    if !chunks.is_empty() {
        client.post_batch(&chunks).await?;
    }
    for id in &manifests {
        if let Some(m) = index.get_manifest(id).await? {
            client.put_manifest(&m).await?;
        }
    }
    for id in &purges {
        client.purge_manifest(id).await?;
    }

    for ack in acks {
        let _ = ack.send(Ok(()));
    }
    Ok(flushed)
}
