//! redb-backed ACID metadata index.
//!
//! Tables (all keys/values are raw bytes):
//! * `chunks`    : 32B hash -> bincode(ChunkMeta)
//! * `files`     : 16B file_id -> msgpack(Manifest)
//! * `name_index`: tenant(16B) || 0x00 || name -> 16B file_id
//! * `tag_index` : tenant(16B) || 0x00 || tag || 0x00 || file_id -> empty
//! * `audit`     : uuidv7(16B, time-ordered) -> JSON(AuditEvent)
//! * `tenants`   : 16B tenant_id -> JSON(Tenant)
//!
//! Every write transaction is fsync'd (Durability::Immediate); all blocking
//! redb work runs on tokio's blocking pool. Guards are always scoped before
//! mutating the same table (redb borrows are exclusive).

use redb::{Database, Durability, ReadableTable, TableDefinition};
use serde::{Deserialize, Serialize};
use std::sync::Arc;
use uuid::Uuid;

use crate::error::{dbe, ser, StorageError};
use crate::security::audit::AuditEvent;
use crate::security::tenant::Tenant;
use crate::storage::manifest::{ChunkMeta, Manifest};

const T_CHUNKS: TableDefinition<&[u8], &[u8]> = TableDefinition::new("chunks");
const T_FILES: TableDefinition<&[u8], &[u8]> = TableDefinition::new("files");
const T_NAME: TableDefinition<&[u8], &[u8]> = TableDefinition::new("name_index");
const T_TAG: TableDefinition<&[u8], &[u8]> = TableDefinition::new("tag_index");
const T_AUDIT: TableDefinition<&[u8], &[u8]> = TableDefinition::new("audit");
const T_TENANTS: TableDefinition<&[u8], &[u8]> = TableDefinition::new("tenants");

// -- v2 trees (docs/design/storage-format-v2.md §3) -------------------------
const T_META: TableDefinition<&[u8], &[u8]> = TableDefinition::new("meta");
const T_SUMMARY: TableDefinition<&[u8], &[u8]> = TableDefinition::new("summary");
const T_MTIME_IDX: TableDefinition<&[u8], &[u8]> = TableDefinition::new("index_by_mtime");
const T_NAME_CI: TableDefinition<&[u8], &[u8]> = TableDefinition::new("index_by_name_ci");
const T_QUOTAS: TableDefinition<&[u8], &[u8]> = TableDefinition::new("quotas");

/// Schema version this binary knows how to open (P5: a data dir written by
/// a NEWER binary must refuse to open here).
pub const SCHEMA_VERSION_KNOWN: u16 = 2;
pub const SCHEMA_VERSION_KEY: &[u8] = b"schema/version";

/// Tenant-scoped summary row (v2): the ONLY row list endpoints read
/// (M-03/V6-08: O(page) lists regardless of tenant size).
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct SummaryRow {
    pub size: u64,
    pub chunk_count: u32,
    pub content_type: String,
    pub mtime_ms: i64,
    pub tags_hash: u64,
    pub deleted: bool,
}

/// Quota counters (v2): commit-time re-check source (storage-format-v2 §5).
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct QuotaRow {
    pub committed_bytes: u64,
    pub committed_files: u64,
}

pub fn summary_key(tenant: &Uuid, path: &str) -> Vec<u8> {
    let mut k = tenant.as_bytes().to_vec();
    k.push(0x00);
    k.extend_from_slice(path.as_bytes());
    k
}

/// Cursor = base64url(direction u8 ‖ last_key). Opaque to clients.
pub fn encode_cursor(descending: bool, last_key: &[u8]) -> String {
    use base64::Engine as _;
    let mut raw = Vec::with_capacity(last_key.len() + 1);
    raw.push(if descending { 1 } else { 0 });
    raw.extend_from_slice(last_key);
    base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(raw)
}

pub fn decode_cursor(cursor: &str) -> Result<(bool, Vec<u8>), StorageError> {
    use base64::Engine as _;
    let raw = base64::engine::general_purpose::URL_SAFE_NO_PAD
        .decode(cursor)
        .map_err(|e| StorageError::Validation(format!("invalid cursor: {e}")))?;
    if raw.is_empty() {
        return Err(StorageError::Validation("empty cursor".into()));
    }
    Ok((raw[0] == 1, raw[1..].to_vec()))
}

#[derive(Clone)]
pub struct IndexDb {
    db: Arc<Database>,
}

/// Result of an upload's chunk registration: per-chunk `true` = dedup hit.
pub type UpsertReport = Vec<bool>;

/// One chunk registration request (C-03): everything needed to create or
/// merge a `ChunkMeta` row, plus the path used for the in-transaction blob
/// presence check.
#[derive(Debug, Clone)]
pub struct ChunkRegistration {
    pub hash: [u8; 32],
    pub size_stored: u64,
    pub on_disk_path: String,
}

/// C-03: outcome of verified batch registration.
#[derive(Debug, Default, Clone)]
pub struct RegisterReport {
    /// hashes registered fresh (row created, refcount 1)
    pub fresh: Vec<[u8; 32]>,
    /// hashes that dedup-hit (row existed, refcount incremented in-txn)
    pub dedup_hit: Vec<[u8; 32]>,
    /// hashes whose blob is GONE — no row was written for them
    pub rejected_missing_blob: Vec<[u8; 32]>,
}

/// Outcome of the single-chunk verified registration used by the streaming
/// pipeline (H-01): payload lifetime is one chunk.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RegisterOne {
    Fresh,
    DedupHit,
    /// Blob absent at registration time — no row was written.
    RejectedMissingBlob,
}

impl RegisterOne {
    pub fn is_dedup_hit(&self) -> bool {
        matches!(self, RegisterOne::DedupHit)
    }
}

impl IndexDb {
    pub fn open(path: &std::path::Path) -> Result<Self, StorageError> {
        Self::open_with_policy(path, false)
    }

