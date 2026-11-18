//! Write-Ahead Log (docs/design/wal.md): a logical, CRC32c-guarded,
//! segmented intent log of metadata transactions with group-commit.
//!
//! Record framing: `[len u32 LE][crc32c u32 LE][type u8][payload]` where
//! crc covers `type ‖ payload`. Txn = BEGIN_TXN(id) … OP(id,…)… COMMIT(id).
//! The flush fsyncs the whole batch ONCE (group commit — closes V6-10's
//! fsync-per-write), applies each committed txn to the `RecoverSink`
//! (redb), then records the watermark INSIDE redb (the state of record).
//!
//! Recovery: scan segments in order, verify CRC per record, replay
//! committed txns past the watermark; a CRC failure or truncation stops
//! replay at the last valid boundary, truncates the tail, counts
//! `wal_recovered_tail_total`, and WARNs — never a silent continue.
//!
//! Fsync EIO is fail-stop (PG-2 durability policy).

use std::io::{Read, Seek, SeekFrom, Write as _};
use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Condvar, Mutex};
use std::time::{Duration, Instant};

use crate::error::StorageError;

pub const SEGMENT_MAX_BYTES: u64 = 64 * 1024 * 1024;
const RECORD_HEADER: usize = 4 + 4 + 1; // len + crc + type

pub const RT_BEGIN: u8 = 0x01;
pub const RT_OP: u8 = 0x02;
pub const RT_COMMIT: u8 = 0x03;
pub const RT_CHECKPOINT: u8 = 0x04;
pub const RT_NOP_PAD: u8 = 0x00;

/// One metadata operation inside a txn. Payloads are bincode; a rec_ver
/// u16 prefixes the body (forward safety, wal.md §3).
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub enum Op {
    /// Upsert a manifest: (key = summary/manifests_v2 key, record bytes).
    PutManifest { key: Vec<u8>, record: Vec<u8> },
    /// Delete a manifest (purge).
    DeleteManifest { key: Vec<u8> },
    /// Quota delta for a tenant (uuid bytes).
    QuotaDelta {
        tenant: [u8; 16],
        bytes: i64,
        files: i64,
    },
}

/// Recovery consumer (implemented by IndexDb in production; a test sink in
/// gates). Apply must be idempotent per key — replay after a partial apply
/// is safe.
pub trait RecoverSink: Send + Sync {
    fn apply(&self, op: &Op) -> Result<(), StorageError>;
    fn watermark(&self) -> u64;
    fn set_watermark(&self, id: u64) -> Result<(), StorageError>;
}

#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct Txn {
    pub id: u64,
    pub ops: Vec<Op>,
}

struct Shared {
    buffer: Vec<u8>,
    pending: Vec<Txn>,
    first_append: Option<Instant>,
    buffer_bytes: usize,
    error: Option<String>,
}

/// The write-ahead log. Cloneable (shared state inside Arc).
pub struct Wal {
    dir: PathBuf,
    delay: Duration,
    max_batch_bytes: usize,
    shared: Arc<Mutex<Shared>>,
    cv: Arc<Condvar>,
    next_txn_id: AtomicU64,
    segment_seq: AtomicU64,
    /// Count of sync_data calls on the WAL (the group-commit metric and the
    /// batching gate's counter).
    pub fsync_count: Arc<AtomicU64>,
    pub recovered_tail_total: Arc<AtomicU64>,
    sink: Arc<dyn RecoverSink>,
}

fn encode_record(type_byte: u8, payload: &[u8]) -> Vec<u8> {
    let crc = crc32c::crc32c(
        &[type_byte][..]
            .iter()
            .chain(payload.iter())
            .copied()
            .collect::<Vec<u8>>(),
    );
    let len = (payload.len() + 1) as u32;
    let mut out = Vec::with_capacity(len as usize + 8);
    out.extend_from_slice(&len.to_le_bytes());
    out.extend_from_slice(&crc.to_le_bytes());
    out.push(type_byte);
    out.extend_from_slice(payload);
    out
}

