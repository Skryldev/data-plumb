<!--
title: Storage Layout
category: architecture
audience: architect
last_updated: 2026-11-10
-->

# Storage Layout

This page documents exactly what a running node writes to disk: the sharded content-addressed blob tree, the six redb tables and their binary encodings, the `Manifest` structure that ties them together, and the crash-atomicity story that binds blobs and index entries. Architects planning backups, capacity, or disaster recovery should read it first.

## Overview

There are exactly three kinds of durable state, controlled by three config sections:

| Config key | Durable state | Contents |
|---|---|---|
| `storage.root` (`data/blocks`) | Blob tree | One immutable `.bin` file per unique chunk |
| `database.path` (`data/index.redb`) | Metadata | Single redb file with six tables |
| `storage.temp_dir` (`data/tmp`) | Transient | Multipart upload spool (never authoritative) |

Blobs carry no metadata of their own — their name is their content hash. All structure lives in redb, which is what keeps the write path atomic and the read path verifiable.

## How it works

### Sharded blob tree

Blobs live at `<storage.root>/<ab>/<cd>/<64-hex>.bin`, where `ab`/`cd` are the first `storage.shard_depth` bytes of the 32-byte storage hash rendered as hex. With the default `shard_depth: 2`, a chunk whose hash begins `3f9a4b…` lands at:

```text
data/blocks/
└── 3f/
    └── 9a/
        └── 3f9a4b17c0d8e5f2a1b3c4d5e6f708192a3b4c5d6e7f8091a2b3c4d5e6f70819.bin
```
*Figure 1: Blob placement at the default `shard_depth: 2` — two hex-char directories, then the full 64-hex file name.*

`shard_depth: 0` is a flat directory. The depth is capped at 4, and the recorded relative path (`3f/9a/<hash>.bin`) is stored in chunk metadata, so reads never re-derive layout assumptions:

| `shard_depth` | Layout | Directories at level 2ⁿ | Blobs per directory at 1M chunks | Trade-off |
|---|---|---|---|---|
| 0 | `<root>/<hash>.bin` | — | 1 000 000 | Simplest; large directories stress some filesystems |
| 1 | `ab/<hash>.bin` | 256 | ~3 900 | Modest spread; still big directories |
| 2 (default) | `ab/cd/<hash>.bin` | 65 536 | ~15 | Balanced; two small `stat` hops |
| 3 | `ab/cd/ef/<hash>.bin` | 16.7 M | <1 | Deep trees slow the orphan sweeper's walk |
| 4 | `ab/cd/ef/01/<hash>.bin` | 4.3 B | <1 | Maximum fan-out; directory overhead dominates |

Blob writes are idempotent: `BlockStore::write` returns `false` without touching anything if the target already exists, so concurrent writers of the same chunk converge safely. The orphan sweeper walks the tree to `shard_depth + 1` levels, skipping `.tmp-*` and `.write-probe-*` files.

### The six redb tables

All keys and values are raw bytes; the encodings are explicit per table:

```text
database.path = data/index.redb            (redb 2.2, single file, WAL + fsync)
+------------------------------------------------------------------------------+
| chunks     | key: 32-byte storage hash (raw)                                 |
|            | val: bincode ChunkMeta { refcount: u64,                         |
|            |                          blob_size: u64,   // nonce+tag incl.   |
|            |                          created_at_ms: i64,                     |
|            |                          on_disk_path: String  // "3f/9a/<hex>.bin" |
|            |        }                                                        |
|------------+-----------------------------------------------------------------|
| files      | key: 16-byte file UUID (raw)                                    |
|            | val: MessagePack Manifest                                       |
|------------+-----------------------------------------------------------------|
| name_index | key: tenant(16B) || 0x00 || name (bytes)                        |
|            | val: 16-byte file UUID                                          |
|------------+-----------------------------------------------------------------|
| tag_index  | key: tenant(16B) || 0x00 || tag || 0x00 || file_id (16B)        |
|            | val: empty (the key carries all information)                    |
|------------+-----------------------------------------------------------------|
| audit      | key: UUIDv7 (16B, time-ordered -> cheap range scans)            |
|            | val: JSON AuditEvent                                            |
|------------+-----------------------------------------------------------------|
| tenants    | key: 16-byte tenant UUID (raw)                                  |
|            | val: JSON Tenant                                                |
+------------------------------------------------------------------------------+
```
*Figure 2: The six redb tables. Every write transaction commits with `Durability::Immediate`; blocking redb work runs on tokio's blocking pool.*

Design notes on the two indexes:

- `name_index` makes file names unique per tenant — the `0x00` separator is unambiguous because tenant keys are fixed-width 16 bytes.
- `tag_index` keys embed the `file_id`, so listing by tag is a byte-prefix scan (`tenant || 0x00 || tag || 0x00`) and removal is a key delete. Tag queries take precedence over name filters in `GET /api/v1/files`.
- `audit` keys are UUIDv7, so the audit log is physically time-ordered — `GET /api/v1/admin/audit?limit=` walks the tail of the table.
- The tenant prefix in both indexes is what enforces tenant isolation at the storage layer: a scan scoped to one tenant's bytes cannot return another tenant's rows.

### Manifest structure

A `Manifest` is the authoritative description of a stored file, serialised with MessagePack into `files`:

