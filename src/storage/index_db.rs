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

#[derive(Clone)]
pub struct IndexDb {
    db: Arc<Database>,
}

/// Result of an upload's chunk registration: per-chunk `true` = dedup hit.
pub type UpsertReport = Vec<bool>;

impl IndexDb {
    pub fn open(path: &std::path::Path) -> Result<Self, StorageError> {
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
        Ok(idx)
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

    /// Batch registration used by the upload pipeline: a single atomic
    /// transaction (one fsync) handles every chunk of a file. For each
    /// `(hash, blob_size, path)`: existing -> refcount += 1 (dedup hit);
    /// absent -> insert with refcount 1.
    pub async fn register_chunks(
        &self,
        items: Vec<([u8; 32], u64, String)>,
    ) -> Result<UpsertReport, StorageError> {
        let now = now_ms();
        self.write(move |txn| {
            let mut table = txn.open_table(T_CHUNKS).map_err(dbe)?;
            let mut report = Vec::with_capacity(items.len());
            for (h, blob_size, path) in &items {
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
                        report.push(true);
                    }
                    None => {
                        let meta = ChunkMeta {
                            refcount: 1,
                            blob_size: *blob_size,
                            created_at_ms: now,
                            on_disk_path: path.clone(),
                        };
                        let enc = bincode::serialize(&meta).map_err(ser)?;
                        table.insert(h.as_slice(), enc.as_slice()).map_err(dbe)?;
                        report.push(false);
                    }
                }
            }
            Ok(report)
        })
        .await
    }

    /// Replica-side insert: creates metadata only when absent, never bumps
    /// refcounts (replicas use reachability GC).
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
                    Some(_) => {
                        table.remove(h.as_slice()).map_err(dbe)?;
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
        let h = [3u8; 32];
        let meta = ChunkMeta {
            refcount: 1,
            blob_size: 10,
            created_at_ms: 0,
            on_disk_path: "ab/cd.bin".into(),
        };
        assert!(!idx.upsert_chunk(&h, meta.clone()).await.unwrap());
        assert!(idx.upsert_chunk(&h, meta).await.unwrap());
        let m = idx.get_chunk_meta(&h).await.unwrap().unwrap();
        assert_eq!(m.refcount, 2);
        assert_eq!(idx.decr_refcounts(&[h]).await.unwrap().len(), 0);
        let zeroed = idx.decr_refcounts(&[h]).await.unwrap();
        assert_eq!(zeroed.len(), 1);
        assert!(idx.get_chunk_meta(&h).await.unwrap().is_none());
    }

    #[tokio::test]
    async fn batch_upsert_reports_dedup() {
        let (idx, _dir) = open();
        let items: Vec<([u8; 32], u64, String)> = (0..4u8)
            .map(|i| ([i; 32], 100 + i as u64, format!("p/{i}.bin")))
            .collect();
        let r1 = idx.register_chunks(items.clone()).await.unwrap();
        assert!(r1.iter().all(|hit| !*hit));
        let r2 = idx.register_chunks(items).await.unwrap();
        assert!(r2.iter().all(|hit| *hit));
        let (count, bytes) = idx.chunk_stats().await.unwrap();
        assert_eq!(count, 4);
        assert_eq!(bytes, 100 + 101 + 102 + 103);
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