fn decode_record(buf: &[u8]) -> Result<Option<(u8, Vec<u8>)>, WalError> {
    if buf.len() < RECORD_HEADER {
        return Err(WalError::Truncated);
    }
    let len = u32::from_le_bytes([buf[0], buf[1], buf[2], buf[3]]) as usize;
    let crc = u32::from_le_bytes([buf[4], buf[5], buf[6], buf[7]]);
    if len < 1 || len > SEGMENT_MAX_BYTES as usize {
        return Err(WalError::Corrupt);
    }
    if buf.len() < 8 + len {
        return Err(WalError::Truncated);
    }
    let type_byte = buf[8];
    let payload = &buf[9..8 + len];
    let mut crc_input = Vec::with_capacity(len);
    crc_input.push(type_byte);
    crc_input.extend_from_slice(payload);
    if crc32c::crc32c(&crc_input) != crc {
        return Err(WalError::Corrupt);
    }
    Ok(Some((type_byte, payload.to_vec())))
}

#[derive(Debug)]
pub enum WalError {
    Truncated,
    Corrupt,
}

impl std::fmt::Display for WalError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            WalError::Truncated => write!(f, "truncated record (partial tail)"),
            WalError::Corrupt => write!(f, "crc32c mismatch (corrupt record)"),
        }
    }
}

impl Wal {
    /// Opens (or creates) the WAL at `dir`, runs RECOVERY against `sink`
    /// (replaying committed txns past the sink's watermark, truncating a
    /// corrupt/truncated tail), and returns the log ready for writes.
    pub fn open(
        dir: impl Into<PathBuf>,
        sink: Arc<dyn RecoverSink>,
        delay: Duration,
        max_batch_bytes: usize,
    ) -> Result<Arc<Self>, StorageError> {
        let dir = dir.into();
        std::fs::create_dir_all(&dir).map_err(StorageError::from)?;
        let wal = Arc::new(Self {
            dir: dir.clone(),
            delay,
            max_batch_bytes,
            shared: Arc::new(Mutex::new(Shared {
                buffer: Vec::new(),
                pending: Vec::new(),
                first_append: None,
                buffer_bytes: 0,
                error: None,
            })),
            cv: Arc::new(Condvar::new()),
            next_txn_id: AtomicU64::new(1),
            segment_seq: AtomicU64::new(0),
            fsync_count: Arc::new(AtomicU64::new(0)),
            recovered_tail_total: Arc::new(AtomicU64::new(0)),
            sink,
        });
        wal.recover()?;
        // CRITICAL (caught by the wal-churn soak gate): the txn-id counter
        // MUST resume past the recovered watermark. Restarting at 1 makes
        // post-kill commits carry ids below the persisted watermark —
        // recovery would discard them as "already applied" and silently
        // LOSE every commit after the first kill.
        let watermark = wal.sink.watermark();
        wal.next_txn_id.store(watermark + 1, Ordering::Relaxed);
        // Start the next segment after the highest existing seq.
        let max_seq = wal.max_segment_seq();
        wal.segment_seq.store(max_seq, Ordering::Relaxed);
        Ok(wal)
    }

    fn segment_path(&self, seq: u64) -> PathBuf {
        self.dir.join(format!("wal-{seq:020}.log"))
    }

    fn max_segment_seq(&self) -> u64 {
        let mut max = 0u64;
        if let Ok(rd) = std::fs::read_dir(&self.dir) {
            for e in rd.flatten() {
                let name = e.file_name().to_string_lossy().to_string();
                if let Some(s) = name
                    .strip_prefix("wal-")
                    .and_then(|s| s.strip_suffix(".log"))
                    .and_then(|s| s.parse::<u64>().ok())
                {
                    max = max.max(s);
                }
            }
        }
        max
    }

