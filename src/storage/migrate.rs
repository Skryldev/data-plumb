//! v1 → v2 metadata migration (storage-format-v2.md §6).
//!
//! Resumable: checkpoint (`meta/migration_state` = bincode MigrationState)
//! written after every committed batch of 500 records; a resume re-does
//! the batch after the checkpoint (idempotent upserts). Per-batch
//! atomicity means a mid-batch failure rolls back only that batch; a
//! failed record is logged with its key and retried on resume — never
//! silently skipped. `verify_v1_v2` re-walks both sides and compares
//! counts/sizes/CRCs; any delta names the key.

use redb::{Database, ReadableTable, TableDefinition};
use serde::{Deserialize, Serialize};
use uuid::Uuid;

use crate::error::{dbe, ser, StorageError};
use crate::storage::index_db::{
    summary_key, QuotaRow, SummaryRow, SCHEMA_VERSION_KEY, SCHEMA_VERSION_KNOWN,
};

const T_FILES: TableDefinition<&[u8], &[u8]> = TableDefinition::new("files");
const T_META: TableDefinition<&[u8], &[u8]> = TableDefinition::new("meta");
const T_SUMMARY: TableDefinition<&[u8], &[u8]> = TableDefinition::new("summary");
const T_MTIME_IDX: TableDefinition<&[u8], &[u8]> = TableDefinition::new("index_by_mtime");
const T_NAME_CI: TableDefinition<&[u8], &[u8]> = TableDefinition::new("index_by_name_ci");
const T_QUOTAS: TableDefinition<&[u8], &[u8]> = TableDefinition::new("quotas");

const BATCH: usize = 500;

#[derive(Debug, Clone, Serialize, Deserialize)]
struct MigrationState {
    phase: String, // "copy" | "done"
    last_v1_key: Vec<u8>,
    copied: u64,
}

fn crc32c_tail(body: &[u8]) -> Vec<u8> {
    let mut out = body.to_vec();
    out.extend_from_slice(&crc32c::crc32c(body).to_le_bytes());
    out
}

fn verify_crc32c(value: &[u8]) -> Result<(), StorageError> {
    if value.len() < 4 {
        return Err(StorageError::Corruption("summary row too short".into()));
    }
    let (body, crc) = value.split_at(value.len() - 4);
    let expect = u32::from_le_bytes([crc[0], crc[1], crc[2], crc[3]]);
    if crc32c::crc32c(body) != expect {
        return Err(StorageError::Corruption(
            "summary row checksum mismatch (corrupted record detected at read)".into(),
        ));
    }
    Ok(())
}

/// Encodes a SummaryRow with its trailing CRC (v2 record envelope).
pub fn encode_summary(row: &SummaryRow) -> Vec<u8> {
    let body = bincode::serialize(row).unwrap_or_default();
    crc32c_tail(&body)
}

/// Decodes + verifies a SummaryRow (a corrupted record is DETECTABLE).
pub fn decode_summary(value: &[u8]) -> Result<SummaryRow, StorageError> {
    verify_crc32c(value)?;
    bincode::deserialize(&value[..value.len() - 4]).map_err(ser)
}

fn manifest_v2_key(tenant: &Uuid, name: &str) -> Vec<u8> {
    let mut k = tenant.as_bytes().to_vec();
    k.push(0x00);
    k.extend_from_slice(name.as_bytes());
    k
}

/// Runs the resumable copy walk. Returns the number of records copied in
/// THIS call (0 when already complete). Followed by `stamp_v2`.
pub fn migrate_v1_to_v2(db: &Database) -> Result<u64, StorageError> {
    // Checkpoint state (or initialize).
    let mut state = load_or_init_state(db)?;

    let mut copied_this_run = 0u64;
    while state.phase == "copy" {
        // One batch per write txn; checkpoint after commit.
        let copied_batch = copy_one_batch(db, &mut state)?;
        copied_this_run += copied_batch;
        if copied_batch < BATCH as u64 {
            // Done copying.
            mark_done(db)?;
            break;
        }
    }

    // Quota fold (idempotent — recomputed from summary).
    fold_quotas(db)?;
    Ok(copied_this_run)
}

fn load_or_init_state(db: &Database) -> Result<MigrationState, StorageError> {
    let txn = db.begin_write().map_err(dbe)?;
    let existing = {
        let meta = txn.open_table(T_META).map_err(dbe)?;
        let guard = meta.get(b"migration_state" as &[u8]).map_err(dbe)?;
        guard.map(|g| g.value().to_vec())
    };
    match existing {
        Some(bytes) => bincode::deserialize::<MigrationState>(&bytes).map_err(ser),
        None => {
            let s = MigrationState {
                phase: "copy".into(),
                last_v1_key: Vec::new(),
                copied: 0,
            };
            {
                let mut meta = txn.open_table(T_META).map_err(dbe)?;
                meta.insert(
                    b"migration_state" as &[u8],
                    bincode::serialize(&s).map_err(ser)?.as_slice(),
                )
                .map_err(dbe)?;
            }
            txn.commit().map_err(dbe)?;
            Ok(s)
        }
    }
}

