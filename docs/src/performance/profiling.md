<!-- title: Profiling category: performance audience: developer last_updated: 2026-11-10 -->

# Profiling

How to attribute time when a benchmark regresses or production latency climbs: criterion for repeatable comparisons, `perf` and flamegraphs for the running binary, `strace` for syscall-level costs such as fsync, and an honest note on runtime and allocator tooling that is *not* wired in by default. The pipeline stages you will see in every profile run chunking → BLAKE3 hashing → zstd → fdatasync. Measurement baselines live in [Benchmarks](./benchmarks.md); the knobs you would turn afterwards in [Tuning](./tuning.md).

## Overview

The upload path is sequential through chunking and hashing, concurrent (8 tasks) through compression, encryption and blob writes, and ends in exactly one redb transaction per file. The download path is L1 → L2 → disk with BLAKE3 verification after each disk load. Each stage has a distinct profile signature, so a flamegraph usually identifies the stage before you read a single line of code.

```text
upload:  FastCDC ──> BLAKE3 ──> zstd ──> AES-GCM (opt.) ──> blob tmp+rename+fsync ──> redb txn (1/file)
         sequential  sequential  8-way concurrent (compress/encrypt/write)
download: L1 hit ─┐
          L2 hit ─┼─> assemble ──> serve (disk loads are BLAKE3-verified; N+1..N+4 read-ahead into L2)
          disk  ─┘
```

*Figure 1: Where profile samples will accumulate on each path.*

## How it works

### Criterion deep dive

```bash
cargo bench --bench throughput            # runs cdc_chunk_4mb, zstd_compress_128kb,
                                          # upload_pipeline/upload_2mb, retrieval/download_2mb_cold|warm
cargo bench --bench throughput -- --save-baseline pre
cargo bench --bench throughput -- --baseline pre    # after your change
```

HTML reports with per-iteration distributions are written under `target/criterion/report/index.html`. The pipeline benches use `sample_size(10)` (each iteration commits redb transactions), so trust only changes outside the ±3–5% noise band; the pure chunking/compression benches are far tighter.

### perf on the release binary

```bash
# kernel.perf_event_paranoid <= 1 and access to /proc/sys/kernel/kptr_restrict
# (in containers: run with --cap-add=PERFMON or --privileged for sampling)
perf record -F 99 -g -- target/release/file-storage --config config/default.yaml
# drive load in another terminal (bench, perf_smoke, or curl loop)
perf report -g --no-children              # then: perf report --sort=dso,symbol
perf stat -e cycles,instructions,cache-misses,branch-misses -p $(pgrep file-storage)
```

The release profile builds with `strip=symbols`; for readable stacks either profile the binary before stripping in your CI artefact or build locally with `debug = true` added to `[profile.release]`.

### Flamegraphs

```bash
cargo install cargo-flamegraph
cargo flamegraph --release --bin file-storage -- --config config/default.yaml
# drive load, Ctrl-C; opens flamegraph.svg
```

### strace for fsync dominance

```bash
strace -c -f -e trace=fsync,fdatasync,rename -p $(pgrep file-storage)
# drive one upload, then read the call counts and time share
```

If `fdatasync`/`fsync` dominates `% time`, the upload is durability-bound — that is the intended design (blobs fsync before the index commit), not a bug.

### Runtime and heap tooling — optional, not shipped

Two tools require instrumentation that the shipped binary does not include; document honestly before promising them:

- **tokio-console** needs the `console-subscriber` installed as the tracing subscriber plus the `tokio_unstable` cfg at build time. There is no subscriber wiring in this codebase; if task stalls are your target, add the wiring in a private build, run `tokio-console`, and keep that build out of production.
- **jemalloc heap profiling** is not linked by default (the binary uses the system allocator). To heap-profile, link `jemallocator` in a private build or `LD_PRELOAD` libjemalloc with `MALLOC_CONF=prof:true,lg_prof_sample:19`, then read the output with `jeprof`.

## Usage

### Worked example: fsync dominance in upload throughput

The benchmark table shows upload via HTTP at **119 MB/s** (64 MiB, incompressible) versus **273 MB/s** for the in-process pipeline (2 MiB). Profiling explains the gap:

1. `strace -c -f -e trace=fsync,fdatasync,rename` during an HTTP upload shows one `fdatasync` per blob write (tmp + rename + dir fsync per chunk) plus the redb commit — the syscall share of wall time is the majority.
2. The pure benches bound the CPU stages: chunking ~1134 MB/s and zstd ~1824 MB/s are both far above the observed 119 MB/s, so CPU is not the constraint.
3. The in-process number (273 MB/s) shares the same fsyncs but drops HTTP body handling and serves a smaller file with better cache locality; the difference between 119 and 273 is dominated by syscall and HTTP overhead on the same 2 vCPUs, not by a config knob.
4. Conclusion path: on this hardware the only meaningful lever is faster storage (or accepting durability as-is — see [Tuning](./tuning.md)); raising compression level or concurrency will not move the number, which a quick `--baseline` bench run confirms.

### Interpreting hotspots by stage

| Signature in profile | Stage | Usual levers |
|---|---|---|
| Rolling-hash / boundary search heavy | Chunking (FastCDC) | Larger `chunking.avg` (fewer boundary checks) — see [Chunking](../architecture/chunking.md) |
| Wide zstd frames, compression.level high | Compression | Drop `compression.level`; widen `compression.min_size` for incompressible content |
| `fdatasync`/`fsync` dominant | Blob write + redb commit | Faster storage; `PIPELINE_CONCURRENCY` overlap; nothing else (durability is fixed) |
| BLAKE3 wide but cold downloads slow | Disk read path | Cache sizing (`l2_max_bytes`), read-ahead already fixed at N+1..N+4 |
| Lock/syscall contention on redb | Index commits | Fewer, larger uploads (batching at the client); not tunable per request |

## Examples

A production-safe low-overhead snapshot of a live process:

```bash
perf stat -p $(pgrep -f 'file-storage') -- sleep 30   # IPC and cache-miss ratio only
strace -c -f -p $(pgrep -f 'file-storage') &          # syscall mix (attach briefly)
```

Then correlate with metrics from [Monitoring](../operations/monitoring.md): rising `upload_duration_seconds` P99 with flat CPU points at storage; rising CPU with flat latency points at compression settings.

## See also

- [Benchmarks](./benchmarks.md) — the numbers and baselines this page explains how to dissect.
- [Tuning](./tuning.md) — the configuration levers for each hotspot.
- [Chunking](../architecture/chunking.md) — FastCDC internals.
- [Caching](../architecture/caching.md) — cache levels and read-ahead behaviour.
- [Configuration reference](../configuration/reference.md) — keys mentioned above with defaults.
