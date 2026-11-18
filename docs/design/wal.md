# Design: Write-Ahead Log (WAL)

Status: APPROVED (V9 PG-1, contract §2.2; D2 — one WAL, four consumers).

## 1. Scope

A logical, CRC-guarded, segmented intent log of METADATA transactions.
Consumers: (1) crash recovery (PG-1), (2) group-commit batching (closes
V6-10), (3) replication feed (PG-1 wiring; replaces per-chunk
registration), (4) incremental backup / PITR (PG-2 §3.3). No subsystem
may build its own durable log (P2).

## 2. Location & segments

`<data_dir>/wal/wal-<seq>.log`, seq = u64 monotonically increasing,
zero-padded to 20 digits for lexical order. Segment rotates at
`wal.segment_max_bytes` (default 64 MiB) or on checkpoint (retention:
segments fully below the minimum live watermark are deleted after a
successful checkpoint).

## 3. Record grammar

```
[len: u32 LE] [crc32c: u32 LE] [type: u8] [payload: len-4 bytes]
types:
  0x01 BEGIN_TXN   {txn_id: u64}
  0x02 OP          {txn_id: u64, op_code: u8, rec_ver: u16 LE, body}
       op codes: 1 upsert_manifest(key, record) 2 delete_manifest(key)
                 3 chunk_ref_delta(hash, delta i64) 4 quota_delta(tenant, bytes i64, files i64)
                 5 tombstone(key, at_ms) 6 pipeline_job_upsert (reserved, PG-3)
  0x03 COMMIT_TXN  {txn_id: u64}
  0x04 CHECKPOINT  {watermark_txn_id: u64}
  0x00 NOP_PAD     (zero payload — fills a segment tail)
```

All payloads are version-tagged (`rec_ver`) from day one; unknown
rec_ver ⇒ refuse replay with a loud error (forward-safety, P5).

## 4. Write path & group-commit

1. `begin_txn()` → txn buffer (ops accumulate in memory).
2. `commit(txn)` → append BEGIN+OPs+COMMIT into the group buffer; the
   batch flushes at `max(wal.group_commit_delay_ms=5, buffer ≥
   wal.group_commit_max_bytes=256KiB)`: ONE `File::sync_data` per batch
   (the fsync the whole batch shares — closes V6-10's per-write fsync
   cost), then the txn is applied to redb, then
   `last_applied_txn_id` (watermark) is recorded INSIDE redb (a separate
   tiny committed txn — the state of record is redb; the WAL is intent).
3. Fsync accounting is injected (`wal.fsync_hook`) so
   `wal_group_commit_batches_under_burst` can count.

An fsync EIO on the WAL → fail-stop (PG-2 durability.md): mark
read-only, drain, exit non-zero.

## 5. Recovery (boot)

lock data dir → scan segments in seq order → per record: read len →
read crc+payload → verify crc32c → dispatch:
- COMMIT_TXN with txn_id > redb watermark → collect; on COMMIT apply the
  txn's ops to redb in one txn.
- CRC failure / truncated tail → RECOVERY STOPS at the last valid
  record; the partial tail is truncated from the segment;
  `wal_recovered_tail_total++`; WARN names segment + byte offset.
  NEVER continue past a mid-file CRC failure.
- End of log → write CHECKPOINT(watermark), apply retention.

Idempotency: op application is upsert/delete by key, so replay after a
partially-written redb apply (crash between WAL fsync and redb commit)
is safe — the watermark only advances after the redb txn commits.

## 6. Replication feed (consumer #3)

Followers tail segments via the existing HTTP channel:
`GET /internal/wal/segment?seq=&offset=` streams raw records; the
follower applies COMMITTED txns txn-atomically (one redb txn per txn_id)
and advances its own watermark. The per-chunk registration path
(`register_chunk_verified` singular) is REMOVED — replication apply is
txn-atomic, closing V6-10. Gate:
`replication_applies_txn_atomically`.

## 7. Consumers designed now, implemented later

Backup (PG-2): `xtask snapshot` flushes + checkpoints, then copies
trees; PITR = replay to `--to-txn`. Search (PG-3): a watermark
subscriber drains commits into the indexer. Interfaces (segment reader,
watermark query) are public in `storage::wal` from PG-1.

## 8. Crash-consistency contract

Every WAL-guarded mutation is listed in crash-consistency.md with its
failpoint name; the harness (`FAILPOINT` env behind the `failpoints`
feature) can abort at: `wal-before-fsync`, `wal-after-fsync-pre-apply`,
`wal-after-apply-pre-watermark`, plus the upload/GC points from the
existing table. Gate `wal_kill_at_every_record_boundary` walks every
byte-boundary truncation of a scripted txn sequence and asserts the
post-recovery state is pre-txn or post-txn — never half.
