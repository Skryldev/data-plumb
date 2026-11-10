<!-- title: Tuning category: performance audience: all last_updated: 2026-11-10 -->

# Tuning

Every performance knob in the configuration, with its trade-off and a recommended starting value. The defaults (FastCDC 8 KiB/32 KiB/128 KiB, zstd level 3, 256 MiB L1 + 512 MiB L2 caches, shard depth 2, 8-way pipeline) are sensible for mixed workloads; deviate when your workload is measurably different, using [Benchmarks](./benchmarks.md) and [Profiling](./profiling.md) to justify the change. Full key reference: [Configuration reference](../configuration/reference.md).

## Overview

Tuning questions in priority order: (1) is the CPU or the disk the bottleneck for your content, (2) is dedup granularity right for your file shapes, (3) are your caches sized for your hot set, (4) are the durability and replication settings matched to your consistency needs. Config changes require a restart — there is no hot reload.

## How it works

### Chunking

| Key | Default | Trade-off |
|---|---|---|
| `chunking.min` | 8192 | Lower bound on chunk size; smaller min → finer boundaries → better dedup on edited documents, more chunks to index |
| `chunking.avg` | 32768 | Target size; the dominant lever on chunk count (a 1 GiB file ≈ 32k chunks at default avg) |
| `chunking.max` | 131072 | Hard cap; validation allows **max ≤ 16 MiB** and `min ≤ avg ≤ max` |

Constraint: `max ≤ 16 MiB` (validator-enforced). Smaller chunks mean better dedup granularity for repeatedly-edited files (FastCDC re-syncs boundaries near edits) but one `chunks` row and one `ChunkRef` per chunk — more redb bytes, larger manifests, more per-chunk work. Larger chunks shrink metadata and amortise fsyncs but dedup only on large identical regions.

### Compression

| Key | Default | Trade-off |
|---|---|---|
| `compression.level` | 3 (range 1..=22) | Each step up buys some ratio for real CPU; 3 keeps the compressor roughly matched to chunking throughput; 9+ is archive-grade, not ingest-grade |
| `compression.min_size` | 1024 | Chunks smaller than this are stored raw; also falls back to raw automatically when compression does not shrink — avoids wasting CPU on incompressible data |

### Caches

| Key | Default | Trade-off |
|---|---|---|
| `cache.l1_max_bytes` | 268435456 (256 MiB) | Plaintext chunks; bigger = more warm-read hits (up to ~1100 MB/s observed), direct RAM cost |
| `cache.l1_ttl_secs` | 300 | Eviction pressure relief; chunk content is immutable so TTL affects memory, not correctness |
| `cache.l2_max_bytes` | 536870912 (512 MiB) | Compressed blobs (pre-decrypt); the read-ahead target |
| `cache.l2_ttl_secs` | 3600 | Same immutability argument as L1 |
| `cache.manifest_capacity` | 10000 | L3 manifest entries; manifests are small, so this is cheap and rarely needs tuning |
| `cache.manifest_ttl_secs` | 60 | Manifest staleness window on listings/reads |

Read-ahead prefetches blobs of chunks N+1..N+4 into L2 only — sizing L2 matters most for sequential reads of large files.

### Storage layout and pipeline

| Key | Default | Trade-off |
|---|---|---|
| `storage.shard_depth` | 2 (range 0–4) | Blob path fan-out `ab/cd/<hash>.bin`; 0 puts every blob in one directory (directory-entry pressure at millions of files), higher depth adds path/stat cost per blob |
| `PIPELINE_CONCURRENCY` (env) | 8 | Concurrent compress/encrypt/write tasks; chunking and hashing stay sequential by design. More concurrency = better disk/CPU overlap, more memory |

**fsync trade-off — design decision.** Blobs are written tmp → rename → dir fsync, and the index transaction commits with `Durability::Immediate` (WAL + fsync). This is deliberately **not configurable**: it is what makes crash-consistent backups trivial (see Backup & Restore in the Operations chapter) and corruption detection honest. Upload throughput is fsync-dominated on slow storage (~119 MB/s vs 273 MB/s in-process on the benchmark sandbox); the lever is disk quality, not config. What *is* tunable is replication overhead: `batch_size`/`flush_interval_ms` control how much network work piggybacks per upload, and sync mode's cost is bounded by `sync_timeout_ms`.

### Limits and rate limiting

| Key | Default | Trade-off |
|---|---|---|
| `rate_limit.per_second` / `burst` | 500 / 1000 (per client IP, REST) | Burst must cover your clients' fan-out per interval; too small throttles legitimate bulk uploads, too large weakens the limiter. gRPC uses a per-tenant token bucket; streaming RPCs are not rate limited |
| gRPC message caps | 256 MiB max message, 4 MiB stream / 8 MiB connection windows | Raise client windows only with matching memory; the server values are fixed |

## Usage

Recommended starting values by workload:

| Knob | Mixed files (default) | Backup archive | Hot CDN-ish | VM images / containers |
|---|---|---|---|---|
| `chunking.avg` | 32768 | 262144 | 32768 | 32768 |
| `chunking.max` | 131072 | 4194304 | 131072 | 131072 |
| `compression.level` | 3 | 19 | 3 | 3 |
| `cache.l1_max_bytes` | 256 MiB | 64 MiB | 1 GiB | 256 MiB |
| `cache.l2_max_bytes` | 512 MiB | 256 MiB | 2 GiB | 512 MiB |
| `PIPELINE_CONCURRENCY` | 8 | 4 | 8 | 8 |

Rationale: archives optimise ratio and metadata volume (bigger chunks, high level, small caches since access is cold); hot read-heavy deployments buy RAM for L1/L2 — read-ahead is already 4 blobs and needs no config; VM images are exactly the workload where the default CDC shines (fixed 4 KiB/64 KiB chunks would resync nothing after a header edit).

## Examples

Applying a profile without editing the YAML — env overrides with `FS_` + `__` nesting:

```bash
FS_CHUNKING__AVG=262144 \
FS_CHUNKING__MAX=4194304 \
FS_COMPRESSION__LEVEL=19 \
FS_CACHE__L1_MAX_BYTES=67108864 \
FS_CACHE__L2_MAX_BYTES=268435456 \
file-storage --config /etc/file-storage/default.yaml
```

Then validate the change with the same method every time:

```bash
cargo test --release --test integration_test perf_smoke -- --ignored --nocapture
cargo bench --bench throughput
```

Compare against your saved baseline and keep the winning profile in config management; revert anything that wins in a microbenchmark but loses in the HTTP smoke test.

## See also

- [Benchmarks](./benchmarks.md) — how to measure before and after.
- [Profiling](./profiling.md) — attributing time when a knob does not help.
- [Chunking](../architecture/chunking.md) — FastCDC mechanics behind the chunking knobs.
- [Caching](../architecture/caching.md) — the three-level hierarchy being sized above.
- [Configuration reference](../configuration/reference.md) — the complete key list with defaults.