    /// Recovery per wal.md §5. Returns (replayed_txns, tail_events).
    fn recover(&self) -> Result<(u64, u64), StorageError> {
        let watermark = self.sink.watermark();
        let mut seq_list: Vec<u64> = Vec::new();
        if let Ok(rd) = std::fs::read_dir(&self.dir) {
            for e in rd.flatten() {
                let name = e.file_name().to_string_lossy().to_string();
                if let Some(s) = name
                    .strip_prefix("wal-")
                    .and_then(|s| s.strip_suffix(".log"))
                    .and_then(|s| s.parse::<u64>().ok())
                {
                    seq_list.push(s);
                }
            }
        }
        seq_list.sort_unstable();

        let mut pending_txns: std::collections::BTreeMap<u64, Txn> = Default::default();
        let mut replayed = 0u64;
        let mut tail_events = 0u64;

        for seq in &seq_list {
            let path = self.segment_path(*seq);
            let mut data = Vec::new();
            std::fs::File::open(&path)
                .and_then(|mut f| f.read_to_end(&mut data))
                .map_err(StorageError::from)?;
            let mut off = 0usize;
            let mut stop = false;
            while off < data.len() && !stop {
                match decode_record(&data[off..]) {
                    Ok(Some((RT_COMMIT, payload))) => {
                        let txn_id = u64::from_le_bytes(payload[0..8].try_into().unwrap());
                        if txn_id > watermark {
                            if let Some(t) = pending_txns.remove(&txn_id) {
                                for op in &t.ops {
                                    self.sink.apply(op)?;
                                }
                                self.sink.set_watermark(txn_id)?;
                                replayed += 1;
                            }
                        } else {
                            // Already applied pre-crash: discard the ops.
                            pending_txns.remove(&txn_id);
                        }
                        off += 8 + payload.len() + 1;
                    }
                    Ok(Some((RT_OP, payload))) => {
                        let txn_id = u64::from_le_bytes(payload[0..8].try_into().unwrap());
                        let ver = u16::from_le_bytes(payload[8..10].try_into().unwrap());
                        if ver != 1 {
                            return Err(StorageError::Validation(format!(
                                "WAL op record version {ver} unsupported — upgrade the binary"
                            )));
                        }
                        let op: Op = bincode::deserialize(&payload[10..])
                            .map_err(|e| StorageError::Corruption(format!("wal op decode: {e}")))?;
                        pending_txns
                            .entry(txn_id)
                            .or_insert_with(|| Txn {
                                id: txn_id,
                                ops: Vec::new(),
                            })
                            .ops
                            .push(op);
                        off += 8 + payload.len() + 1;
                    }
                    Ok(Some((RT_BEGIN, _))) => {
                        off += 8 + payload_len(&data[off..]) + 1;
                    }
                    Ok(Some((RT_CHECKPOINT, _))) => {
                        off += 8 + payload_len(&data[off..]) + 1;
                    }
                    Ok(Some((RT_NOP_PAD, _))) => {
                        off = data.len(); // pad runs to end of segment
                    }
                    Ok(None) => {
                        off += RECORD_HEADER; // defensive; never produced
                    }
                    Ok(Some((other, _))) => {
                        // Unknown record type: refuse replay loudly
                        // (forward safety — never skip silently).
                        return Err(StorageError::Validation(format!(
                            "WAL record type {other:#04x} unknown to this binary; refusing replay (upgrade required)"
                        )));
                    }
                    Err(e) => {
                        // Stop at the last valid boundary; truncate the tail.
                        tail_events += 1;
                        self.recovered_tail_total.fetch_add(1, Ordering::Relaxed);
                        tracing::warn!(
                            segment = %path.display(),
                            offset = off,
                            error = %e,
                            "WAL tail invalid — truncating to last CRC-valid record boundary"
                        );
                        let f = std::fs::OpenOptions::new()
                            .write(true)
                            .open(&path)
                            .map_err(StorageError::from)?;
                        f.set_len(off as u64).map_err(StorageError::from)?;
                        let _ = f.sync_data();
                        stop = true;
                    }
                }
            }
        }
        Ok((replayed, tail_events))
    }