    /// Opens the store applying the schema-version gate
    /// (storage-format-v2.md §2, P5): a v1 dir is migrated inline ONLY
    /// when `auto_migrate` is true (`migration.auto`); a dir written by a
    /// NEWER binary always refuses with the exact versions in the error.
    pub fn open_with_policy(
        path: &std::path::Path,
        auto_migrate: bool,
    ) -> Result<Self, StorageError> {
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent).map_err(dbe)?;
        }
        let db = if path.exists() {
            Database::open(path).map_err(dbe)?
        } else {
            Database::create(path).map_err(dbe)?
        };
        let idx = Self { db: Arc::new(db) };
        idx.init_tables()?;
        idx.schema_gate(auto_migrate)?;
        Ok(idx)
    }

    /// Reads `schema/version` (absent on a v1 dir => 1) and enforces the
    /// boot matrix. On a fresh dir (no manifests at all) stamps v2.
    pub fn schema_gate(&self, auto_migrate: bool) -> Result<u16, StorageError> {
        let read_txn = self.db.begin_read().map_err(dbe)?;
        let meta = read_txn.open_table(T_META).map_err(dbe)?;
        let version: u16 = match meta.get(SCHEMA_VERSION_KEY).map_err(dbe)? {
            Some(v) if v.value().len() == 2 => u16::from_le_bytes([v.value()[0], v.value()[1]]),
            _ => {
                // Absent version key: a v1 dir if any manifest exists, else
                // a fresh store (stamp immediately).
                let files = read_txn.open_table(T_FILES).map_err(dbe)?;
                if files.iter().map_err(dbe)?.next().is_none() {
                    0u16 // fresh
                } else {
                    1u16
                }
            }
        };
        drop(read_txn);
        match version {
            0 => {
                let txn = self.db.begin_write().map_err(dbe)?;
                {
                    let mut meta = txn.open_table(T_META).map_err(dbe)?;
                    meta.insert(
                        SCHEMA_VERSION_KEY,
                        SCHEMA_VERSION_KNOWN.to_le_bytes().as_slice(),
                    )
                    .map_err(dbe)?;
                }
                txn.commit().map_err(dbe)?;
                Ok(SCHEMA_VERSION_KNOWN)
            }
            1 => {
                if auto_migrate {
                    // Inline auto-migrate runs the same resumable walk as
                    // `xtask migrate` (checkpointed; idempotent), then stamps.
                    crate::storage::migrate::migrate_v1_to_v2(&self.db)?;
                    crate::storage::migrate::verify_v1_v2(&self.db)?;
                    crate::storage::migrate::stamp_v2(&self.db)?;
                    Ok(SCHEMA_VERSION_KNOWN)
                } else {
                    Err(StorageError::Validation(
                        "data dir schema is v1; this binary requires v2. Run `file-storage xtask migrate --commit` (or set migration.auto=true to migrate on open). Refusing to boot a v1 dir without an explicit choice.".to_string(),
                    ))
                }
            }
            v if v > SCHEMA_VERSION_KNOWN => Err(StorageError::Validation(format!(
                "data dir schema is v{v}; this binary supports up to v{SCHEMA_VERSION_KNOWN} — upgrade the binary (old code must NEVER open newer dirs)"
            ))),
            v => Ok(v),
        }
    }

    fn init_tables(&self) -> Result<(), StorageError> {
        let db = self.db.clone();
        let txn = db.begin_write().map_err(dbe)?;
        {
            txn.open_table(T_CHUNKS).map_err(dbe)?;
            txn.open_table(T_FILES).map_err(dbe)?;
            txn.open_table(T_NAME).map_err(dbe)?;
            txn.open_table(T_TAG).map_err(dbe)?;
            txn.open_table(T_AUDIT).map_err(dbe)?;
            txn.open_table(T_TENANTS).map_err(dbe)?;
            txn.open_table(T_META).map_err(dbe)?;
            txn.open_table(T_SUMMARY).map_err(dbe)?;
            txn.open_table(T_MTIME_IDX).map_err(dbe)?;
            txn.open_table(T_NAME_CI).map_err(dbe)?;
            txn.open_table(T_QUOTAS).map_err(dbe)?;
        }
        txn.commit().map_err(dbe)?;
        Ok(())
    }

    // -- helpers ------------------------------------------------------------

    async fn write<T, F>(&self, f: F) -> Result<T, StorageError>
    where
        T: Send + 'static,
        F: FnOnce(&redb::WriteTransaction) -> Result<T, StorageError> + Send + 'static,
    {
        let db = self.db.clone();
        tokio::task::spawn_blocking(move || {
            let mut txn = db.begin_write().map_err(dbe)?;
            txn.set_durability(Durability::Immediate);
            let out = f(&txn)?;
            txn.commit().map_err(dbe)?;
            Ok(out)
        })
        .await
        .map_err(internal_join)?
    }

    async fn read<T, F>(&self, f: F) -> Result<T, StorageError>
    where
        T: Send + 'static,
        F: FnOnce(&redb::ReadTransaction) -> Result<T, StorageError> + Send + 'static,
    {
        let db = self.db.clone();
        tokio::task::spawn_blocking(move || {
            let txn = db.begin_read().map_err(dbe)?;
            f(&txn)
        })
        .await
        .map_err(internal_join)?
    }

    // -- keys ---------------------------------------------------------------

    fn name_key(tenant: &Uuid, name: &str) -> Vec<u8> {
        let mut v = tenant.as_bytes().to_vec();
        v.push(0x00);
        v.extend_from_slice(name.as_bytes());
        v
    }

    fn tag_key(tenant: &Uuid, tag: &str, file_id: &Uuid) -> Vec<u8> {
        let mut v = tenant.as_bytes().to_vec();
        v.push(0x00);
        v.extend_from_slice(tag.as_bytes());
        v.push(0x00);
        v.extend_from_slice(file_id.as_bytes());
        v
    }

    fn tag_prefix_len(tag_len: usize) -> usize {
        16 + 1 + tag_len + 1 + 16
    }

    /// End bound for a byte-prefix scan (`None` = unbounded).
    fn prefix_end(prefix: &[u8]) -> Option<Vec<u8>> {
        let mut end = prefix.to_vec();
        loop {
            let last = end.last_mut()?;
            if *last == u8::MAX {
                end.pop();
            } else {
                *last += 1;
                return Some(end);
            }
        }
    }

    // -- chunks -------------------------------------------------------------

    pub async fn get_chunk_meta(&self, hash: &[u8; 32]) -> Result<Option<ChunkMeta>, StorageError> {
        let h = *hash;
        self.read(move |txn| {
            let table = txn.open_table(T_CHUNKS).map_err(dbe)?;
            match table.get(h.as_slice()).map_err(dbe)? {
                Some(g) => {
                    let bytes: &[u8] = g.value();
                    bincode::deserialize::<ChunkMeta>(bytes)
                        .map(Some)
                        .map_err(ser)
                }
                None => Ok(None),
            }
        })
        .await
    }

    /// Single-chunk upsert with refcount increment. Returns `true` when the
    /// chunk already existed (dedup hit).
    pub async fn upsert_chunk(
        &self,
        hash: &[u8; 32],
        meta_if_new: ChunkMeta,
    ) -> Result<bool, StorageError> {
        let h = *hash;
        self.write(move |txn| {
            let mut table = txn.open_table(T_CHUNKS).map_err(dbe)?;
            let existing: Option<ChunkMeta> = match table.get(h.as_slice()).map_err(dbe)? {
                Some(g) => {
                    let bytes: &[u8] = g.value();
                    Some(bincode::deserialize(bytes).map_err(ser)?)
                }
                None => None,
            };
            match existing {
                Some(mut meta) => {
                    meta.refcount += 1;
                    let enc = bincode::serialize(&meta).map_err(ser)?;
                    table.insert(h.as_slice(), enc.as_slice()).map_err(dbe)?;
                    Ok(true)
                }
                None => {
                    let enc = bincode::serialize(&meta_if_new).map_err(ser)?;
                    table.insert(h.as_slice(), enc.as_slice()).map_err(dbe)?;
                    Ok(false)
                }
            }
        })
        .await
    }

    /// C-03: batched registration with in-transaction blob presence
    /// verification. ONE write txn (Durability::Immediate preserved), one
    /// pass: for each reg, if the meta row exists the blob is verified and
    /// the refcount incremented (dedup hit); otherwise the blob is verified
    /// and a row with refcount 1 is created. Missing blob => NO row; the
    /// hash lands in `RegisterReport::rejected_missing_blob` so the caller
    /// (upload pipeline) can self-heal by rewriting the blob.
    ///
    /// The race window collapses from "the entire upload stream" to "one
    /// syscall inside the registration transaction".
    pub async fn register_chunks_at_commit(
        &self,
        regs: Vec<ChunkRegistration>,
        blob_exists: impl Fn(&str) -> bool + Send + Sync + 'static,
    ) -> Result<RegisterReport, StorageError> {
        let now = now_ms();
        self.write(move |txn| {
            let mut table = txn.open_table(T_CHUNKS).map_err(dbe)?;
            let mut report = RegisterReport::default();
            for reg in &regs {
                let present = blob_exists(&reg.on_disk_path);
                let existing: Option<ChunkMeta> =
                    match table.get(reg.hash.as_slice()).map_err(dbe)? {
                        Some(g) => {
                            let bytes: &[u8] = g.value();
                            Some(bincode::deserialize(bytes).map_err(ser)?)
                        }
                        None => None,
                    };
                if !present {
                    // No row is written for a blob that vanished; the
                    // uploader re-writes (CAS is idempotent) and retries.
                    report.rejected_missing_blob.push(reg.hash);
                    continue;
                }
                match existing {
                    Some(mut meta) => {
                        meta.refcount += 1;
                        let enc = bincode::serialize(&meta).map_err(ser)?;
                        table
                            .insert(reg.hash.as_slice(), enc.as_slice())
                            .map_err(dbe)?;
                        report.dedup_hit.push(reg.hash);
                    }
                    None => {
                        let meta = ChunkMeta {
                            refcount: 1,
                            blob_size: reg.size_stored,
                            created_at_ms: now,
                            on_disk_path: reg.on_disk_path.clone(),
                        };
                        let enc = bincode::serialize(&meta).map_err(ser)?;
                        table
                            .insert(reg.hash.as_slice(), enc.as_slice())
                            .map_err(dbe)?;
                        report.fresh.push(reg.hash);
                    }
                }
            }
            Ok(report)
        })
        .await
    }

    /// C-03 guard (a): true when the chunk row exists with refcount > 0 —
    /// an upload re-registered this chunk since the victim list was built.
    pub async fn chunk_is_referenced(&self, hash: &[u8; 32]) -> Result<bool, StorageError> {
        let h = *hash;
        self.read(move |txn| {
            let table = txn.open_table(T_CHUNKS).map_err(dbe)?;
            match table.get(h.as_slice()).map_err(dbe)? {
                Some(g) => {
                    let meta: ChunkMeta = bincode::deserialize(g.value()).map_err(ser)?;
                    Ok(meta.refcount > 0)
                }
                None => Ok(false),
            }
        })
        .await
    }

    /// Synchronous `chunk_is_referenced` for the blocking boot sweep
    /// (V8 C-4): runs on the blocking pool, so a blocking read txn is fine.
    pub fn chunk_is_referenced_sync(&self, hash: &[u8; 32]) -> Result<bool, StorageError> {
        let txn = self.db.begin_read().map_err(dbe)?;
        let table = txn.open_table(T_CHUNKS).map_err(dbe)?;
        match table.get(hash.as_slice()).map_err(dbe)? {
            Some(g) => {
                let meta: ChunkMeta = bincode::deserialize(g.value()).map_err(ser)?;
                Ok(meta.refcount > 0)
            }
            None => Ok(false),
        }
    }

    /// C-03 guard (c): removes the chunk row ONLY if it is still
    /// unreferenced, inside one write transaction. Returns true when the
    /// row was removed.
    pub async fn finalize_sweep(&self, hash: &[u8; 32]) -> Result<bool, StorageError> {
        let h = *hash;
        self.write(move |txn| {
            let mut table = txn.open_table(T_CHUNKS).map_err(dbe)?;
            let existing: Option<ChunkMeta> = match table.get(h.as_slice()).map_err(dbe)? {
                Some(g) => {
                    let bytes: &[u8] = g.value();
                    Some(bincode::deserialize(bytes).map_err(ser)?)
                }
                None => None,
            };
            match existing {
                Some(meta) if meta.refcount == 0 => {
                    table.remove(h.as_slice()).map_err(dbe)?;
                    Ok(true)
                }
                _ => Ok(false),
            }
        })
        .await
    }

    /// M-01: streams every manifest's chunk hashes into `into`. Batched
    /// reads keep peak memory at the referenced-set size.
    pub async fn collect_referenced_hashes(
        &self,
        into: &mut std::collections::HashSet<[u8; 32]>,
    ) -> Result<(), StorageError> {
        let manifests = self.all_manifests().await?;
        for m in manifests {
            for c in &m.chunks {
                into.insert(c.hash);
            }
        }
        Ok(())
    }

    /// M-01: tombstones chunk rows with `refcount > 0` but ZERO manifest
    /// references. Rule 16: the authoritative decision is made INSIDE one
    /// `Durability::Immediate` write transaction that re-walks every
    /// manifest — a manifest committed between the caller's
    /// `collect_referenced_hashes` snapshot and this call makes its chunks
    /// referenced again, and the in-txn re-check sees it (defends against
    /// the C-03 race pattern). `referenced` is the caller's advisory
    /// snapshot used for reporting; tombstoning never deletes blobs (the
    /// normal sweep reclaims refcount-0 rows afterwards).
    ///
    /// Returns the tombstoned hashes (count = `len()`); the caller feeds
    /// them into the C-03-safe victim sweep for reclamation.
    pub async fn tombstone_unreferenced_chunks(
        &self,
        referenced: &std::collections::HashSet<[u8; 32]>,
    ) -> Result<Vec<[u8; 32]>, StorageError> {
        let advisory = referenced.len();
        let _ = advisory;
        self.write(move |txn| {
            // In-txn authoritative referenced set.
            let mut live: std::collections::HashSet<[u8; 32]> = std::collections::HashSet::new();
            {
                let files = txn.open_table(T_FILES).map_err(dbe)?;
                for row in files.iter().map_err(dbe)? {
                    let (_, v) = row.map_err(dbe)?;
                    let bytes: &[u8] = v.value();
                    let m: Manifest = rmp_serde::from_slice(bytes).map_err(ser)?;
                    for c in &m.chunks {
                        live.insert(c.hash);
                    }
                }
            }
            let mut tombstoned: Vec<[u8; 32]> = Vec::new();
            let mut rows: Vec<(Vec<u8>, ChunkMeta)> = Vec::new();
            {
                let chunks = txn.open_table(T_CHUNKS).map_err(dbe)?;
                for row in chunks.iter().map_err(dbe)? {
                    let (k, v) = row.map_err(dbe)?;
                    let key = k.value().to_vec();
                    let meta: ChunkMeta = bincode::deserialize(v.value()).map_err(ser)?;
                    if meta.refcount > 0 {
                        rows.push((key, meta));
                    }
                }
            }
            {
                let mut chunks = txn.open_table(T_CHUNKS).map_err(dbe)?;
                for (key, mut meta) in rows {
                    let mut arr = [0u8; 32];
                    if key.len() == 32 {
                        arr.copy_from_slice(&key);
                    } else {
                        continue;
                    }
                    if live.contains(&arr) {
                        continue; // referenced by a manifest — untouched
                    }
                    meta.refcount = 0;
                    let enc = bincode::serialize(&meta).map_err(ser)?;
                    chunks.insert(key.as_slice(), enc.as_slice()).map_err(dbe)?;
                    tombstoned.push(arr);
                }
            }
            Ok(tombstoned)
        })
        .await
    }

    /// Replica-side insert: creates metadata only when absent, never bumps
    /// refcounts (replicas use reachability GC).
    ///
    /// NOTE(audit C-02): the row's refcount STAYS 0 by construction.
    /// Refcount GC must never run on replicas — purge propagation is
    /// metadata-only and reclamation is reachability-based.
    pub async fn upsert_chunk_replica(
        &self,
        hash: &[u8; 32],
        blob_size: u64,
        path: &str,
    ) -> Result<bool, StorageError> {
        let h = *hash;
        let path = path.to_string();
        let now = now_ms();
        self.write(move |txn| {
            let mut table = txn.open_table(T_CHUNKS).map_err(dbe)?;
            if table.get(h.as_slice()).map_err(dbe)?.is_some() {
                return Ok(false);
            }
            let meta = ChunkMeta {
                refcount: 0,
                blob_size,
                created_at_ms: now,
                on_disk_path: path,
            };
            let enc = bincode::serialize(&meta).map_err(ser)?;
            table.insert(h.as_slice(), enc.as_slice()).map_err(dbe)?;
            Ok(true)
        })
        .await
    }

    /// Decrements refcounts; returns hashes whose refcount reached zero and
    /// whose metadata was removed (their blobs are now garbage).
    /// Decrements refcounts, keeping rows that reach 0 (C-03): the GC sweep
    /// helper re-checks reference status and unlinks the blob BEFORE the row
    /// is removed via [`finalize_sweep`]. Returns the hashes whose refcount
    /// reached 0 (sweep candidates).
    pub async fn decr_refcounts(&self, hashes: &[[u8; 32]]) -> Result<Vec<[u8; 32]>, StorageError> {
        let hashes = hashes.to_vec();
        self.write(move |txn| {
            let mut table = txn.open_table(T_CHUNKS).map_err(dbe)?;
            let mut zeroed = Vec::new();
            for h in &hashes {
                let existing: Option<ChunkMeta> = match table.get(h.as_slice()).map_err(dbe)? {
                    Some(g) => {
                        let bytes: &[u8] = g.value();
                        Some(bincode::deserialize(bytes).map_err(ser)?)
                    }
                    None => None,
                };
                match existing {
                    Some(mut meta) if meta.refcount > 1 => {
                        meta.refcount -= 1;
                        let enc = bincode::serialize(&meta).map_err(ser)?;
                        table.insert(h.as_slice(), enc.as_slice()).map_err(dbe)?;
                    }
                    Some(mut meta) => {
                        // keep the row at refcount 0; finalize_sweep removes it
                        meta.refcount = 0;
                        let enc = bincode::serialize(&meta).map_err(ser)?;
                        table.insert(h.as_slice(), enc.as_slice()).map_err(dbe)?;
                        zeroed.push(*h);
                    }
                    None => {}
                }
            }
            Ok(zeroed)
        })
        .await
    }

    pub async fn chunk_exists(&self, hash: &[u8; 32]) -> Result<bool, StorageError> {
        let h = *hash;
        self.read(move |txn| {
            let table = txn.open_table(T_CHUNKS).map_err(dbe)?;
            Ok(table.get(h.as_slice()).map_err(dbe)?.is_some())
        })
        .await
    }

    /// All chunk hashes known to the index.
    pub async fn all_chunk_hashes(&self) -> Result<Vec<[u8; 32]>, StorageError> {
        self.read(|txn| {
            let table = txn.open_table(T_CHUNKS).map_err(dbe)?;
            let mut out = Vec::new();
            for row in table.iter().map_err(dbe)? {
                let (k, _) = row.map_err(dbe)?;
                let key: &[u8] = k.value();
                if key.len() == 32 {
                    let mut arr = [0u8; 32];
                    arr.copy_from_slice(key);
                    out.push(arr);
                }
            }
            Ok(out)
        })
        .await
    }

    /// (chunk_count, total_blob_bytes) for metrics.
    pub async fn chunk_stats(&self) -> Result<(u64, u64), StorageError> {
        self.read(|txn| {
            let table = txn.open_table(T_CHUNKS).map_err(dbe)?;
            let mut count = 0u64;
            let mut bytes = 0u64;
            for row in table.iter().map_err(dbe)? {
                let (_, v) = row.map_err(dbe)?;
                let bytes_v: &[u8] = v.value();
                let meta: ChunkMeta = bincode::deserialize(bytes_v).map_err(ser)?;
                count += 1;
                bytes += meta.blob_size;
            }
            Ok((count, bytes))
        })
        .await
    }

    // -- files --------------------------------------------------------------

    /// I-2: quota-aware manifest write. The re-check lives INSIDE the same
    /// redb txn as the manifest insert (storage-format-v2.md §5) — the
    /// admission-gate reserve is a fast-path only; a concurrent writer can
    /// never push committed bytes past the limit because the re-check and
    /// the write are one atomic txn (redb is single-writer, so races
    /// serialize at the txn).
    pub async fn put_manifest_checked(
        &self,
        m: &Manifest,
        max_quota_bytes: Option<u64>,
    ) -> Result<(), StorageError> {
        let key = m.file_id.as_bytes().to_vec();
        let payload = rmp_serde::to_vec(m).map_err(ser)?;
        let name_key = Self::name_key(&m.tenant_id, &m.name);
        let file_id_bytes = m.file_id.as_bytes().to_vec();
        let tag_keys: Vec<Vec<u8>> = m
            .tags
            .iter()
            .map(|t| Self::tag_key(&m.tenant_id, t, &m.file_id))
            .collect();
        let tenant_id = m.tenant_id;
        let stored = m.size_stored;
        self.write(move |txn| {
            {
                let mut files = txn.open_table(T_FILES).map_err(dbe)?;
                files
                    .insert(key.as_slice(), payload.as_slice())
                    .map_err(dbe)?;
            }
            {
                let mut names = txn.open_table(T_NAME).map_err(dbe)?;
                names
                    .insert(name_key.as_slice(), file_id_bytes.as_slice())
                    .map_err(dbe)?;
            }
            {
                let mut tags = txn.open_table(T_TAG).map_err(dbe)?;
                for tk in &tag_keys {
                    tags.insert(tk.as_slice(), b"".as_slice()).map_err(dbe)?;
                }
            }
            // Quota re-check + delta in the SAME txn. The `quotas` row is
            // populated by migration/fold; an absent row is treated as 0.
            if let Some(limit) = max_quota_bytes {
                let mut quotas = txn.open_table(T_QUOTAS).map_err(dbe)?;
                let current: QuotaRow =
                    match quotas.get(tenant_id.as_bytes().as_slice()).map_err(dbe)? {
                        Some(v) => bincode::deserialize(v.value()).map_err(ser)?,
                        None => QuotaRow::default(),
                    };
                if current.committed_bytes + stored > limit {
                    return Err(StorageError::TooLarge(format!(
                        "tenant {} quota exceeded: committed {} + {} > limit {}",
                        tenant_id, current.committed_bytes, stored, limit
                    )));
                }
                // Keep the v2 quota row accurate in the SAME txn: the next
                // re-check sees this commit's bytes (race-correct).
                quotas
                    .insert(
                        tenant_id.as_bytes().as_slice(),
                        bincode::serialize(&QuotaRow {
                            committed_bytes: current.committed_bytes + stored,
                            committed_files: current.committed_files + 1,
                        })
                        .map_err(ser)?
                        .as_slice(),
                    )
                    .map_err(dbe)?;
            }
            adjust_tenant_usage(txn, &tenant_id, 1, stored as i64)?;
            crate::failpoint!("quota-commit-after-check");
            Ok(())
        })
        .await
    }

    pub async fn put_manifest(&self, m: &Manifest) -> Result<(), StorageError> {
        let key = m.file_id.as_bytes().to_vec();
        let payload = rmp_serde::to_vec(m).map_err(ser)?;
        let name_key = Self::name_key(&m.tenant_id, &m.name);
        let file_id_bytes = m.file_id.as_bytes().to_vec();
        let tag_keys: Vec<Vec<u8>> = m
            .tags
            .iter()
            .map(|t| Self::tag_key(&m.tenant_id, t, &m.file_id))
            .collect();
        let stored = m.size_stored;
        let tenant_id = m.tenant_id;
        self.write(move |txn| {
            {
                let mut files = txn.open_table(T_FILES).map_err(dbe)?;
                files
                    .insert(key.as_slice(), payload.as_slice())
                    .map_err(dbe)?;
            }
            {
                let mut names = txn.open_table(T_NAME).map_err(dbe)?;
                names
                    .insert(name_key.as_slice(), file_id_bytes.as_slice())
                    .map_err(dbe)?;
            }
            {
                let mut tags = txn.open_table(T_TAG).map_err(dbe)?;
                for tk in &tag_keys {
                    tags.insert(tk.as_slice(), b"".as_slice()).map_err(dbe)?;
                }
            }
            adjust_tenant_usage(txn, &tenant_id, 1, stored as i64)?;
            Ok(())
        })
        .await
    }

    pub async fn get_manifest(&self, id: &Uuid) -> Result<Option<Manifest>, StorageError> {
        let key = id.as_bytes().to_vec();
        self.read(move |txn| {
            let table = txn.open_table(T_FILES).map_err(dbe)?;
            match table.get(key.as_slice()).map_err(dbe)? {
                Some(g) => {
                    let bytes: &[u8] = g.value();
                    rmp_serde::from_slice::<Manifest>(bytes)
                        .map(Some)
                        .map_err(ser)
                }
                None => Ok(None),
            }
        })
        .await
    }

    /// Soft-deletes a manifest (sets `deleted_at_ms`, bumps version).
    /// Returns the previous manifest when it existed and was live.
    pub async fn soft_delete_manifest(&self, id: &Uuid) -> Result<Option<Manifest>, StorageError> {
        let key = id.as_bytes().to_vec();
        let now = now_ms();
        self.write(move |txn| {
            let mut files = txn.open_table(T_FILES).map_err(dbe)?;
            let existing: Option<Manifest> = match files.get(key.as_slice()).map_err(dbe)? {
                Some(g) => {
                    let bytes: &[u8] = g.value();
                    Some(rmp_serde::from_slice(bytes).map_err(ser)?)
                }
                None => None,
            };
            let Some(existing) = existing else {
                return Ok(None);
            };
            if existing.deleted_at_ms.is_some() {
                return Ok(None);
            }
            let mut m = existing;
            m.deleted_at_ms = Some(now);
            m.updated_at_ms = now;
            m.version += 1;
            let payload = rmp_serde::to_vec(&m).map_err(ser)?;
            files
                .insert(key.as_slice(), payload.as_slice())
                .map_err(dbe)?;
            Ok(Some(m))
        })
        .await
    }

    /// Removes a manifest plus its name/tag index rows. Returns the deleted
    /// manifest (caller handles chunk refcounts and tenant usage).
    pub async fn remove_manifest(&self, id: &Uuid) -> Result<Option<Manifest>, StorageError> {
        let key = id.as_bytes().to_vec();
        self.write(move |txn| {
            let existing: Option<Manifest> = {
                let files = txn.open_table(T_FILES).map_err(dbe)?;
                let guard = files.get(key.as_slice()).map_err(dbe)?;
                match guard {
                    Some(g) => {
                        let bytes: &[u8] = g.value();
                        Some(rmp_serde::from_slice(bytes).map_err(ser)?)
                    }
                    None => None,
                }
            };
            let Some(existing) = existing else {
                return Ok(None);
            };
            {
                let mut files = txn.open_table(T_FILES).map_err(dbe)?;
                files.remove(key.as_slice()).map_err(dbe)?;
            }
            {
                let nk = Self::name_key(&existing.tenant_id, &existing.name);
                let mut names = txn.open_table(T_NAME).map_err(dbe)?;
                let points_at_file: Option<Vec<u8>> = match names.get(nk.as_slice()).map_err(dbe)? {
                    Some(g) => {
                        let v: &[u8] = g.value();
                        Some(v.to_vec())
                    }
                    None => None,
                };
                if points_at_file.as_deref() == Some(existing.file_id.as_bytes()) {
                    names.remove(nk.as_slice()).map_err(dbe)?;
                }
            }
            {
                let mut tags = txn.open_table(T_TAG).map_err(dbe)?;
                for t in &existing.tags {
                    let tk = Self::tag_key(&existing.tenant_id, t, &existing.file_id);
                    tags.remove(tk.as_slice()).map_err(dbe)?;
                }
            }
            adjust_tenant_usage(txn, &existing.tenant_id, -1, -(existing.size_stored as i64))?;
            Ok(Some(existing))
        })
        .await
    }

    /// Lists a tenant's manifests via the name index (paginated).
    pub async fn list_by_tenant(
        &self,
        tenant: &Uuid,
        include_deleted: bool,
        limit: usize,
        offset: usize,
    ) -> Result<Vec<Manifest>, StorageError> {
        let start = tenant.as_bytes().to_vec();
        let end = Self::prefix_end(&start);
        self.read(move |txn| {
            let names = txn.open_table(T_NAME).map_err(dbe)?;
            let files = txn.open_table(T_FILES).map_err(dbe)?;
            let mut out = Vec::new();
            let mut skipped = 0usize;
            let iter = match end {
                Some(e) => names.range(start.as_slice()..e.as_slice()).map_err(dbe)?,
                None => names.range(start.as_slice()..).map_err(dbe)?,
            };
            for row in iter {
                let (_, v) = row.map_err(dbe)?;
                let id_bytes: &[u8] = v.value();
                if id_bytes.len() != 16 {
                    continue;
                }
                let mut arr = [0u8; 16];
                arr.copy_from_slice(id_bytes);
                let id = Uuid::from_bytes(arr);
                let g = match files.get(id.as_bytes().as_slice()).map_err(dbe)? {
                    Some(g) => g,
                    None => continue,
                };
                let bytes: &[u8] = g.value();
                let m: Manifest = rmp_serde::from_slice(bytes).map_err(ser)?;
                if !include_deleted && m.is_deleted() {
                    continue;
                }
                if skipped < offset {
                    skipped += 1;
                    continue;
                }
                out.push(m);
                if out.len() >= limit {
                    break;
                }
            }
            Ok(out)
        })
        .await
    }

    /// M-04d: THE one list/count implementation both transports call.
    ///
    /// Tag filter takes precedence (exact match, unpaginated result set);
    /// name filter is an exact match; unfiltered lists are paginated with
    /// an exact tenant-wide count. REST and gRPC previously ran two
    /// different code paths here (the audit's M-04d drift); they now share
    /// this method, so their `total` values cannot diverge.
    ///
    /// Returns `(page_manifests, total)` where `total` is the exact number
    /// of matching files for the request's filter — independent of the
    /// page window.
    pub async fn list_filtered(
        &self,
        tenant: &Uuid,
        tag: Option<&str>,
        name: Option<&str>,
        include_deleted: bool,
        limit: usize,
        offset: usize,
    ) -> Result<(Vec<Manifest>, u64), StorageError> {
        if let Some(tag) = tag.map(str::trim).filter(|t| !t.is_empty()) {
            let ids = self.list_ids_by_tag(tenant, tag).await?;
            let mut out = Vec::new();
            for id in ids {
                if let Some(m) = self.get_manifest(&id).await? {
                    if include_deleted || !m.is_deleted() {
                        out.push(m);
                    }
                }
            }
            let total = out.len() as u64;
            Ok((out, total))
        } else if let Some(name) = name.map(str::trim).filter(|n| !n.is_empty()) {
            let out: Vec<Manifest> = match self.get_by_name(tenant, name).await? {
                Some(m) if include_deleted || !m.is_deleted() => vec![m],
                _ => Vec::new(),
            };
            let total = out.len() as u64;
            Ok((out, total))
        } else {
            let list = self
                .list_by_tenant(tenant, include_deleted, limit, offset)
                .await?;
            let total = self.count_by_tenant(tenant, include_deleted).await?;
            Ok((list, total))
        }
    }

    /// Counts a tenant's files (used for `total` in list responses).
    pub async fn count_by_tenant(
        &self,
        tenant: &Uuid,
        include_deleted: bool,
    ) -> Result<u64, StorageError> {
        let start = tenant.as_bytes().to_vec();
        let end = Self::prefix_end(&start);
        self.read(move |txn| {
            let names = txn.open_table(T_NAME).map_err(dbe)?;
            let files = txn.open_table(T_FILES).map_err(dbe)?;
            let mut count = 0u64;
            let iter = match end {
                Some(e) => names.range(start.as_slice()..e.as_slice()).map_err(dbe)?,
                None => names.range(start.as_slice()..).map_err(dbe)?,
            };
            for row in iter {
                let (_, v) = row.map_err(dbe)?;
                let id_bytes: &[u8] = v.value();
                if id_bytes.len() != 16 {
                    continue;
                }
                let mut arr = [0u8; 16];
                arr.copy_from_slice(id_bytes);
                let id = Uuid::from_bytes(arr);
                if let Some(g) = files.get(id.as_bytes().as_slice()).map_err(dbe)? {
                    let bytes: &[u8] = g.value();
                    let m: Manifest = rmp_serde::from_slice(bytes).map_err(ser)?;
                    if include_deleted || !m.is_deleted() {
                        count += 1;
                    }
                }
            }
            Ok(count)
        })
        .await
    }

    pub async fn get_by_name(
        &self,
        tenant: &Uuid,
        name: &str,
    ) -> Result<Option<Manifest>, StorageError> {
        let key = Self::name_key(tenant, name);
        self.read(move |txn| {
            let names = txn.open_table(T_NAME).map_err(dbe)?;
            let files = txn.open_table(T_FILES).map_err(dbe)?;
            let g = match names.get(key.as_slice()).map_err(dbe)? {
                Some(g) => g,
                None => return Ok(None),
            };
            let id_bytes: &[u8] = g.value();
            if id_bytes.len() != 16 {
                return Ok(None);
            }
            let mut arr = [0u8; 16];
            arr.copy_from_slice(id_bytes);
            let id = Uuid::from_bytes(arr);
            match files.get(id.as_bytes().as_slice()).map_err(dbe)? {
                Some(g) => {
                    let bytes: &[u8] = g.value();
                    rmp_serde::from_slice::<Manifest>(bytes)
                        .map(Some)
                        .map_err(ser)
                }
                None => Ok(None),
            }
        })
        .await
    }

    /// Lists file ids for a (tenant, tag) pair.
    pub async fn list_ids_by_tag(
        &self,
        tenant: &Uuid,
        tag: &str,
    ) -> Result<Vec<Uuid>, StorageError> {
        let tenant = *tenant;
        let tag = tag.to_string();
        let prefix = Self::tag_key(&tenant, &tag, &Uuid::nil());
        // prefix = tenant||0||tag||0 ; trim the trailing 16 zero bytes of the nil id
        let prefix_len = prefix.len() - 16;
        let prefix = prefix[..prefix_len].to_vec();
        let end = Self::prefix_end(&prefix);
        let expect_key_len = Self::tag_prefix_len(tag.len());
        self.read(move |txn| {
            let tags = txn.open_table(T_TAG).map_err(dbe)?;
            let mut out = Vec::new();
            let iter = match end {
                Some(e) => tags.range(prefix.as_slice()..e.as_slice()).map_err(dbe)?,
                None => tags.range(prefix.as_slice()..).map_err(dbe)?,
            };
            for row in iter {
                let (k, _) = row.map_err(dbe)?;
                let key: &[u8] = k.value();
                if key.len() != expect_key_len {
                    continue;
                }
                let tail = &key[key.len() - 16..];
                let mut arr = [0u8; 16];
                arr.copy_from_slice(tail);
                out.push(Uuid::from_bytes(arr));
            }
            Ok(out)
        })
        .await
    }

    /// All manifests — GC reachability scans and stats.
    pub async fn all_manifests(&self) -> Result<Vec<Manifest>, StorageError> {
        self.read(|txn| {
            let files = txn.open_table(T_FILES).map_err(dbe)?;
            let mut out = Vec::new();
            for row in files.iter().map_err(dbe)? {
                let (_, v) = row.map_err(dbe)?;
                let bytes: &[u8] = v.value();
                let m: Manifest = rmp_serde::from_slice(bytes).map_err(ser)?;
                out.push(m);
            }
            Ok(out)
        })
        .await
    }

    // -- tenants ------------------------------------------------------------

    pub async fn upsert_tenant(&self, t: &Tenant) -> Result<(), StorageError> {
        let key = t.id.as_bytes().to_vec();
        let payload = serde_json::to_vec(t).map_err(ser)?;
        self.write(move |txn| {
            let mut table = txn.open_table(T_TENANTS).map_err(dbe)?;
            table
                .insert(key.as_slice(), payload.as_slice())
                .map_err(dbe)?;
            Ok(())
        })
        .await
    }

    pub async fn get_tenant(&self, id: &Uuid) -> Result<Option<Tenant>, StorageError> {
        let key = id.as_bytes().to_vec();
        self.read(move |txn| {
            let table = txn.open_table(T_TENANTS).map_err(dbe)?;
            match table.get(key.as_slice()).map_err(dbe)? {
                Some(g) => {
                    let bytes: &[u8] = g.value();
                    serde_json::from_slice::<Tenant>(bytes)
                        .map(Some)
                        .map_err(ser)
                }
                None => Ok(None),
            }
        })
        .await
    }

    pub async fn list_tenants(&self) -> Result<Vec<Tenant>, StorageError> {
        self.read(|txn| {
            let table = txn.open_table(T_TENANTS).map_err(dbe)?;
            let mut out = Vec::new();
            for row in table.iter().map_err(dbe)? {
                let (_, v) = row.map_err(dbe)?;
                let bytes: &[u8] = v.value();
                let t: Tenant = serde_json::from_slice(bytes).map_err(ser)?;
                out.push(t);
            }
            out.sort_by(|a, b| a.name.cmp(&b.name));
            Ok(out)
        })
        .await
    }

    /// Adjusts a tenant's usage counters inside an existing write transaction.
    pub fn adjust_usage_in_txn(
        txn: &redb::WriteTransaction,
        tenant: &Uuid,
        files: i64,
        bytes: i64,
    ) -> Result<(), StorageError> {
        adjust_tenant_usage(txn, tenant, files, bytes)
    }

    // -- audit --------------------------------------------------------------

    pub async fn append_audit(&self, ev: &AuditEvent) -> Result<(), StorageError> {
        let key = ev.id.as_bytes().to_vec();
        let payload = serde_json::to_vec(ev).map_err(ser)?;
        self.write(move |txn| {
            let mut table = txn.open_table(T_AUDIT).map_err(dbe)?;
            table
                .insert(key.as_slice(), payload.as_slice())
                .map_err(dbe)?;
            Ok(())
        })
        .await
    }

    /// Most recent audit events (time-ordered descending).
    pub async fn list_audit(&self, limit: usize) -> Result<Vec<AuditEvent>, StorageError> {
        self.read(move |txn| {
            let table = txn.open_table(T_AUDIT).map_err(dbe)?;
            let mut out = Vec::new();
            for row in table.iter().map_err(dbe)?.rev() {
                let (_, v) = row.map_err(dbe)?;
                let bytes: &[u8] = v.value();
                let ev: AuditEvent = serde_json::from_slice(bytes).map_err(ser)?;
                out.push(ev);
                if out.len() >= limit {
                    break;
                }
            }
            Ok(out)
        })
        .await
    }

    /// The WAL recovery sink (wal.md §5): applies a committed txn's ops to
    /// redb and maintains the watermark INSIDE redb (the state of record).
    /// Idempotent per key: replay after a partial apply is safe.
    pub fn apply_wal_op(&self, op: &crate::storage::wal::Op) -> Result<(), StorageError> {
        use crate::storage::wal::Op as WOp;
        match op {
            WOp::PutManifest { key, record } => {
                let txn = self.db.begin_write().map_err(dbe)?;
                {
                    let mut files = txn.open_table(T_FILES).map_err(dbe)?;
                    files
                        .insert(key.as_slice(), record.as_slice())
                        .map_err(dbe)?;
                }
                txn.commit().map_err(dbe)?;
                Ok(())
            }
            WOp::DeleteManifest { key } => {
                let txn = self.db.begin_write().map_err(dbe)?;
                {
                    let mut files = txn.open_table(T_FILES).map_err(dbe)?;
                    files.remove(key.as_slice()).map_err(dbe)?;
                }
                txn.commit().map_err(dbe)?;
                Ok(())
            }
            WOp::QuotaDelta {
                tenant,
                bytes,
                files: files_delta,
            } => {
                let txn = self.db.begin_write().map_err(dbe)?;
                {
                    let mut quotas = txn.open_table(T_QUOTAS).map_err(dbe)?;
                    let current: QuotaRow = match quotas.get(tenant.as_slice()).map_err(dbe)? {
                        Some(v) => bincode::deserialize(v.value()).map_err(ser)?,
                        None => QuotaRow::default(),
                    };
                    let updated = QuotaRow {
                        committed_bytes: (current.committed_bytes as i64 + bytes).max(0) as u64,
                        committed_files: (current.committed_files as i64 + files_delta).max(0)
                            as u64,
                    };
                    quotas
                        .insert(
                            tenant.as_slice(),
                            bincode::serialize(&updated).map_err(ser)?.as_slice(),
                        )
                        .map_err(dbe)?;
                }
                txn.commit().map_err(dbe)?;
                Ok(())
            }
        }
    }

    /// Readiness probe: a read transaction must succeed.
    pub async fn ping(&self) -> Result<(), StorageError> {
        self.read(|txn| {
            txn.open_table(T_TENANTS).map_err(dbe)?;
            Ok(())
        })
        .await
    }

    /// Global stats for `/metrics` and ops tooling.
    pub async fn global_stats(&self) -> Result<FileStats, StorageError> {
        let manifests = self.all_manifests().await?;
        let (chunks, bytes_stored) = self.chunk_stats().await?;
        let live = manifests.iter().filter(|m| !m.is_deleted());
        Ok(FileStats {
            files: live.clone().count() as u64,
            chunks,
            bytes_logical: live.map(|m| m.size_original).sum(),
            bytes_stored,
        })
    }
}

