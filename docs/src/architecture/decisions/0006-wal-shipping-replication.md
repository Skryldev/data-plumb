<!--
title: ADR 0006 — WAL-style replication shipping
category: architecture
audience: architect
last_updated: 2026-11-10
-->

# ADR 0006: WAL-style replication shipping

This decision record explains why replication is an in-memory bounded queue drained by a batched background worker, rather than a durable on-disk journal, a per-operation call stream, or an external log broker. It also records the durability trade this buys — and the bounded loss window operators must understand.

## Overview

Every upload enqueues idempotent operations (chunk blobs, then the manifest) into a bounded in-memory channel; a worker ships batches of up to 64 ops or every 5 seconds to the replica over `POST /internal/batch`, retrying with a 100 ms → 500 ms → 2 s backoff ladder. A `Flush` sentinel with a oneshot reply turns the same queue into the sync-mode barrier that lets uploads wait for replica acknowledgement.

## Status

**Accepted** — 2026-11-10.

## Context

The replication design had to fit the deployment model: one primary, one replica target, shared-secret authenticated HTTP between them, and no external infrastructure. Additional constraints:

- **Ordering per file** — a replica must never apply a manifest whose chunk blobs have not arrived.
- **Idempotent ops** — retries after timeouts and crashes must be safe without a dedup protocol.
- **Bounded memory** — a stalled replica must not let the queue grow without limit; backpressure should propagate to uploads.
- **Low upload-path overhead** — enqueueing must be cheaper than the upload itself; the queue sits after the local commit.
- **A durability escape hatch** — some deployments want the upload to wait for replica ack; the mechanism should be a mode, not a second implementation.

## Decision

Implement a WAL-*style* shipper in `src/replication/replicator.rs`:

- An **mpsc channel** of capacity `replication.queue_capacity` (10 000) holds `Op::Chunk`, `Op::Manifest`, `Op::Purge`, and `Op::Flush{reply}`. Sends await a slot when the queue is full — backpressure by design.
- A **single worker task** drains opportunistically, ships when it holds `batch_size` (64) ops or after `flush_interval_ms` (5000) of lingering, and retries failed batches after 100 ms, then 500 ms, then 2 s (held at 2 s until recovery).
- **Transport**: the built-in shipper POSTs to the replica's `/internal/batch` endpoint with the compact frame codec (`u32 count`, then per chunk `hash(32) | plain_hash(32) | u64 size_original | u64 size_stored | u8 flags | u32 blob_len | blob`; manifests as length-prefixed MessagePack), authenticated by the `X-Api-Key` header. The gRPC `ReplicationService` (`ReplicateBatch` bidi stream, `PullChunk`, `PullManifest`) exposes the same operations for external shippers, authenticated by `x-api-key` metadata.
- **Sync mode**: `Op::Flush` carries a oneshot sender; the worker replies when every previously queued op is applied. Uploads in `replication.mode: sync` await this reply, bounded by `sync_timeout_ms` (5000); a timeout fails the upload with `Unavailable` (503 / gRPC `UNAVAILABLE`).

## How it works

```text
upload commit ──► enqueue Chunk(op)* ──► enqueue Manifest ──► [sync: wait_flushed]
                     |                              |
                     v                              v
        queue (mpsc, cap 10 000, backpressure on send)
                     |
                     v
        worker: drain ≤ 64 ops or 5000 ms linger
                     |
                     v
        POST /internal/batch (chunks first, then manifest)
                     |
             ok ─────┴───── fail ──► backoff 100 ms → 500 ms → 2 s → retry batch
```
*Figure 1: The shipper loop. Order is preserved by the channel; ops are idempotent, so a retried batch re-writes blobs as no-ops.*

Observability comes from `file_storage_replication_queue_depth` (gauge), `replication_failures_total`, `replication_ops_applied_total`, and `replication_flush_seconds{status}`.

## Consequences

**Positive**