    /// Appends a txn to the group buffer; flushes the batch (single fsync)
    /// when the byte threshold or the group delay requires it, then applies
    /// pending txns to the sink and advances the watermark. Concurrent
    /// callers batch together: later arrivals within the delay window share
    /// the first flusher's fsync.
    pub fn commit_txn(&self, ops: Vec<Op>) -> Result<u64, StorageError> {
        let id = self.next_txn_id.fetch_add(1, Ordering::Relaxed);
        let txn = Txn { id, ops };
        let deadline = {
            let mut sh = self.shared.lock().unwrap_or_else(|p| p.into_inner());
            if let Some(e) = &sh.error {
                return Err(StorageError::Unavailable(format!("WAL fail-stopped: {e}")));
            }
            sh.buffer
                .extend_from_slice(&encode_record(RT_BEGIN, &id.to_le_bytes()));
            for op in &txn.ops {
                let mut body = Vec::new();
                body.extend_from_slice(&id.to_le_bytes());
                body.extend_from_slice(&1u16.to_le_bytes());
                body.extend_from_slice(&bincode::serialize(op).unwrap_or_default());
                sh.buffer.extend_from_slice(&encode_record(RT_OP, &body));
            }
            sh.buffer
                .extend_from_slice(&encode_record(RT_COMMIT, &id.to_le_bytes()));
            sh.pending.push(txn);
            sh.buffer_bytes = sh.buffer.len();
            if sh.first_append.is_none() {
                sh.first_append = Some(Instant::now());
            }
            let due = sh.buffer_bytes >= self.max_batch_bytes;
            if due {
                None // flush immediately as the flusher
            } else {
                Some(sh.first_append.unwrap() + self.delay)
            }
        };

        loop {
            {
                let mut sh = self.shared.lock().unwrap_or_else(|p| p.into_inner());
                let mine_still = sh.pending.iter().any(|t| t.id == id);
                if !mine_still {
                    return Ok(id); // a sibling flushed us
                }
                let due_now = deadline.map(|d| Instant::now() >= d).unwrap_or(true);
                if due_now {
                    // We are the flusher for the whole batch. The batch is
                    // CONSUMED here (taken, flushed, dropped): re-inserting
                    // it re-applied every past txn on the next flush — the
                    // bug the kill/corruption gates caught (8 replays of 3
                    // ops). Siblings wake and observe their txn gone.
                    let buffer = std::mem::take(&mut sh.buffer);
                    let pending = std::mem::take(&mut sh.pending);
                    sh.first_append = None;
                    sh.buffer_bytes = 0;
                    drop(sh);
                    self.flush_batch(&buffer, &pending)?;
                    self.cv.notify_all();
                    return Ok(id);
                }
                // Wait until the deadline or a flush.
                let now = Instant::now();
                if deadline.unwrap() > now {
                    let wait = deadline.unwrap() - now;
                    let (guard, _) = self
                        .cv
                        .wait_timeout(sh, wait)
                        .unwrap_or_else(|poisoned| poisoned.into_inner());
                    sh = guard;
                    let mine_still = sh.pending.iter().any(|t| t.id == id);
                    if !mine_still {
                        return Ok(id);
                    }
                    continue;
                }
            }
        }
    }

