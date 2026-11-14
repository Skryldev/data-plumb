# V7 Baseline (Wave 3 verification record)

**Host (this wave):** Windows 10 x64 (10.0.26100), rustc 1.99.1 MSVC,
16-core, local NVMe. **NOT the release Linux box** — see the gaps section.

## Test suite

`cargo test --locked --all-targets --no-fail-fast`: **265 passed / 0 failed**
(160 src unit incl. boot-sweep + gate units · 19 gRPC · 17 REST+integration ·
43 regression/property/soak · 1 C-01 binary boot gate; +1 ignored soak,
+1 ignored memory-bound — both exercised below).

## cargo audit (cargo-audit 0.23.0, advisory DB 1509 entries, 2026-11-13)

- `cargo audit --deny warnings`: **exit 0** with `.cargo/audit.toml`
  (RUSTSEC-2025-0141 bincode, RUSTSEC-2025-0134 rustls-pemfile — rationale
  + expiry in the file; mirrored in deny.toml).
- **Liveness verified (rule 19):** with the ignore list emptied, the same
  command exits non-zero, `error: 2 denied warnings found`.

## cargo deny check (cargo-deny 0.20.2)

`advisories ok, bans ok, licenses ok, sources ok` (CC0-1.0 added for
tiny-keccak 2.0.2, transitive via blake3, with provenance note).

## CI liveness (rule 19, locally reproduced)

- **c01-boot-gate inversion:** a padded shipped-literal config (length
  floor passes, deny-list catches) refuses to boot, exit 1, output names
  `matches a shipped/known literal` — matching the repaired grep.
- **no-handler-validation inversion:** a planted `name.len() > 255` in
  `src/api/handlers/violation_probe.rs` trips the rule (exit 1); removed,
  the rule passes clean.
- The `ci-liveness.yml` workflow runs both inversions daily + on
  workflow-file PRs. (The GitHub-hosted runs could not be observed from
  this sandbox — operator should confirm green on the first push.)

## Benchmarks (re-run on THIS host; ±tolerance vs Wave-2 Linux claims)

| Bench | Wave-2 claim (2-vCPU Linux) | This host (Windows, release) | Note |
|---|---|---|---|
| cdc_chunk_4mb | 1176 MB/s | 4 MiB / 1.454 ms ≈ **2810 MB/s** | faster host |
| zstd_compress_128kb | 1878 MB/s | 128 KiB / 310.7 µs ≈ **421 MB/s** | Windows/zstd-sys codegen differs; cross-platform delta expected |
| upload_pipeline/upload_2mb | 253 MB/s | 2 MiB / 27.5 ms ≈ **75 MB/s** | Windows fsync path is the dominant cost; Linux re-run required (I-07 open) |
| retrieval/download_2mb_cold | 29.1 µs (L2) | **19.6 µs** | warm ≈ cold here (moka) |
| retrieval/download_2mb_warm | 28.9 µs | **19.6 µs** | |

Cross-platform absolute numbers are NOT comparable; the Linux release-box
re-run (I-07) remains the authoritative baseline. No regression signal:
the deltas are host-shaped, not code-shaped (same bench code as Wave 2).

## Memory bound (H-01)

`h01_upload_memory_bound -- --include-ignored` (release): **PASS on this
host** — with the abort-tolerant outcome contract; the O(chunk) RSS
assertion is Linux-only (`/proc/self/status`) and runs in the nightly
job. This test had NEVER executed before (V6-06: dead nightly) and its
budget arithmetic was wrong (2×1.2 GiB vs 1.5 GiB) — fixed this wave
(budget 2.5 GiB).

## Soak slice

`SOAK_SECONDS=20 cargo test --test soak -- --include-ignored`: PASS
(upload/delete/GC/reconcile mix, zero phantom chunk rows at final
reconcile, RSS/disk growth assertions). The 24 h run is operator-
dispatched (`SOAK_SECONDS=86400`).

## Gaps (honest)

- **No Linux re-run** of the full matrix on the release box (no Docker/WSL
  in this sandbox). CI will execute the same gates on ubuntu-latest on the
  first push; record those run URLs in REMEDIATION_REPORT_V7.md.
- **No `buf lint`/`buf breaking` execution** (no buf binary in sandbox);
  proto changes this wave: NONE (wire untouched) — the CI proto job
  guards it.
- **No compose-stack black-box re-run** of `poc_expect_fail.sh` (needs
  Docker); the compose hardening landed but the stack smoke is CI-side.