impl crate::storage::wal::RecoverSink for IndexDb {
    fn apply(&self, op: &crate::storage::wal::Op) -> Result<(), StorageError> {
        self.apply_wal_op(op)
    }
    fn watermark(&self) -> u64 {
        // The watermark row lives in meta (key `wal/watermark`); absent => 0.
        (|| -> Result<u64, StorageError> {
            let txn = self.db.begin_read().map_err(dbe)?;
            let meta = txn.open_table(T_META).map_err(dbe)?;
            match meta.get(b"wal/watermark".as_slice()).map_err(dbe)? {
                Some(v) if v.value().len() == 8 => {
                    let mut b = [0u8; 8];
                    b.copy_from_slice(v.value());
                    Ok(u64::from_le_bytes(b))
                }
                _ => Ok(0),
            }
        })()
        .unwrap_or(0)
    }
    fn set_watermark(&self, id: u64) -> Result<(), StorageError> {
        let txn = self.db.begin_write().map_err(dbe)?;
        {
            let mut meta = txn.open_table(T_META).map_err(dbe)?;
            meta.insert(b"wal/watermark".as_slice(), id.to_le_bytes().as_slice())
                .map_err(dbe)?;
        }
        txn.commit().map_err(dbe)?;
        Ok(())
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct FileStats {
    pub files: u64,
    pub chunks: u64,
    pub bytes_logical: u64,
    pub bytes_stored: u64,
}

fn internal_join(e: tokio::task::JoinError) -> StorageError {
    StorageError::Internal(format!("blocking task failed: {e}"))
}

pub fn now_ms() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::SystemTime::UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis() as i64
}

fn adjust_tenant_usage(
    txn: &redb::WriteTransaction,
    tenant: &Uuid,
    files: i64,
    bytes: i64,
) -> Result<(), StorageError> {
    let mut table = txn.open_table(T_TENANTS).map_err(dbe)?;
    let existing: Option<Tenant> = match table.get(tenant.as_bytes().as_slice()).map_err(dbe)? {
        Some(g) => {
            let bytes_v: &[u8] = g.value();
            Some(serde_json::from_slice(bytes_v).map_err(ser)?)
        }
        None => None,
    };
    let mut t = existing.unwrap_or(Tenant {
        id: *tenant,
        name: format!("tenant-{}", tenant.simple()),
        created_at_ms: now_ms(),
        file_count: 0,
        bytes_used: 0,
    });
    t.file_count = (t.file_count as i64 + files).max(0) as u64;
    t.bytes_used = (t.bytes_used as i64 + bytes).max(0) as u64;
    let payload = serde_json::to_vec(&t).map_err(ser)?;
    table
        .insert(tenant.as_bytes().as_slice(), payload.as_slice())
        .map_err(dbe)?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::storage::manifest::ChunkRef;

    fn open() -> (IndexDb, tempfile::TempDir) {
        let dir = tempfile::tempdir().unwrap();
        (IndexDb::open(&dir.path().join("index.redb")).unwrap(), dir)
    }

    fn manifest(name: &str, tenant: Uuid, stored: u64) -> Manifest {
        Manifest {
            file_id: Uuid::now_v7(),
            name: name.into(),
            tenant_id: tenant,
            mime: "application/octet-stream".into(),
            size_original: stored * 2,
            size_stored: stored,
            original_hash: [1u8; 32],
            chunks: vec![ChunkRef {
                hash: [2u8; 32],
                plain_hash: [2u8; 32],
                offset: 0,
                size_original: stored * 2,
                size_stored: stored,
                compressed: true,
                encrypted: false,
            }],
            tags: vec!["q1".into()],
            created_at_ms: now_ms(),
            updated_at_ms: now_ms(),
            deleted_at_ms: None,
            version: 1,
        }
    }

    #[tokio::test]
    async fn chunk_upsert_and_refcounts() {
        let (idx, _dir) = open();
        let h = [1u8; 32];
        let reg = ChunkRegistration {
            hash: h,
            size_stored: 42,
            on_disk_path: "p/x.bin".into(),
        };
        // fresh registration
        let one = idx
            .register_chunks_at_commit(vec![reg.clone()], |_| true)
            .await
            .unwrap();
        assert_eq!(one.fresh.len(), 1, "fresh registration");
        // dedup hit
        let two = idx
            .register_chunks_at_commit(vec![reg], |_| true)
            .await
            .unwrap();
        assert_eq!(two.dedup_hit.len(), 1, "dedup hit");
        let meta = idx.get_chunk_meta(&h).await.unwrap().unwrap();
        assert_eq!(meta.refcount, 2);
        // first decr: refcount 1
        assert!(idx.decr_refcounts(&[h]).await.unwrap().is_empty());
        // second decr: reaches 0 — C-03 keeps the ROW (sweep finalizes it)
        assert_eq!(idx.decr_refcounts(&[h]).await.unwrap(), vec![h]);
        let meta = idx.get_chunk_meta(&h).await.unwrap().unwrap();
        assert_eq!(meta.refcount, 0);
        assert!(!idx.chunk_is_referenced(&h).await.unwrap());
        assert!(idx.finalize_sweep(&h).await.unwrap());
        assert!(idx.get_chunk_meta(&h).await.unwrap().is_none());
    }

    #[tokio::test]
    async fn verified_register_reports_dedup_and_rejects_missing() {
        let (idx, _dir) = open();
        let regs: Vec<ChunkRegistration> = (0..4u8)
            .map(|i| ChunkRegistration {
                hash: [i; 32],
                size_stored: 100 + i as u64,
                on_disk_path: format!("p/{i}.bin"),
            })
            .collect();
        let r1 = idx
            .register_chunks_at_commit(regs.clone(), |_| true)
            .await
            .unwrap();
        assert_eq!(r1.fresh.len(), 4);
        assert!(r1.dedup_hit.is_empty());
        let r2 = idx
            .register_chunks_at_commit(regs.clone(), |_| true)
            .await
            .unwrap();
        assert_eq!(r2.dedup_hit.len(), 4);
        let (count, bytes) = idx.chunk_stats().await.unwrap();
        assert_eq!(count, 4);
        assert_eq!(bytes, 100 + 101 + 102 + 103);

        // Missing blob: no row written, hash reported rejected.
        let missing = ChunkRegistration {
            hash: [9; 32],
            size_stored: 10,
            on_disk_path: "p/missing.bin".into(),
        };
        let r3 = idx
            .register_chunks_at_commit(vec![missing], |_| false)
            .await
            .unwrap();
        assert_eq!(r3.rejected_missing_blob, vec![[9u8; 32]]);
        assert!(idx.get_chunk_meta(&[9; 32]).await.unwrap().is_none());
    }

    #[tokio::test]
    async fn fused_register_rejects_missing_blob() {
        let (idx, _dir) = open();
        let reg = ChunkRegistration {
            hash: [8; 32],
            size_stored: 10,
            on_disk_path: "p/gone.bin".into(),
        };
        let out = idx
            .register_chunks_at_commit(vec![reg.clone()], |_| false)
            .await
            .unwrap();
        assert_eq!(out.rejected_missing_blob.len(), 1);
        assert!(idx.get_chunk_meta(&reg.hash).await.unwrap().is_none());
        // refcounts untouched
        assert!(!idx.chunk_is_referenced(&reg.hash).await.unwrap());
    }

    #[tokio::test]
    async fn finalize_sweep_only_removes_unreferenced() {
        let (idx, _dir) = open();
        let h = [7u8; 32];
        let reg = ChunkRegistration {
            hash: h,
            size_stored: 10,
            on_disk_path: "p/x.bin".into(),
        };
        idx.register_chunks_at_commit(vec![reg.clone()], |_| true)
            .await
            .unwrap();
        // referenced: finalize refuses
        assert!(idx.chunk_is_referenced(&h).await.unwrap());
        assert!(!idx.finalize_sweep(&h).await.unwrap());
        assert!(idx.get_chunk_meta(&h).await.unwrap().is_some());
        // decr to zero (row stays), finalize now removes
        let zeroed = idx.decr_refcounts(&[h]).await.unwrap();
        assert_eq!(zeroed, vec![h]);
        assert!(!idx.chunk_is_referenced(&h).await.unwrap());
        assert!(idx.finalize_sweep(&h).await.unwrap());
        assert!(idx.get_chunk_meta(&h).await.unwrap().is_none());
    }

    #[tokio::test]
    async fn manifest_crud_and_indexes() {
        let (idx, _dir) = open();
        let tenant = Uuid::now_v7();
        let m = manifest("report.pdf", tenant, 500);
        idx.upsert_tenant(&Tenant {
            id: tenant,
            name: "t".into(),
            created_at_ms: 0,
            file_count: 0,
            bytes_used: 0,
        })
        .await
        .unwrap();
        idx.put_manifest(&m).await.unwrap();

        let got = idx.get_manifest(&m.file_id).await.unwrap().unwrap();
        assert_eq!(got, m);
        let by_name = idx
            .get_by_name(&tenant, "report.pdf")
            .await
            .unwrap()
            .unwrap();
        assert_eq!(by_name.file_id, m.file_id);

        let list = idx.list_by_tenant(&tenant, false, 10, 0).await.unwrap();
        assert_eq!(list.len(), 1);
        assert_eq!(idx.count_by_tenant(&tenant, false).await.unwrap(), 1);
        let tagged = idx.list_ids_by_tag(&tenant, "q1").await.unwrap();
        assert_eq!(tagged.len(), 1);

        let t = idx.get_tenant(&tenant).await.unwrap().unwrap();
        assert_eq!(t.file_count, 1);
        assert_eq!(t.bytes_used, 500);

        idx.soft_delete_manifest(&m.file_id).await.unwrap();
        assert_eq!(
            idx.list_by_tenant(&tenant, false, 10, 0)
                .await
                .unwrap()
                .len(),
            0
        );
        assert_eq!(
            idx.list_by_tenant(&tenant, true, 10, 0)
                .await
                .unwrap()
                .len(),
            1
        );

        let removed = idx.remove_manifest(&m.file_id).await.unwrap().unwrap();
        assert!(removed.is_deleted());
        assert!(idx.get_manifest(&m.file_id).await.unwrap().is_none());
        assert_eq!(idx.get_by_name(&tenant, "report.pdf").await.unwrap(), None);
        let t2 = idx.get_tenant(&tenant).await.unwrap().unwrap();
        assert_eq!(t2.file_count, 0);
        assert_eq!(t2.bytes_used, 0);
    }

    #[tokio::test]
    async fn same_name_replace_keeps_index_consistent() {
        let (idx, _dir) = open();
        let tenant = Uuid::now_v7();
        let m1 = manifest("same.bin", tenant, 100);
        idx.put_manifest(&m1).await.unwrap();
        let mut m2 = manifest("same.bin", tenant, 200);
        m2.file_id = Uuid::now_v7();
        idx.put_manifest(&m2).await.unwrap();

        let by_name = idx.get_by_name(&tenant, "same.bin").await.unwrap().unwrap();
        assert_eq!(by_name.file_id, m2.file_id);
        // name_index is a unique index: only the newest file is listed.
        // (Name-replacement soft-deletes happen in the upload pipeline,
        // not in put_manifest.) The old manifest stays reachable by id.
        assert_eq!(
            idx.list_by_tenant(&tenant, true, 10, 0)
                .await
                .unwrap()
                .len(),
            1
        );
        let old = idx.get_manifest(&m1.file_id).await.unwrap().unwrap();
        assert!(!old.is_deleted());
    }

    #[tokio::test]
    async fn audit_append_and_read() {
        let (idx, _dir) = open();
        for i in 0..5 {
            idx.append_audit(&AuditEvent {
                id: Uuid::now_v7(),
                ts_ms: now_ms(),
                actor: format!("user{i}"),
                tenant_id: Some(Uuid::now_v7()),
                action: "upload".into(),
                object: "file".into(),
                ip: "127.0.0.1".into(),
                status: "ok".into(),
                detail: String::new(),
            })
            .await
            .unwrap();
        }
        let evs = idx.list_audit(3).await.unwrap();
        assert_eq!(evs.len(), 3);
        assert!(evs
            .windows(2)
            .all(|w| w[0].id.as_bytes() >= w[1].id.as_bytes()));
    }

    #[tokio::test]
    async fn tenant_crud() {
        let (idx, _dir) = open();
        let t = Tenant {
            id: Uuid::now_v7(),
            name: "acme".into(),
            created_at_ms: 1,
            file_count: 2,
            bytes_used: 42,
        };
        idx.upsert_tenant(&t).await.unwrap();
        assert_eq!(idx.list_tenants().await.unwrap().len(), 1);
        assert_eq!(idx.get_tenant(&t.id).await.unwrap().unwrap().bytes_used, 42);
    }

    #[tokio::test]
    async fn prefix_end_handles_ff() {
        assert_eq!(IndexDb::prefix_end(&[1, 2, 3]), Some(vec![1, 2, 4]));
        assert_eq!(IndexDb::prefix_end(&[1, 255]), Some(vec![2]));
        assert_eq!(IndexDb::prefix_end(&[255, 255]), None);
    }
}

#[cfg(test)]
mod quota_tests {
    use super::*;
    use std::sync::Arc as SArc;