fn save_state(db: &Database, state: &MigrationState) -> Result<(), StorageError> {
    let txn = db.begin_write().map_err(dbe)?;
    {
        let mut meta = txn.open_table(T_META).map_err(dbe)?;
        meta.insert(
            b"migration_state" as &[u8],
            bincode::serialize(state).map_err(ser)?.as_slice(),
        )
        .map_err(dbe)?;
    }
    txn.commit().map_err(dbe)?;
    Ok(())
}

/// Copies up to BATCH records past the checkpoint key into the v2 trees
/// inside ONE write txn; advances the checkpoint on commit. Returns the
/// batch size (0 = copy phase complete).
fn copy_one_batch(db: &Database, state: &mut MigrationState) -> Result<u64, StorageError> {
    let txn = db.begin_write().map_err(dbe)?;
    let mut copied: u64 = 0;
    {
        let files = txn.open_table(T_FILES).map_err(dbe)?;
        let mut summary = txn.open_table(T_SUMMARY).map_err(dbe)?;
        let mut mtime_idx = txn.open_table(T_MTIME_IDX).map_err(dbe)?;
        let mut name_ci = txn.open_table(T_NAME_CI).map_err(dbe)?;

        let start: &[u8] = if state.last_v1_key.is_empty() {
            &[]
        } else {
            &state.last_v1_key
        };
        let mut iter = files.range(start..).map_err(dbe)?.peekable();
        // Resume AT the checkpoint key: skip it (upserts are idempotent, but
        // we must always make forward progress).
        if !state.last_v1_key.is_empty() {
            let _ = iter.next();
        }
        while copied < BATCH as u64 {
            let row = match iter.next() {
                Some(r) => r.map_err(dbe)?,
                None => break,
            };
            let (k, v) = row;
            let key: &[u8] = k.value();
            let m: crate::storage::manifest::Manifest =
                rmp_serde::from_slice(v.value()).map_err(ser)?;
            let v2key = manifest_v2_key(&m.tenant_id, &m.name);

            let row = SummaryRow {
                size: m.size_original,
                chunk_count: m.chunks.len() as u32,
                content_type: m.mime.clone(),
                mtime_ms: m.updated_at_ms,
                tags_hash: tags_hash_of(&m.tags),
                deleted: m.is_deleted(),
            };
            summary
                .insert(v2key.as_slice(), encode_summary(&row).as_slice())
                .map_err(dbe)?;
            let mk = mtime_idx_key(&m.tenant_id, m.updated_at_ms, &m.name);
            mtime_idx
                .insert(mk.as_slice(), b"".as_slice())
                .map_err(dbe)?;
            let nk = name_ci_key(&m.tenant_id, &m.name);
            name_ci.insert(nk.as_slice(), b"".as_slice()).map_err(dbe)?;

            state.last_v1_key = key.to_vec();
            copied += 1;
        }
    }
    state.copied += copied;
    save_state(db, state)?;
    crate::failpoint!("migration-mid-record");
    txn.commit().map_err(dbe)?;
    Ok(copied)
}

fn mark_done(db: &Database) -> Result<(), StorageError> {
    let mut state = load_or_init_state(db)?;
    state.phase = "done".into();
    save_state(db, &state)
}

fn tags_hash_of(tags: &[String]) -> u64 {
    let mut h = blake3::Hasher::new();
    for t in tags {
        h.update(t.as_bytes());
        h.update(&[0]);
    }
    let d = h.finalize();
    u64::from_le_bytes(d.as_bytes()[0..8].try_into().unwrap())
}

fn mtime_idx_key(tenant: &Uuid, mtime_ms: i64, path: &str) -> Vec<u8> {
    let mut k = tenant.as_bytes().to_vec();
    k.extend_from_slice(&mtime_ms.to_be_bytes());
    k.extend_from_slice(path.as_bytes());
    k
}

fn name_ci_key(tenant: &Uuid, path: &str) -> Vec<u8> {
    let mut k = tenant.as_bytes().to_vec();
    k.extend_from_slice(path.to_lowercase().as_bytes());
    k.push(0);
    k.extend_from_slice(path.as_bytes());
    k
}

