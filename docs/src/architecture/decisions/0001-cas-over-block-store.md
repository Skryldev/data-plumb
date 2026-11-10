<!--
title: ADR 0001 — Content-addressed storage over a block store
category: architecture
audience: architect
last_updated: 2026-11-10
-->

# ADR 0001: Content-addressed storage over a block store

This decision record explains why every chunk is stored as its own immutable, hash-named blob rather than inside per-file block containers. It is the foundation decision of the storage engine; chunking ([ADR 0002](./0002-fastcdc-over-fixed-chunking.md)) and the metadata design build directly on it.

## Overview

The block store is a flat map from 32-byte hash to blob bytes on the local filesystem, sharded by hash prefix. Nothing else — no per-file grouping, no offsets, no headers — lives in the blob layer; all structure lives in the redb index. Choosing content addressing made dedup, integrity checking, and replication consequences of the design rather than features bolted on top.

## Status

**Accepted** — 2026-11-10.

## Context

The service is a hybrid chunked store: files are split into content-defined chunks (32 KiB average), and the dominant workloads are duplicate-heavy (repeated uploads, nightly backups, version churn, many similar VM images). The blob layer had to satisfy, in combination:

- **Cross-file dedup** — identical chunks appearing in different files (or different versions of one file) must occupy storage once.
- **End-to-end integrity** — bit rot, partial writes, and replication bugs must be detectable without external tooling.
- **Crash safety** — a kill at any instant must never leave the system referring to data that was never durably written.
- **Simple replication** — the replica must be able to converge with the primary without a locking protocol.

## Decision

Chunks are stored as individual immutable blobs keyed by their content hash, at `<storage.root>/<ab>/<cd>/<64-hex>.bin` (shard depth configurable 0–4, default 2). Writing is atomic — unique temp file, fsync, rename, directory fsync — and idempotent: writing an existing hash returns `false` without modifying anything. The index side records per-hash metadata (`refcount`, `blob_size`, `on_disk_path`) in the redb `chunks` table, and manifests reference blobs purely by hash.

## How it works

- `src/storage/block_store.rs` implements `write` / `read` / `delete` / `list_all` over the sharded tree; `list_all` walks to `shard_depth + 1` levels for the orphan sweeper.
- The upload pipeline in `src/core/pipeline.rs` writes every blob **before** the single index transaction that registers its refcount, so a crash leaves at most an orphan blob, never a dangling reference.
- The retrieval engine (`src/storage/retrieval.rs`) re-hashes every disk-loaded chunk with BLAKE3 and compares it to the manifest's `plain_hash`; a mismatch aborts the read with `HashMismatch`/`Corruption` and increments `file_storage_hash_mismatch_total`.
- Replication ships blobs by hash; a replica missing a blob simply pulls it from upstream by hash. Because blobs are immutable, there is nothing to invalidate.

## Consequences

**Positive**

- **Global dedup for free**: identical plaintext chunks (or tenant-scoped derivations under encryption, per [ADR 0007](./0007-encrypted-dedup-key-derivation.md)) collapse to one blob and a refcount bump.
- **Integrity self-check**: the blob's name is its content, so verification needs no checksum sidecar — read back, hash, compare.
- **Immutable blobs**: caches never need invalidation, replication is append-only, and same-name replacement never rewrites bytes.
- **Simple garbage collection**: refcount to zero means delete; anything on disk without an index row is an orphan.
- **Idempotent, replay-safe writes**: concurrent or repeated writes of one hash converge.

**Negative**

- **One small file per chunk**: hundreds of thousands of inodes for large datasets, mitigated by sharding (`storage.shard_depth`) and bounded by the chunk-size floor.
- **No partial blob updates or compaction**: storage changes only through create/delete; reorganising data means re-writing blobs through the pipeline.
- **GC must exist**: refcounting plus an orphan sweep is real machinery ([Garbage Collection](../gc.md)), not free.
- **fsync on the hot write path**: durability costs latency (measured as the dominant cost in the 119 MB/s HTTP upload figure on the benchmark rig).

## Alternatives considered

- **Per-file block containers** (one file per stored file, chunked internally with local offsets). Chosen against because identical chunks across files would duplicate storage unless a shared container format were invented; containers also make same-name replacement a rewrite of a large file, and replication would need container-level delta logic instead of shipping blobs by hash.
- **External object store (S3 and friends)** as the blob layer. Adds a network dependency and per-request latency to a component that is local by design; also breaks the single-process, single-volume deployment model. Rejected for the core; a bolt-on remote tier remains a possible future backend.
- **Key-value store for blob bytes** (blob bytes inside redb or another KV). Mixes large-value IO with metadata IO in one file, hurting both; keeps the atomicity story murkier. Rejected.

## Usage

Operational knobs for this decision:

| Key | Default | Effect |
|---|---|---|
| `storage.root` | `data/blocks` | Root of the blob tree |
| `storage.shard_depth` | `2` | Directory fan-out (0–4) |
| `storage.temp_dir` | `data/tmp` | Spool for multipart bodies (blobs themselves temp inside their shard dir) |

## Examples

The layout and its idempotency contract:

```rust
// blob at data/blocks/3f/9a/3f9a4b17….bin — name == BLAKE3 (or tenant-scoped derivation)
let first  = blocks.write(&hash, b"payload").await?; // true  — created
let second = blocks.write(&hash, b"payload").await?; // false — already present
```

Re-uploading the same 512 KiB file therefore reports `dedup_chunks = chunk_count` while writing zero new blob bytes.

## See also

- [Storage Layout](../storage-layout.md) — the tree and index encodings in full
- [Chunking & Dedup](../chunking.md) — how chunk boundaries create the dedup opportunity
- [ADR 0002 — FastCDC](./0002-fastcdc-over-fixed-chunking.md) — the chunking decision feeding this one
- [ADR 0003 — redb](./0003-redb-over-sled.md) — the metadata side of the split
- [Garbage Collection](../gc.md) — how un-referenced blobs are reclaimed
- [Benchmarks](../../performance/benchmarks.md) — the fsync-dominated upload numbers
