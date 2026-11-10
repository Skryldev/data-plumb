<!--
title: ADR 0004 — moka cache hierarchy
category: architecture
audience: architect
last_updated: 2026-11-10
-->

# ADR 0004: moka cache hierarchy

This decision record explains why the read path uses three independent moka caches with different value shapes, budgets, and TTLs — instead of one general-purpose cache. It also records why `moka` was chosen over `lru` and `cached`.

## Overview

The retrieval path has three distinct hot values: decrypted plaintext ready to stream, raw stored blobs ready to decode, and file manifests that route every request. Collapsing them into one cache would force a single TTL and eviction policy onto incompatible shapes. The service therefore runs three `moka::future::Cache` instances — L1 plaintext (256 MiB / 300 s), L2 blobs (512 MiB / 3600 s), L3 manifests (10 000 entries / 60 s) — built once in `src/cache/` and shared by both listeners through `AppState`.

## Status

**Accepted** — 2026-11-10.

## Context

Cache requirements by tier differ in every dimension:

- **L1 (plaintext chunks)** — values are decompressed/decrypted bytes that took real CPU to produce; correctness requires that only verified plaintext be cached; the right budget is bytes, since chunk sizes vary 8 KiB–128 KiB; a short TTL bounds stale-plaintext risk after hypothetical corruption events and keeps memory reclaimable.
- **L2 (stored blobs)** — values are compressed/encrypted blobs as written to disk; they are immutable, so a long TTL is safe; budget is again bytes; this is the tier read-ahead warms, because decode must stay on the hot path.
- **L3 (manifests)** — values are `Arc<Manifest>` routing records; budget is naturally entry count; the TTL must be short because soft-deletes must become visible promptly, even where explicit invalidation cannot reach (for example replicas learning about deletes lazily).

Concurrency matters as much as policy: hundreds of concurrent range requests may need the same blob, and thundering-herd disk loads would defeat the cache entirely.

## Decision

Adopt `moka` 0.12 (future feature) and build three caches in `src/cache/chunk_cache.rs` and `src/cache/manifest_cache.rs`:

```rust
let plain = Cache::builder()
    .max_capacity(cfg.l1_max_bytes)                       // 268435456
    .weigher(|_k, v: &Bytes| (v.len() as u32).max(1))     // byte weigher
    .time_to_live(Duration::from_secs(cfg.l1_ttl_secs))   // 300
    .build();
```
*Figure 1: The L1 builder. L2 is identical with `l2_*` values; L3 drops the weigher and caps by `manifest_capacity`.*

The blob tier exposes `get_or_load_blob`, which consults L2 and, on miss, loads via `try_get_with` — moka's single-flight primitive, so concurrent requests for one hash share one disk load. Load errors are propagated and never cached. L1 insertion happens only after BLAKE3 verification of the decoded plaintext.

## How it works

- **Read order** — L1 → L2 → disk, then decode and verify, then insert into L1 (the raw blob is already in L2 via the loader). See [Caching](../caching.md) for the full path and hit accounting (`file_storage_cache_hits_total{level}`).
- **Read-ahead** — the retrieval engine prefetches chunks N+1..N+4 into L2 only (`peek_blob` skips already-cached entries; tasks time out after 10 s), deliberately leaving decode work off the prefetch path.
- **Invalidation** — deletes and same-name replacement invalidate affected manifest entries explicitly; the 60 s L3 TTL bounds staleness everywhere else.
- **Diagnostics** — `stats()` runs moka's pending tasks and reports per-tier entry counts and weighted bytes.

## Consequences

**Positive**

- **Per-tier tuning**: bytes-versus-entries, long-versus-short TTLs, and different eviction pressures are independent, so each tier can be sized for its workload instead of compromising.
- **Warm-read performance**: measured 1107 MB/s warm downloads versus 399 MB/s cold; an in-process 2 MiB retrieval on L1 hits costs ~29 µs ([Benchmarks](../../performance/benchmarks.md)).
- **Single-flight loads**: `try_get_with` turns concurrent misses into one disk read, protecting disk and index under hot ranges.
- **Verified-only L1**: integrity checking cannot be bypassed by the cache; a corrupt disk load fails before it is cached.
- **Concurrency-native**: moka's future API matches the tokio stack of both listeners without locks around the cache.

**Negative**

- **Three knobs to tune** (six config keys), and a misconfigured pair of budgets can over-commit memory: defaults add up to ~768 MiB before manifests.
- **No cross-process coherence**: caches are per-process; a replica repaired by pull-repair cannot warm a primary's cache, and TTLs (not invalidation) bound staleness between nodes.
- **No persistence**: restarts start cold; the first requests after a deploy always hit disk.
- **TinyLFU admission is probabilistic**: occasionally-evicted-then-readmitted entries cause minor variance in hit rates, which complicates naive hit-rate alerting.

## Alternatives considered

- **Single shared cache** — one TTL and one weigher for plaintext, blobs, and manifests. Rejected: the manifest TTL must be short for delete visibility, while blob TTLs want to be long; a single policy either wastes L2 or risks serving from stale manifests.
- **`lru` crate** — simple and popular, but synchronous and lock-based, with no byte weighers, TTLs, or single-flight loads; wrapping it in async primitives would reintroduce the bugs moka already solved. Rejected.
- **`cached` crate** — function-memoisation macros with TTL support, but limited control over per-entry weighing and no `try_get_with`-style concurrent-load collapsing for async functions. Rejected for the byte-weighted tiers.
- **No cache** — every read pays disk IO plus BLAKE3 verification plus decode. Rejected: the measured cold-versus-warm gap (399 vs 1107 MB/s) makes this a 2.8× regression on read-heavy workloads.

## Usage

| Key | Default | Controls |
|---|---|---|
| `cache.l1_max_bytes` | `268435456` | L1 plaintext byte budget |
| `cache.l1_ttl_secs` | `300` | L1 entry lifetime |
| `cache.l2_max_bytes` | `536870912` | L2 blob byte budget |
| `cache.l2_ttl_secs` | `3600` | L2 entry lifetime |
| `cache.manifest_capacity` | `10000` | L3 entry cap |
| `cache.manifest_ttl_secs` | `60` | L3 entry lifetime |

Sizing recipes and the interaction with `PIPELINE_CONCURRENCY` are in [Tuning](../../performance/tuning.md); hit-rate dashboards in [Monitoring](../../operations/monitoring.md).

## Examples

Observe tier behaviour under a repeated range read (second request should hit L1):

```bash
curl -s localhost:8080/metrics | grep cache_hits_total
# file_storage_cache_hits_total{level="l1"} 1042
# file_storage_cache_hits_total{level="l2"} 87
```

The cache types as exposed to the rest of the system:

```rust
pub struct CacheLayer {
    pub chunks: Arc<ChunkCache>,       // L1 plain + L2 blob
    pub manifests: Arc<ManifestCache>, // L3
}
```

## See also

- [Caching](../caching.md) — the tiers, read-ahead, and hit accounting in depth
- [Data Flow](../data-flow.md) — cache misses in the retrieval sequence diagrams
- [Storage Layout](../storage-layout.md) — what L2 caches on disk
- [ADR 0003 — redb](./0003-redb-over-sled.md) — the L3-miss fallback store
- [Benchmarks](../../performance/benchmarks.md) — cold/warm numbers
- [Tuning](../../performance/tuning.md) — budget sizing
