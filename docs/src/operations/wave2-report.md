# Wave 2 Report — P1 Closure + CI Hardening (V5)

**Date:** 2026-11-12 · **Branch protocol:** one `fix/<ID>-<slug>` branch per
finding, `fix(<ID>):` commits, gate test per fix (red where the pre-fix
behavior was observable at runtime). Full contract table:
`REMEDIATION_REPORT.md` §"Wave 2".

## What landed

| Phase | Finding | Design | Status |
|---|---|---|---|
| W2-A | M-04 (validation unification) | executed V5 §3 / V4 §5.5 | **fixed** — 5 runtime-red gates → green |
| W2-B | M-07 (repair verify-then-persist) | executed V5 §4 / V4 §5.8 | **fixed** — 4 gates + quarantine units; bench recorded |
| W2-C | M-05 (streaming parity + login lockout) | executed V5 §5 / V4 §5.6 | **fixed** — 7 gates (3 runtime-red) |
| W2-D | M-01 (reconcile pass) | executed V5 §6 / V4 §5.3 | **fixed** — 6 gates |
| W2-E | CI-W1..CI-W5 | executed V5 §7 | **fixed** — boot gate + dashboard smoke verified live; size 11.85 MiB < 15 MiB |

Plus one **Wave-1 regression** caught and fixed mid-wave (see below).

## The four P1 fixes in one paragraph each

**M-04.** `src/core/validation.rs` is now the single source of truth for
upload-request shape: name 1..=255 bytes with C0/C1 banned, ≤32 tags of
1..=64 bytes (L-06), mime ≤128 bytes, `expected_size` capped at
`max_file_size` before admission control on both transports (REST gained
`?expected_size=`), and `include_deleted` admin-only with an explicit
`403`/`PERMISSION_DENIED` (the old REST silently honored it for viewers —
the audit's leak). GetManifest caps chunk lists at 1000 with the additive
`chunks_truncated` proto field (replication stays full-fidelity), and both
transports count list totals through one `IndexDb::list_filtered`. CI rule
`no-handler-validation` (grep gate, negative-tested) keeps the invariant
structural.

**M-07.** The replica repair path is verify-then-persist: pulled bytes are
UNTRUSTED and must decrypt/decompress and BLAKE3-match the chunk
reference's `plain_hash` before anything reaches the CAS or L2. Mismatches
are quarantined (`<root>/../quarantine/<hash>.bin`, size-bounded with
oldest-mtime eviction, boot-swept after the 7-day retention), the L2 entry
invalidated, `file_storage_repair_hash_mismatch_total{source="repair"}`
ticks and the caller sees `Corruption`. Ciphertext-only replicas (no
tenant key) keep the legacy order with a per-event warn and a dedicated
metric — documented as a degraded posture in the threat model. Gate tests
drive a REAL mock upstream over the replication wire protocol.

**M-05.** One per-tenant token-bucket governor lives in `AppState`; the
unary interceptor and the streaming Upload/Download handlers take tokens
from the same instance (pointer-identity gate + behavioral flood gate), so
the previously documented streaming bypass is closed. `LoginThrottle`
locks out usernames exponentially (1s base, ×2, 15m cap, default ON)
BEFORE the argon2 verify; only real credential failures grow the backoff,
verifier errors never do (rule 17 asserted), success resets, and every
throttled attempt is audited as `auth.login.throttled` with `retry_after`.
I-06's wholesale `clear()` is gone: governor and throttle share a
`BoundedMap` with insertion-order oldest eviction.

**M-01.** The reconcile pass (hourly by default, admin-triggerable on both
transports, audited) walks every manifest, classifies blobs/rows, and in
Repair mode tombstones rows with refcount>0 but zero manifest references —
decided inside ONE `Durability::Immediate` transaction that re-walks every
manifest (the C-03 race defense) — then reclaims them through the
C-03-safe victim sweep. Dry-run is read-only (rule 16, asserted). In-process
GC/reconcile mutual exclusion plus a cross-process lock file (1h
stale-steal, MSRV-safe). `file_storage_gc_reconcile_leaked_total` makes
leak rate observable; the README crash-safety sentence is now true.

## Wave-1 regression fixed mid-wave

`race_gc_upload_x100` (flaky ~1-in-3 full-suite runs under parallel load)
exposed a genuine residual C-03 interleaving: the Wave-1 sweep re-checked
references BEFORE the unlink, so an upload could register in the gap and
lose its blob under a live row. Fixed with a two-phase GC delete barrier
(finalize in-txn → atomic `.gc-pending` rename claim → resurrection
re-check restores → delete only when unreferenced), recorded as
`regression(W1-C-03)` in `REMEDIATION_REPORT.md`. Green ×5 consecutive
runs + full suite.

## Numbers

- Tests: **206 → 251 green** (0 failed), 2 ignored (nightly-only).
- clippy `-D warnings` clean · fmt clean · `cargo audit` 0 non-allowlisted
  advisories · `cargo deny check` clean.
- Bench (post-M-07, same 2-vCPU sandbox): cdc_chunk_4mb 1176 MB/s ·
  zstd_128kb 1878 MB/s · upload_2mb 253 MB/s · download_2mb_cold 29.1 µs —
  no cold-path regression (repair only fires on disk-miss-with-upstream).
- Binary (musl release): 12,421,616 B = 11.85 MiB (< 15 MiB guard).

## Deferred to V6 (ticketed, unchanged)

M-09 (route-scoped /internal body limit), M-10 (audit actor plumbing),
M-12 (replication supervisor), M-16 (tenant-scoped dedup keys), P2
(M-03, M-08, M-11, group-commit, smalls, WAL), P3. Reasons per row in
`REMEDIATION_REPORT.md`.

## Verification checklist (V5 §11 deltas)

- [x] Pre-flight green (206/0; clippy/fmt/audit/deny clean; git baseline
      deviation recorded: Wave-1 commits squashed to `f47c474` on session
      recycle — substance verified by the pre-flight suite).
- [x] M-04/M-07/M-05/M-01 rows marked fixed with gate lists.
- [x] CI-W1..CI-W5 landed; boot gate + dashboard smoke verified live
      locally; binary size verified.
- [x] Rules 15/16/17 asserted by tests (grep gate negative-tested;
      dry-run-never-deletes; verifier-error-does-not-throttle).
- [x] `audit-repro/` evidence preserved; PoCs remain expect-fail smokes.
- [x] Bench before/after recorded (M-07 perf-touching).
- [x] Re-audit scope: all CRITICAL/HIGH + M-01/M-04/M-05/M-07 closed —
      the surface is GO-ready for the V3 re-audit; remaining P1 (M-09/
      M-10/M-12/M-16) explicitly V6.