- **Zero-disk queue**: enqueueing costs an atomic counter increment and a channel send — no fsync on the replication path, so uploads are never slowed by replication bookkeeping.
- **Batching amortises HTTP overhead**: 64 ops per request keeps per-chunk latency negligible on LANs.
- **Bounded memory with graceful degradation**: 10 000 ops is the worst-case buffer; beyond that, uploads slow down instead of the process OOMing.
- **Replay safety**: idempotent ops mean a retried or duplicated batch converges — the replica does the same writes twice and the second is a no-op.
- **One mechanism, two modes**: the `Flush` sentinel reuses the queue for sync mode instead of forking a second code path.
- **Crash residue is reconcilable**: the replica's pull-repair on read misses (`upstream_url`) and its reachability GC close whatever gaps the lost queue tail left.

**Negative**

- **Un-flushed ops are lost on a primary crash**: the queue is memory only, so async mode is eventual consistency with a bounded loss window (at most the queue contents; typically the last few seconds of writes). This is the core trade of the design — sync mode closes it at the cost of upload latency.
- **No fan-out**: exactly one `target_url`; multi-replica topologies need chained replicas or an external shipper speaking the gRPC protocol.
- **Queue depth is not observable across restarts**: the gauge resets; after a crash, reconciliation is pull-based, not replay-based.
- **Backoff caps reactivity**: while the replica is down, retries pause up to 2 s; recovery drains at batch speed, so large gaps take many batch windows.

## Alternatives considered

- **On-disk queue file (real WAL)** — fsyncing a journal per enqueue would survive primary crashes and enable true replay; rejected for v1 because it adds fsyncs to the hot upload path, plus segment-rotation and compaction machinery, for a benefit that only matters in async mode (sync-mode users already get acked durability). Revisit if async durability requirements harden.
- **Direct gRPC stream per operation** — one RPC per chunk is chatty (thousands of round trips per large upload) and forfeits batch acknowledgment semantics; rejected. The bidi `ReplicateBatch` RPC exists for external shippers that want streaming, but the built-in shipper batches.
- **Kafka / NATS as the op log** — solves durability and fan-out elegantly, but imports a broker cluster, its operational burden, and an availability dependency into a single-replica design; the service would no longer run from one binary and one data directory. Rejected for now.
- **Outbox table in redb** — durable ops inside the existing index transactions would give replayable replication without a new component; rejected for v1 because it couples replication throughput to the metadata store's write path and complicates GC of applied entries. A plausible future upgrade path.

## Usage

| Key | Default | Effect |
|---|---|---|
| `replication.enabled` | `false` | Spawns the worker and client |
| `replication.mode` | `async` | `sync` enables the flush barrier per upload |
| `replication.target_url` | `null` | Replica base URL (REST transport) |
| `replication.upstream_url` | `null` | Replica-side pull-repair source |
| `replication.api_key` | — | Shared secret, both transports |
| `replication.queue_capacity` | `10000` | Backpressure threshold |
| `replication.batch_size` | `64` | Ops per shipped batch |
| `replication.flush_interval_ms` | `5000` | Max linger of a partial batch |
| `replication.sync_timeout_ms` | `5000` | Sync-mode barrier budget |

Runbook procedures (failover, draining, reconciliation) are in [Replication Ops](../../operations/replication-ops.md).

## Examples

Watch the shipper's health:

```bash
curl -s localhost:8080/metrics | grep replication
# file_storage_replication_queue_depth 0
# file_storage_replication_failures_total 0
# file_storage_replication_ops_applied_total 14213
# file_storage_replication_flush_seconds_count{status="ok"} 218
```

The gRPC equivalent of a batch, for custom shippers:

```bash
grpcurl -plaintext -H "x-api-key: $KEY" \
  -d '{"put_chunk": {"hash": "…32 bytes…", "data": "…", "compressed": true}}' \
  localhost:9091 file_storage.v1.ReplicationService/ReplicateBatch
```

## See also

- [Replication](../replication.md) — the full replication architecture
- [Data Flow](../data-flow.md) — where ops enter the queue in the upload sequence
- [Garbage Collection](../gc.md) — purge propagation into the same queue
- [gRPC API](../../api/grpc.md) — `ReplicationService` contract details
- [Replication Ops](../../operations/replication-ops.md) — operational procedures
