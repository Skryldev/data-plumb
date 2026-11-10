<!-- title: Benchmarks category: performance audience: developer last_updated: 2026-11-10 -->

# Benchmarks

The service ships two measurement paths: criterion micro/macro benchmarks for the pipeline internals (`benches/throughput.rs`) and an ignored end-to-end smoke test that produces HTTP-level throughput numbers. This page records the measured results with their environment caveats, explains how to reproduce them, and how to read criterion output when you change a hot path. Related knobs are in [Tuning](./tuning.md); finding where time goes is in [Profiling](./profiling.md).

## Overview

Measured results (release build, `opt-level=3`, thin LTO off in this measurement, 8-way chunk pipeline, zstd level 3). **Environment disclaimer:** 2-vCPU sandbox with an NVMe-backed overlayfs — treat the ratios between rows as meaningful and the absolute numbers as machine-dependent; re-run on your hardware before publishing comparisons.

| Metric | Measured | Note |
|---|---|---|
| CDC chunking (pure) | **1134 MB/s** | 4 MiB input, 8 KiB/32 KiB/128 KiB min/avg/max |
| zstd compress (pure) | **1824 MB/s** | 128 KiB blocks, level 3 |
| Upload pipeline (in-process, 2 MiB) | **273 MB/s** | chunk → hash → compress → blob fsync → redb txn |
| Upload via HTTP (64 MiB) | **119 MB/s** | random bytes (incompressible → stored raw); fsync-dominated |
| Download via HTTP, cold | **399 MB/s** | disk → verify → decompress |
| Download via HTTP, warm | **1107 MB/s** | L1 plaintext cache hits |
| Range (1 MiB slice) | **0.9 ms** | cross-chunk slice |
| Retrieval path (L1 hit, in-process) | ~29 µs / 2 MiB | `download_2mb_warm` criterion bench |

## How it works

The criterion file defines four benches: `cdc_chunk_4mb` (pure chunker on 4 MiB of pseudo-random data), `zstd_compress_128kb` (compressor at level 3 with `min_size` 1024), `upload_pipeline/upload_2mb` (a real `AppState` over a tempdir, streaming 2 MiB through `pipeline.process`) and `retrieval/download_2mb_cold` / `download_2mb_warm` (a seeded file streamed back through the retrieval path — the warm variant hits L1). The smoke test (`perf_smoke` in `tests/integration/perf.rs`) boots a real HTTP server, uploads 64 MiB of pseudo-random bytes, downloads it cold and warm, times a 1 MiB range read and prints dedup ratio from index statistics.

**Expected gRPC framing overhead:** every gRPC message carries a 5-byte length-prefixed frame plus protobuf tags, on top of HTTP/2 headers. For bulk transfers with 1 MiB pieces this amortises to well under 1%, but it is not zero — do not expect gRPC upload numbers to match raw HTTP body streaming byte-for-byte, and measure both if the transport matters to you.

## Usage

Run your own numbers:

```bash
# criterion pipeline/retrieval benches
cargo bench --bench throughput

# HTTP end-to-end smoke (ignored by default; prints MB/s and dedup ratio)
cargo test --release --test integration_test perf_smoke -- --ignored --nocapture
```

Interpreting criterion output:

```text
upload_pipeline/upload_2mb
                        time:   [7.25 ms 7.31 ms 7.38 ms]
                        change: [-3.1% -1.4% +0.2%] (p = 0.11 > 0.05)
                        No change in performance detected.
```

- The middle number is the median estimate; the brackets are the confidence interval.
- `change` compares against the previous run stored in `target/criterion` — regressions larger than the noise band (a few percent) are real.
- The `upload_pipeline` group uses `sample_size(10)` because each iteration commits redb transactions; expect wider intervals than the pure benches.
- Save and compare baselines across branches:
  ```bash
  cargo bench --bench throughput -- --save-baseline main
  # on your branch:
  cargo bench --bench throughput -- --baseline main
  ```

## Configuration

### Dedup measurement recipe

Dedup is verifiable in one round trip — re-upload the same 512 KiB file and check that every chunk was a dedup hit:

```bash
head -c 524288 /dev/urandom > /tmp/dup.bin
A=$(curl -s -X POST "localhost:8080/api/v1/files?name=dup.bin" \
  -H "Authorization: Bearer $TOKEN" --data-binary @/tmp/dup.bin)
B=$(curl -s -X POST "localhost:8080/api/v1/files?name=dup.bin" \
  -H "Authorization: Bearer $TOKEN" --data-binary @/tmp/dup.bin)
echo "$B" | jq '{chunk_count, dedup_chunks, dedup_bytes, size_stored}'
# dedup_chunks == chunk_count and size_stored stays flat: the second upload stored nothing.
```

Uniform content (all zeros, all one byte) collapses to a single chunk per hash — that is the pathological best case, not the realistic one; pseudo-random files give honest dedup numbers.

## Examples

Benchmark-to-optimisation loop, concretely:

1. Establish a baseline (`--save-baseline main`).
2. Make one change (for example raise `PIPELINE_CONCURRENCY` in the bench harness).
3. `cargo bench --bench throughput -- --baseline main` — read `change` per bench.
4. Confirm with the HTTP smoke test, since fsync effects dominate there; see the worked analysis in [Profiling](./profiling.md).

Expected relationships to sanity-check your environment: pure chunking and compression are CPU-bound and far above disk speed; the in-process pipeline sits below them because of blob fsyncs and the redb commit; the HTTP upload number is lowest because body handling, multipart spooling and per-chunk durability all sit on the same 2 vCPUs. Warm downloads at ~1.1 GB/s confirm the L1 cache is doing its job (see [Caching](../architecture/caching.md) for the cache hierarchy behind that number).

## See also

- [Tuning](./tuning.md) — every knob these benchmarks exercise.
- [Profiling](./profiling.md) — criterion deep dive, perf, flamegraphs, strace.
- [Chunking](../architecture/chunking.md) — why CDC throughput sets the ceiling.
- [Caching](../architecture/caching.md) — L1/L2/L3 sizing behind warm-read numbers.
- [Monitoring](../operations/monitoring.md) — production latency metrics that mirror these benches.
