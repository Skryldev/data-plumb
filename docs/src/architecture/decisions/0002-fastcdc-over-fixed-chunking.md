<!--
title: ADR 0002 — FastCDC content-defined chunking
category: architecture
audience: architect
last_updated: 2026-11-10
-->

# ADR 0002: FastCDC over fixed chunking

This decision record explains why the service cuts data with the FastCDC 2020 algorithm instead of fixed-size splits. It is the reason a one-byte edit to a gigabyte file re-uploads kilobytes rather than re-uploading the file.

## Overview

Dedup quality is determined before hashing ever runs: it depends on where chunk boundaries land. Fixed-size boundaries are cheap but hostile to edits; content-defined boundaries move with the data. The service uses `fastcdc` 5.x's `v2020` module (gear-hash based, tokio-aware) with default bounds of 8 KiB / 32 KiB / 128 KiB, achieving a measured ~1.1 GB/s pure chunking rate.

## Status

**Accepted** — 2026-11-10.

## Context

The target workloads — backups, VM images, media, repeated uploads — share one property: successive versions differ by small insertions at arbitrary offsets. The chunking layer must therefore:

- **Survive edits** — inserting or deleting *B* bytes must disturb only the local neighbourhood of chunks, not every chunk after the edit point.
- **Be transport-agnostic** — the same bytes arriving over different transports (multipart spooling, raw HTTP streaming, gRPC 1 MiB frames) must produce identical chunk sequences, or dedup would depend on packetisation.
- **Stream with bounded memory** — chunking happens while the request body is still arriving; nothing may require buffering whole files.
- **Be fast** — chunking sits in the hottest part of the upload pipeline ahead of BLAKE3 and zstd.

## Decision

Adopt the FastCDC 2020 algorithm via the `fastcdc` crate (version 5, `tokio` feature), configured by `chunking.min` (8192), `chunking.avg` (32768), `chunking.max` (131072) — with the validated constraint `min ≤ avg ≤ max` and a hard cap of 16 MiB on `max`. The async wrapper `chunk_stream` in `src/core/chunker.rs` feeds the body stream through an `AsyncRead` adapter into `fastcdc::v2020::AsyncStreamCDC`, which maintains its own rolling carry buffer; the pipeline then processes chunks through the 8-wide concurrent stage.

## How it works

- The gear hash slides a window over the stream; a cut is declared when the rolling hash hits a mask derived from `avg`, never before `min`, and forcibly at `max`.
- Because `AsyncStreamCDC` buffers across stream items, cut points depend only on content: unit tests feed the same pseudo-random 2 MB payload in 1000-byte and 250 KB pieces and assert identical BLAKE3 hash sequences (`boundaries_independent_of_packetisation`), and compare streamed output against the synchronous single-pass scanner (`matches_single_pass_scanner`).
- Tests also assert the bounds (`respects_min_and_max`), exact reassembly at odd packet sizes (997 / 4096 / 65533), single-chunk behaviour for tiny inputs, and empty-input no-ops.
- Upstream stream errors abort the scanner and surface as the original `StorageError` variant, recovered from the `io::Error` boundary.

```text
payload:  |-------A-------||---B---|  insert 4 KB at offset p
fixed 64 KiB:  every boundary after p shifts  ->  (size / 64 KiB) chunks change
FastCDC 32 KiB: re-synchronises near p        ->  ~B/32 KiB + 2 chunks change
```
*Figure 1: Edit locality. The gear hash re-anchors boundaries shortly after the disturbed region, which is what preserves dedup for versioned content.*

## Consequences

**Positive**

- **Boundary stability under edits**: shifted content dedups at the expected `B/avg + 2` disturbed-chunk rate instead of losing everything downstream of an edit.
- **Transport-independent dedup**: identical bytes uploaded via multipart, raw HTTP, or gRPC yield identical chunk sets and dedup against each other.
- **~GB/s throughput**: measured 1134 MB/s pure chunking on the benchmark rig (4 MiB input, default bounds); see [Benchmarks](../../performance/benchmarks.md).
- **Streaming by construction**: constant memory regardless of upload size (up to the 10 GiB limit).
- **Tunable**: `chunking.min/avg/max` let operators trade dedup granularity against metadata overhead ([Chunking & Dedup](../chunking.md) has the tuning table).

**Negative**

- **CPU cost versus `memcpy` splits**: rolling-hash work is real, though hidden behind the compress/encrypt stages' concurrency.
- **Chunk-size variance**: content-defined cuts produce a distribution around `avg`, so manifests and the `chunks` table hold more, smaller entries than a fixed scheme at the same average.
- **Dedup is exact-match only**: near-duplicate content (recompressed media) dedups poorly; CDC improves edit locality, not fuzzy similarity.
- **Encryption interplay**: boundaries dedup fine, but ciphertext-addressed storage is impossible with random nonces — resolved separately in [ADR 0007](./0007-encrypted-dedup-key-derivation.md).

## Alternatives considered

- **Fixed 64 KiB chunking** — trivially fast and simple, but edit-hostile: one inserted byte shifts every subsequent boundary, so versioned files dedup to ~0%. Rejected as the primary scheme.
- **Rabin fingerprinting** — the classic content-defined approach; correct but slower (string polynomial rolling hash without the gear/normalisation refinements) and no better at boundary stability than FastCDC. Rejected on throughput.
- **Custom rolling hash** — a hand-rolled hash and mask scheme could shave CPU, but it abandons the peer-reviewed algorithm and the maintained, tested crate for marginal gain. Rejected on risk.
- **Larger fixed chunks plus whole-file dedup** — a hybrid where identical whole files (matched by `original_hash`) shortcut to refcount bumps. The pipeline already gets this behaviour for free: every chunk of a duplicate file dedups, and the same-name replacement path soft-deletes the old manifest. No separate mechanism needed.

## Usage

| Key | Default | Constraint |
|---|---|---|
| `chunking.min` | `8192` | `min ≤ avg` |
| `chunking.avg` | `32768` | Target cut distance |
| `chunking.max` | `131072` | `≤ 16777216` (16 MiB hard cap) |

Changing the values affects new uploads only; existing manifests reference their already-recorded chunk hashes, so reads are unaffected. Overrides: `FS_CHUNKING__MIN=4096 FS_CHUNKING__AVG=16384 FS_CHUNKING__MAX=65536`.

## Examples

Reproduce the packetisation-independence property outside the test suite:

```rust
use file_storage::core::chunker::chunk_slice;

let cfg = ChunkingConfig { min: 8192, avg: 32768, max: 131072 };
let one_pass = chunk_slice(&data, &cfg);
// hashing each chunk of `one_pass` equals hashing each chunk of the
// streamed version, regardless of how the body was packetised
```

A typical result on versioned content (from the upload report):

```json
{ "chunk_count": 2048, "dedup_chunks": 1984, "dedup_bytes": 63963136 }
```

## See also

- [Chunking & Dedup](../chunking.md) — algorithm mechanics, tuning table, and dedup math
- [ADR 0001 — CAS over block store](./0001-cas-over-block-store.md) — what chunk hashes address
- [ADR 0007 — Encrypted dedup key derivation](./0007-encrypted-dedup-key-derivation.md) — dedup under encryption
- [Data Flow](../data-flow.md) — chunking's place in the pipeline
- [Tuning](../../performance/tuning.md) — matching chunk profile to workload