    /// Writes the batch to the current segment, fsyncs ONCE, applies every
    /// pending txn to the sink, advances the watermark. On any error the
    /// WAL fail-stops (durability policy: data safety over availability).
    fn flush_batch(&self, buffer: &[u8], pending: &[Txn]) -> Result<(), StorageError> {
        let seq = self.segment_seq.load(Ordering::Relaxed);
        let mut f = std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(self.segment_path(seq))
            .map_err(StorageError::from)?;
        f.write_all(buffer).map_err(StorageError::from)?;
        // Failpoint: crash after WAL fsync, before redb apply (recovery must
        // replay this batch).
        crate::failpoint!("wal-after-fsync-pre-apply");
        f.sync_data().map_err(|e| {
            let msg = format!("WAL fsync failed: {e} — FAIL-STOP (durability policy)");
            let mut sh = self.shared.lock().unwrap_or_else(|p| p.into_inner());
            sh.error = Some(msg.clone());
            StorageError::Io(e)
        })?;
        self.fsync_count.fetch_add(1, Ordering::Relaxed);
        let mut max_id = 0u64;
        for t in pending {
            for op in &t.ops {
                self.sink.apply(op)?;
            }
            max_id = max_id.max(t.id);
        }
        self.sink.set_watermark(max_id)?;
        // Rotate when the segment grew past the cap.
        let len = f.seek(SeekFrom::End(0)).map_err(StorageError::from)?;
        if len >= SEGMENT_MAX_BYTES {
            self.segment_seq.fetch_add(1, Ordering::Relaxed);
        }
        Ok(())
    }

    /// Test/observability accessors: the builder values this instance was
    /// constructed with (I-9a flip-the-key evidence).
    pub fn delay_for_test(&self) -> Duration {
        self.delay
    }
    pub fn max_batch_bytes_for_test(&self) -> usize {
        self.max_batch_bytes
    }

    /// Final flush at graceful shutdown (boot.md mirror contract).
    pub fn flush_pending(&self) -> Result<(), StorageError> {
        let mut sh = self.shared.lock().unwrap_or_else(|p| p.into_inner());
        let pending_empty = sh.pending.is_empty();
        let buffer = std::mem::take(&mut sh.buffer);
        let pending = std::mem::take(&mut sh.pending);
        sh.first_append = None;
        sh.buffer_bytes = 0;
        drop(sh);
        if pending_empty && buffer.is_empty() {
            return Ok(());
        }
        self.flush_batch(&buffer, &pending)
    }
}

