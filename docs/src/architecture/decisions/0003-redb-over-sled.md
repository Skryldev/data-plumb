<!--
title: ADR 0003 — redb as the metadata index
category: architecture
audience: architect
last_updated: 2026-11-10
-->

# ADR 0003: redb over sled

This decision record explains why the metadata index (manifests, chunk refcounts, name/tag indexes, audit log, tenants) is a single redb file rather than sled, SQLite, or RocksDB. The index is the transactional heart of the service, so this choice shapes crash safety and latency.

## Overview

The service needs an embedded, ACID, crash-safe key-value store holding six byte-keyed tables, accessed from an async runtime. redb 2.2 was chosen: a single-file, pure-Rust B-tree store with an explicit durability control (`Durability::Immediate`), an active maintenance record, and a synchronous API that slots cleanly into `spawn_blocking`.

## Status

**Accepted** — 2026-11-10.

## Context

The index must provide:

- **ACID transactions with fsync-able commits** — the write path collapses a whole upload's refcount changes and manifest insert into one transaction; a torn commit must be impossible.
- **Six byte-string-keyed tables** — `chunks`, `files`, `name_index`, `tag_index`, `audit`, `tenants`; raw byte keys and values keep encodings (bincode / MessagePack / JSON) explicit per table.
- **Embedded operation** — the deployment model is a single process with a data directory; requiring a networked database service contradicts it.
- **Bounded latency tails** — background compaction pauses or GC stalls in the write path would show up directly in upload latency.
- **Single-file durability story** — WAL-backed writes whose commit survives power loss without external tooling.

## Decision

Use redb (2.2) with the database file at `database.path` (default `data/index.redb`). Every write transaction sets `txn.set_durability(Durability::Immediate)` before commit — WAL plus fsync. All blocking redb work runs on tokio's blocking pool behind async wrappers (`IndexDb::write`/`read`), and table guards are always scoped before mutating the same table, since redb borrows are exclusive. Keys and values are raw bytes; serialisation is bincode for `ChunkMeta`, MessagePack for `Manifest`, JSON for audit events and tenants.

## How it works

```rust
const T_CHUNKS: TableDefinition<&[u8], &[u8]> = TableDefinition::new("chunks");
// ... files, name_index, tag_index, audit, tenants

async fn write<T, F>(&self, f: F) -> Result<T, StorageError> {
    tokio::task::spawn_blocking(move || {
        let mut txn = db.begin_write()?;
        txn.set_durability(Durability::Immediate);   // WAL + fsync on commit
        let out = f(&txn)?;
        txn.commit()?;
        Ok(out)
    }).await
}
```
*Figure 1: The canonical write pattern in `src/storage/index_db.rs`. One upload = one such transaction covering all chunk refcounts plus the manifest.*

The upload pipeline depends on exactly this behaviour: `register_chunks` (refcount increments with a per-chunk dedup report) and `put_manifest` commit atomically, so a partially-registered file can never be observed.

## Consequences

**Positive**

- **Real ACID semantics** with an explicit durability knob — the crash-safety story (blob-before-index, then one fsynced transaction) is provable rather than hopeful.
- **Single-file embeddability**: no daemon, no network, no connection pool; the whole metadata state is one file, which simplifies backup and restore ([Backup & Restore](../../operations/backup-restore.md)).
- **Synchronous, simple API**: no async-safety surprises; blocking calls are confined to `spawn_blocking` where they belong.
- **Pure Rust**: no C/C++ FFI build surface; the release binary stays ~11.7 MB and distroless-friendly.
- **Actively maintained** upstream, with a stable 2.x API.

**Negative**

- **Single-writer file lock**: the database is opened by one process; no multi-process sharing of one index.
- **B-tree over LSM trade**: sustained, write-heavy churn (very high upload rates with heavy dedup refcount updates) gives more headroom to LSM engines, at the cost of compaction stalls.
- **One file = mixed IO**: audit appends and hot-path writes share the same WAL/fsync budget.
- **Ecosystem tooling is thin** compared with SQLite; inspection means the service's own API rather than a universal CLI.

## Alternatives considered

- **sled** — the natural Rust-native candidate when the project started, but upstream maintenance stalled (unreleased, unfinished guarantees); building an archive format on a frozen project was unacceptable. Rejected on project health.
- **SQLite (rusqlite)** — battle-tested ACID, superior tooling (`sqlite3` CLI), but heavier: C FFI build, SQL/schema layer for what is pure byte-keyed access, and per-query parsing overhead for hot refcount paths. Rejected as unnecessary weight for these access patterns; a defensible choice if rich ad-hoc querying ever becomes a requirement.
- **RocksDB** — excellent write throughput, but LSM background compaction introduces latency stalls directly in the upload path, the C++ FFI complicates the distroless build, and its operational knobs far exceed the need for six small tables. Rejected on tail latency and build complexity.
- **LMDB (via bindings)** — strong read performance and crash safety, but the mmap model and C dependency complicate the deployment; write concurrency limits would bottleneck the 8-wide pipeline. Rejected.

## Usage

| Key | Default | Notes |
|---|---|---|
| `database.path` | `data/index.redb` | Single file; back up while the process is stopped |

Startup creates or opens the file and initialises all six tables. There are no tunables beyond the path by design — durability is fixed at `Immediate` for correctness.

## Examples

The six tables and their encodings are documented with a layout diagram in [Storage Layout](../storage-layout.md). A minimal read through the API:

```rust
let meta = index.get_chunk_meta(&hash).await?;   // Option<ChunkMeta>, bincode
let manifest = index.get_manifest(&file_id).await?; // Option<Manifest>, msgpack
```

Reads run without durability overhead (no fsync), so L3-cache misses on the manifest path cost one blocking-pool round trip.

## See also

- [Storage Layout](../storage-layout.md) — the six tables and their encodings
- [ADR 0001 — CAS over block store](./0001-cas-over-block-store.md) — why blobs live outside the database
- [Data Flow](../data-flow.md) — the single-transaction commit in sequence
- [Garbage Collection](../gc.md) — the refcount sweeps that read these tables
- [Backup & Restore](../../operations/backup-restore.md) — operating on the single index file