| Field | Type | Meaning |
|---|---|---|
| `file_id` | Uuid (16 B) | UUIDv7, minted at upload; primary key of `files` |
| `name` | String | Display name; unique per tenant via `name_index` |
| `tenant_id` | Uuid | Owning tenant; part of all index key prefixes |
| `mime` | String | MIME type supplied at upload |
| `size_original` | u64 | Plaintext byte count |
| `size_stored` | u64 | Sum of `size_stored` across chunks |
| `original_hash` | [u8; 32] | BLAKE3 over the whole plaintext file; becomes the HTTP ETag |
| `chunks` | Vec&lt;ChunkRef&gt; | Ordered by offset; drives retrieval and ranges |
| `tags` | Vec&lt;String&gt; | Indexed in `tag_index` |
| `created_at_ms` / `updated_at_ms` | i64 | Epoch milliseconds |
| `deleted_at_ms` | Option&lt;i64&gt; | Soft-delete tombstone; `None` while live |
| `version` | u64 | Optimistic-concurrency counter, bumped on mutation |

Each `ChunkRef` records: `hash` (32-byte block-store key), `plain_hash` (32-byte plaintext BLAKE3 used for read-back verification), `offset`, `size_original`, `size_stored`, and explicit `compressed` / `encrypted` flags — no magic-byte sniffing at read time.

### Atomicity: blob before index

The write path is arranged so that any crash leaves recoverable state:

```text
1. write blob to .tmp-<uuidv7>        (same shard directory)
2. fsync the temp file                (data durable)
3. rename .tmp-<uuidv7> -> <hash>.bin (atomic within the directory)
4. fsync the shard directory          (rename survives power loss)
        ---- only now ----
5. ONE redb transaction:
      refcount++ / Manifest insert / index updates
      commit durability = Durability::Immediate (WAL + fsync)
```
*Figure 3: The atomic write sequence. The index transaction runs only after the blob is fully durable.*

Consequences of this ordering:

- **No dangling references.** The index never points at a blob that is not already on disk.
- **Crash residue is bounded.** A crash between steps 4 and 5 leaves an orphan blob with no metadata row. The GC orphan sweep deletes it only after it ages past `retention.orphan_min_age_secs` (default 3600), which is long enough to exclude blobs that are legitimately mid-upload.
- **One fsync boundary per upload.** All of a file's chunk refcounts and its manifest commit in a single redb transaction, so dedup refcount updates are race-free and a partially-registered file is impossible.
- **Same-name replacement is soft.** Re-uploading under an existing name soft-deletes the old manifest (`deleted_at_ms` set) but keeps its refcounts, so shared blobs stay alive until GC purges the tombstone.

## Configuration

| Key | Default | Notes |
|---|---|---|
| `storage.root` | `data/blocks` | Block-store root; mount on the volume that must survive |
| `storage.temp_dir` | `data/tmp` | Multipart spool; same filesystem as `root` avoids cross-device renames |
| `storage.shard_depth` | `2` | 0–4; changing it affects new writes and the sweeper's walk |
| `database.path` | `data/index.redb` | Single-file ACID index; copy for backups while the process is stopped |

Environment overrides follow the `FS_` + `__` convention, for example `FS_STORAGE__SHARD_DEPTH=1` or `FS_DATABASE__PATH=/var/lib/file-storage/index.redb`. There is no hot reload.

## Examples

Chunk-level metadata is exposed through the manifest endpoint, showing exactly what landed where:

```bash
curl -s -H "Authorization: Bearer $TOKEN" \
     "localhost:8080/api/v1/files/<id>/meta?chunks=true" | jq '.chunks[0]'
```

```json
{
  "hash": "3f9a4b17c0d8e5f2a1b3c4d5e6f708192a3b4c5d6e7f8091a2b3c4d5e6f70819",
  "plain_hash": "7c1e9a04b2d8f6a3c5e1b7d0924f8a3c6e5d1b0a9f8e7d6c5b4a3f2e1d0c9b8a7",
  "offset": 0,
  "size_original": 33161,
  "size_stored": 12104,
  "compressed": true,
  "encrypted": false
}
```

The `hash` above is the storage key — the blob's file name plus its `chunks`-table row. With encryption enabled the two hashes diverge, because the storage key becomes `BLAKE3(tenant_id || plain_hash)` (see [ADR 0007](./decisions/0007-encrypted-dedup-key-derivation.md)).

On-disk, the same upload looks like:

```console
$ find data/blocks -type f | head -3
data/blocks/3f/9a/3f9a4b17….bin
data/blocks/ab/cd/abcd1234….bin
data/blocks/0e/5f/0e5f8888….bin
```

## See also

- [Data Flow](./data-flow.md) — the write/read paths that produce this layout
- [Chunking & Dedup](./chunking.md) — why blobs are keyed by content hashes
- [Garbage Collection](./gc.md) — how orphan blobs and zero-refcount blobs are reclaimed
- [Caching](./caching.md) — what is kept in memory instead of re-reading these files
- [ADR 0003 — redb](./decisions/0003-redb-over-sled.md) — why a single-file ACID store was chosen
- [ADR 0007 — Encrypted dedup key](./decisions/0007-encrypted-dedup-key-derivation.md) — storage-key derivation under encryption
- [Backup & Restore](../operations/backup-restore.md) — operational procedures for these files
- [Configuration reference](../configuration/reference.md) — every `storage.*` and `database.*` key