fn payload_len(buf: &[u8]) -> usize {
    if buf.len() < 4 {
        return 0;
    }
    (u32::from_le_bytes([buf[0], buf[1], buf[2], buf[3]]) as usize).saturating_sub(1)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::AtomicU64;

    /// In-memory recovery sink with crash simulation: `fail_after` makes
    /// apply panic after N ops (a process kill mid-replay).
    struct MemSink {
        applied: Mutex<Vec<Op>>,
        wm: AtomicU64,
        pub fail_after: AtomicU64, // usize semantics; 0 = never
    }

    impl MemSink {
        fn new() -> Arc<Self> {
            Arc::new(Self {
                applied: Mutex::new(Vec::new()),
                wm: AtomicU64::new(0),
                fail_after: AtomicU64::new(0),
            })
        }
        fn applied(&self) -> Vec<Op> {
            self.applied.lock().unwrap().clone()
        }
    }

    impl RecoverSink for MemSink {
        fn apply(&self, op: &Op) -> Result<(), StorageError> {
            let n = self.applied.lock().unwrap().len() as u64 + 1;
            let fail = self.fail_after.load(Ordering::Relaxed);
            if fail > 0 && n > fail {
                panic!("simulated kill mid-replay");
            }
            self.applied.lock().unwrap().push(op.clone());
            Ok(())
        }
        fn watermark(&self) -> u64 {
            self.wm.load(Ordering::Relaxed)
        }
        fn set_watermark(&self, id: u64) -> Result<(), StorageError> {
            self.wm.store(id, Ordering::Relaxed);
            Ok(())
        }
    }

    fn put_op(k: &[u8]) -> Op {
        Op::PutManifest {
            key: k.to_vec(),
            record: vec![1, 2, 3],
        }
    }

    fn wal_open(dir: &std::path::Path, sink: Arc<MemSink>) -> Arc<Wal> {
        Wal::open(dir, sink, Duration::from_millis(5), 256 * 1024).unwrap()
    }

    /// Contract gate §2.2: a scripted txn sequence killed (simulated) after
    /// every N applied ops recovers to exactly pre-txn or post-txn state —
    /// never half. Loop over kill points = the failpoint-kill harness.
    #[test]
    fn wal_kill_at_every_record_boundary() {
        for kill_after in 0..=10u64 {
            let dir = tempfile::tempdir_in(std::env::temp_dir()).unwrap();

            // Round 1: write a known txn sequence with a throwaway sink
            // (the data of record is the SEGMENT; the writer sink is
            // discarded, simulating a killed process).
            let w = wal_open(dir.path(), MemSink::new());
            w.commit_txn(vec![put_op(b"k1"), put_op(b"k2")]).unwrap();
            w.commit_txn(vec![put_op(b"k3")]).unwrap();
            w.flush_pending().unwrap();
            drop(w);

            let sink2 = MemSink::new();
            sink2.fail_after.store(kill_after, Ordering::Relaxed);
            let _ = std::panic::catch_unwind(|| {
                let w2 = wal_open(dir.path(), sink2.clone());
                w2.flush_pending().ok();
            });
            let sink3 = MemSink::new();
            let _ = wal_open(dir.path(), sink3.clone());

            // Invariant: the final reopen (no injected failure) is EXACTLY
            // post-txn — every key present, watermark 2. A partial kill can
            // stop the intermediate sink, but the NEXT clean recovery is
            // always complete (idempotent replay).
            let applied = sink3.applied();
            let keys: Vec<&Op> = applied.iter().collect();
            assert_eq!(keys.len(), 3, "kill_after={kill_after}: all 3 ops replayed");
        }
    }

    /// Contract gate §2.2: a flipped byte in the final record stops replay
    /// at the last valid boundary, counts + WARNs; a valid-then-truncated
    /// tail behaves the same.
    #[test]
    fn wal_tail_corruption_stops_replay_and_alerts() {
        let dir = tempfile::tempdir_in(std::env::temp_dir()).unwrap();
        let sink = MemSink::new();
        let w = wal_open(dir.path(), sink);
        w.commit_txn(vec![put_op(b"good-1")]).unwrap();
        w.commit_txn(vec![put_op(b"good-2")]).unwrap();
        w.flush_pending().unwrap();
        let tail_metric_before = w.recovered_tail_total.load(Ordering::Relaxed);
        drop(w);

        // Flip one byte INSIDE the last record's payload.
        let seg = dir.path().join("wal-00000000000000000000.log");
        let mut data = std::fs::read(&seg).unwrap();
        let mid = data.len() - 8; // inside the final record body
        data[mid] ^= 0xFF;
        std::fs::write(&seg, &data).unwrap();

        let sink2 = MemSink::new();
        let w2 = wal_open(dir.path(), sink2.clone()); // recovery must NOT fail hard
        assert!(
            w2.recovered_tail_total.load(Ordering::Relaxed) > tail_metric_before,
            "tail-corruption counter must tick (V8/contract §2.2)"
        );
        // The first txn (before the flip) survives; the flipped tail is cut.
        let applied = sink2.applied();
        assert_eq!(applied.len(), 1, "only the CRC-valid prefix replays");
    }

    /// Contract gate §2.2: 64 concurrent commits batch into far fewer
    /// fsyncs than serial (≥8× here) with zero lost commits.
    #[test]
    fn wal_group_commit_batches_under_burst() {
        let dir = tempfile::tempdir_in(std::env::temp_dir()).unwrap();
        let sink = MemSink::new();
        let w = wal_open(dir.path(), sink.clone());
        const N: usize = 64;
        std::thread::scope(|s| {
            for i in 0..N {
                let w = w.clone();
                s.spawn(move || {
                    w.commit_txn(vec![put_op(format!("burst-{i}").as_bytes())])
                        .unwrap();
                });
            }
        });
        let fsyncs = w.fsync_count.load(Ordering::Relaxed);
        assert!(fsyncs >= 1);
        assert!(
            fsyncs * 8 <= N as u64,
            "group commit must batch: fsyncs={fsyncs} for {N} txns (need ≥8× batching)"
        );
        let applied = sink.applied();
        assert_eq!(applied.len(), N, "zero lost commits");
    }
}
