# INTERLEAVINGS — scheduler-order traces (audit evidence, commit e4cd5a6)

Frozen evidence from the 2026-11-11 audit. Each section names the interleaving
that breaks an invariant. The fixing commits annotate their section with
`fixed by <commit>`; inverted tests live under `tests/regression/`.

## C-02 — replica gRPC purge destroys shared blobs

1. Replica applies `PutChunk X` → `upsert_chunk_replica(X)` → row `refcount: 0` (by construction).
2. Replica applies `PutManifest A(X)` and `PutManifest B(X)` — both live, X shared.
3. Primary purges A; shipper forwards `DeleteFile{A, purge=true}`.
4. Replica `apply_delete_file`: `remove_manifest(A)` ✓ then `decr_refcounts([X])`
   → row exists with `refcount 0` → the `Some(_)` arm treats ≤1 as unreferenced → row removed, X returned.
5. `blocks.delete(X)` → **blob gone while B is live**.
6. Next reachability GC pass: B's manifest references a missing blob → B is treated as
   unreachable after its reads fail; the loss cascades.

The HTTP internal path (`DELETE /internal/manifest/:id`) never touches refcounts — the two
protocols disagree. Gate: `replication_grpc_purge_shared_chunk`.

## C-03 — GC/purge vs dedup-upload race (dangling manifest)

Actors: uploader U (dedup upload of chunk X, blob already on disk from old owner M),
purger P (removes M), GC tick G.

1. U: `blocks.write(X)` — dedup case: file exists, idempotent no-op; U holds compressed bytes.
2. P: `soft_delete_manifest(M)` → GC purges M (grace expired): `decr_refcounts([X])` → row removed (refcount ≤1) → `blocks.delete(X)` → blob unlinked.
3. U (stream ends, seconds to hours later): `register_chunks([X])` — **never verifies blob presence** → row re-created `refcount: 1`.
4. U: `put_manifest` → **201 Created**.
5. Every read of U's file: `blocks.read(X)` → NotFound → 404 forever (primaries have no repair path).

Window = the entire remaining upload stream (up to `max_file_size`). Variant C-03b: the orphan
sweeper (blobs without meta rows older than `orphan_min_age_secs`) deletes U's blob for uploads
streaming longer than that window. Gate: `race_gc_upload`, `race_orphan_sweep_upload`.

## H-01 — whole-file retention

Per uploaded file, `stored: BTreeMap<usize, StoredChunk>` accumulates `blob: Bytes` for EVERY
chunk until stream end; `payloads` clones them again for the replicator. With
`PIPELINE_CONCURRENCY = 8` the concurrency is bounded but retention is not: N concurrent uploads
× file size RSS. 4 × 6 GiB incompressible gRPC uploads at 1 MB/s → ~24 GiB RSS before any
request completes. Gate: `upload_memory_bound` + `budget_rejects_before_stream`.

## M-07 — repair poisoning

Replica read miss → `fetch_blob(upstream)` → `blocks.write(bytes)` **before any verification**.
Hostile/compromised primary (or MITM on the plaintext channel) returns wrong bytes under a valid
hash: poisoned blob persists on disk, L2 caches it for its TTL, `hash_mismatch` fires on every
read with no quarantine and no repair. Gate: `repair_rejects_lying_upstream`.

## M-11 — shipper failure modes

1. Peer down: retry loop backs off at 2 s forever — no breaker, no dead-letter.
2. Queue bounded by op count (10 000) not bytes: ~1.2 GiB RAM worst case.
3. Shutdown: `let _ = flush_batch(...)` — acked-but-unshipped ops vanish silently.
Gate: `breaker_opens_and_half_opens`, `shutdown_drops_are_counted_and_replayable`.

## M-14 — enqueue backpressure stalls committed uploads

`pipeline.rs` awaits `enqueue_chunk` per chunk AFTER `put_manifest`; `Replicator::send` awaits
`tx.send` when the queue is full with no timeout. Replica down + full queue ⇒ every new upload
hangs indefinitely after the manifest is durable (client sees a hung 201-less request).
Gate: `async_enqueue_timeout_drops_and_counts`.

## sync-mode durability verification (read before touching replication)

Sync mode: `wait_flushed()` after enqueue → per-op transitivity verified in the audit: an upload
returns only after its chunk blobs AND manifest were acked by the peer (batch order preserved by
the single mpsc queue). Any refactor (H-01 streaming enqueue, M-11 breaker) must preserve:
(a) chunk-before-manifest op ordering, (b) flush barrier covering ALL chunks of the upload,
(c) timeout + explicit error instead of unbounded wait.