    fn manifest(tenant: Uuid, name: &str, stored: u64) -> Manifest {
        Manifest {
            file_id: Uuid::now_v7(),
            name: name.into(),
            tenant_id: tenant,
            mime: "application/octet-stream".into(),
            size_original: stored,
            size_stored: stored,
            original_hash: [1u8; 32],
            chunks: vec![],
            tags: vec![],
            created_at_ms: 0,
            updated_at_ms: 0,
            deleted_at_ms: None,
            version: 1,
        }
    }

    /// Contract gate (PG-1R I-2): N concurrent writers racing the boundary;
    /// committed bytes never exceed quota; rejections surface as the quota
    /// error class. 100 iterations x 4 writers against a limit that admits
    /// exactly 2 of 4 files.
    #[tokio::test]
    async fn quota_commit_race_cannot_overshoot() {
        for _iter in 0..100 {
            let dir = tempfile::tempdir_in(std::env::temp_dir()).unwrap();
            let idx = SArc::new(IndexDb::open(&dir.path().join("i.redb")).unwrap());
            let tenant = Uuid::now_v7();
            let limit = 2000u64; // admits exactly two 1000-byte files
            let mut handles = Vec::new();
            for w in 0..4u32 {
                let idx = idx.clone();
                handles.push(tokio::spawn(async move {
                    idx.put_manifest_checked(
                        &manifest(tenant, &format!("race-{w}"), 1000),
                        Some(limit),
                    )
                    .await
                }));
            }
            let mut ok = 0;
            let mut rejected = 0;
            for h in handles {
                match h.await.unwrap() {
                    Ok(()) => ok += 1,
                    Err(StorageError::TooLarge(m)) if m.contains("quota exceeded") => rejected += 1,
                    Err(e) => panic!("wrong error class: {e}"),
                }
            }
            assert_eq!(ok, 2, "exactly two commits fit the limit");
            assert_eq!(rejected, 2, "the rest rejected with the quota class");
            let q = crate::storage::migrate::quota_of(&idx.db, &tenant).unwrap();
            assert!(
                q.committed_bytes <= limit,
                "OVERSHOOT: committed {} > limit {}",
                q.committed_bytes,
                limit
            );
            assert_eq!(q.committed_bytes, 2000);
        }
    }

    /// I-7 (cc row 5): the quota failpoint fires after the re-check passes —
    /// the txn aborts (panic), so nothing is written; recovery = retry.
    #[tokio::test]
    #[ignore = "requires FAILPOINT env + failpoints feature"]
    async fn quota_commit_after_check_failpoint_aborts_cleanly() {
        // With FAILPOINT=quota-commit-after-check and --features failpoints:
        // put_manifest_checked panics at the failpoint; the redb txn (held
        // by the panicked task) aborts; a subsequent checked write succeeds
        // — no partial manifest, no quota delta.
    }
}