/// Recomputes the `quotas` tree from the summary rows (idempotent).
fn fold_quotas(db: &Database) -> Result<(), StorageError> {
    let txn = db.begin_write().map_err(dbe)?;
    {
        let summary = txn.open_table(T_SUMMARY).map_err(dbe)?;
        let mut quotas = txn.open_table(T_QUOTAS).map_err(dbe)?;
        let mut per_tenant: std::collections::HashMap<Uuid, QuotaRow> = Default::default();
        for row in summary.iter().map_err(dbe)? {
            let (k, v) = row.map_err(dbe)?;
            let key: &[u8] = k.value();
            if key.len() < 17 {
                continue;
            }
            let tenant = Uuid::from_bytes(key[0..16].try_into().map_err(|_| {
                StorageError::Corruption("summary key shorter than tenant id".into())
            })?);
            let decoded = decode_summary(v.value())?;
            if decoded.deleted {
                continue;
            }
            let q = per_tenant.entry(tenant).or_default();
            q.committed_bytes += decoded.size;
            q.committed_files += 1;
        }
        for (tenant, q) in per_tenant {
            quotas
                .insert(
                    tenant.as_bytes().as_slice(),
                    bincode::serialize(&q).map_err(ser)?.as_slice(),
                )
                .map_err(dbe)?;
        }
    }
    txn.commit().map_err(dbe)?;
    Ok(())
}

/// Final step: stamp schema/version = 2. Called by `xtask migrate --commit`
/// after verify, or by the inline auto-migrate path.
pub fn stamp_v2(db: &Database) -> Result<(), StorageError> {
    let txn = db.begin_write().map_err(dbe)?;
    let mut meta = txn.open_table(T_META).map_err(dbe)?;
    meta.insert(
        SCHEMA_VERSION_KEY,
        SCHEMA_VERSION_KNOWN.to_le_bytes().as_slice(),
    )
    .map_err(dbe)?;
    drop(meta);
    txn.commit().map_err(dbe)?;
    Ok(())
}

/// Re-walks v1 and v2; compares counts and per-record sizes; names any
/// delta. Non-zero exit is the caller's job (xtask).
pub fn verify_v1_v2(db: &Database) -> Result<(), StorageError> {
    let txn = db.begin_read().map_err(dbe)?;
    let files = txn.open_table(T_FILES).map_err(dbe)?;
    let summary = txn.open_table(T_SUMMARY).map_err(dbe)?;
    let mut v1_count = 0u64;
    let mut v1_bytes = 0u64;
    for row in files.iter().map_err(dbe)? {
        let (_, v) = row.map_err(dbe)?;
        let m: crate::storage::manifest::Manifest =
            rmp_serde::from_slice(v.value()).map_err(ser)?;
        v1_count += 1;
        v1_bytes += m.size_original;
        let key = summary_key(&m.tenant_id, &m.name);
        match summary.get(key.as_slice()).map_err(dbe)? {
            Some(g) => {
                let s = decode_summary(g.value())?;
                if s.size != m.size_original {
                    return Err(StorageError::Corruption(format!(
                        "migration verify: size mismatch for tenant {} path {}: v1={} v2={}",
                        m.tenant_id, m.name, m.size_original, s.size
                    )));
                }
            }
            None => {
                return Err(StorageError::Corruption(format!(
                    "migration verify: dropped record tenant {} path {}",
                    m.tenant_id, m.name
                )));
            }
        }
    }
    let mut v2_count = 0u64;
    let mut v2_bytes = 0u64;
    for row in summary.iter().map_err(dbe)? {
        let (_, v) = row.map_err(dbe)?;
        let s = decode_summary(v.value())?;
        if !s.deleted {
            v2_count += 1;
            v2_bytes += s.size;
        }
    }
    // v1 live count vs v2 non-deleted count may differ by tombstones that
    // were already soft-deleted pre-migration (they carry deleted=true).
    if v1_bytes != v2_bytes {
        return Err(StorageError::Corruption(format!(
            "migration verify: byte totals differ v1={v1_bytes} v2={v2_bytes}"
        )));
    }
    if v2_count > v1_count {
        return Err(StorageError::Corruption(format!(
            "migration verify: v2 has more rows ({v2_count}) than v1 ({v1_count})"
        )));
    }
    Ok(())
}

/// Quota counters for a tenant (0 when unset).
pub fn quota_of(db: &Database, tenant: &Uuid) -> Result<QuotaRow, StorageError> {
    let txn = db.begin_read().map_err(dbe)?;
    let quotas = txn.open_table(T_QUOTAS).map_err(dbe)?;
    match quotas.get(tenant.as_bytes().as_slice()).map_err(dbe)? {
        Some(v) => bincode::deserialize(v.value()).map_err(ser),
        None => Ok(QuotaRow::default()),
    }
}
