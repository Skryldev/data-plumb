# CHANGELOG

All notable remediation changes. One line per finding ID (see
`REMEDIATION_REPORT.md` for the full contract table and
`audit-repro/` for the frozen evidence).

## 1.2.0 — Remediation wave 2 (2026-11-12)

Closes the four P1 deferrals Wave 1 nominated (M-01, M-04, M-05, M-07) plus
the CI-hardening workstream, with a gating test per fix. All fixes are
additive; no Wave-1 fail-closed default was softened.

### Fixed
- **M-04** — one validation layer for both transports (`core::validation`): name 1..=255 bytes + C0/C1 ban, ≤32 tags × 1..=64 bytes (closes **L-06**), mime ≤128 bytes, `expected_size` capped pre-admission on both transports (REST gains `?expected_size=`), `include_deleted` admin-only with explicit 403/PERMISSION_DENIED (M-04a), GetManifest chunk list capped at 1000 with the additive `chunks_truncated` proto field (M-04c), shared `IndexDb::list_filtered` counting code for REST+gRPC list totals (M-04d). CI rule `no-handler-validation` keeps the invariant structural (`fix(M-04)`).
- **M-07** — replica repair is verify-then-persist: pulled bytes are decrypted/decompressed and BLAKE3-checked against the chunk reference's `plain_hash` BEFORE any CAS/L2 write; mismatches go to a size-bounded, boot-swept `quarantine/` (outside the CAS walk), the L2 entry is invalidated, `file_storage_repair_hash_mismatch_total{source="repair"}` ticks and the caller gets `Corruption`. Ciphertext-only replicas (no tenant key) keep the legacy order with a loud warn + `repair_ciphertext_only_total` — documented degraded posture. Error variants now survive the L2 `try_get_with` boundary (`fix(M-07)`).
- **M-05** — ONE shared per-tenant token-bucket governor in `AppState`: unary interceptor AND streaming Upload/Download take tokens from the same instance (the documented streaming bypass is closed; `RESOURCE_EXHAUSTED` before any pipeline work). `LoginThrottle`: per-username exponential lockout (base 1s, ×2, cap 15m, default ON) checked BEFORE argon2; only `InvalidCredentials` grows the backoff, verifier errors stay fail-closed and never lock out (rule 17); throttled logins are `429` with `retry_after` and audited as `auth.login.throttled`. Closes **I-06**: the wholesale `buckets.clear()` is replaced by a shared bounded-map helper with oldest-first eviction used by both the governor and the throttle (`fix(M-05)`).
- **M-01** — drift-reconciliation pass: every `retention.reconcile_interval_secs` (default 3600, 0 disables) or on-demand admin trigger (`POST /admin/gc?reconcile=true[&dry_run=true]`, gRPC `TriggerGC(reconcile=true)`, audited, admin-only on both transports), the pass tombstones chunk rows with refcount>0 but zero manifest references — decided inside ONE `Durability::Immediate` transaction that re-walks every manifest (C-03 race defense) — then reclaims them through the C-03-safe sweep. Rule 16: dry-run is read-only. Locking: in-process busy mutex + cross-process lock file (1h stale-steal). Metric `file_storage_gc_reconcile_leaked_total`. The README crash-safety claim is now actually true (`fix(M-01)`).

### Fixed (regressions)
- **regression(W1-C-03)** — `race_gc_upload_x100` exposed a residual Wave-1 interleaving: the sweep re-checked references BEFORE the unlink, so an upload could register in the gap and lose its blob (dangling manifest). Fixed with a two-phase GC delete barrier: finalize row in-txn → atomic `.gc-pending` rename claim → resurrection re-check (RESTORE if a row re-appeared) → delete only when nothing holds it. Boot sweep discards crash-orphaned `.gc-pending` files (`regression(W1-C-03)`).

### Added
- CI: `c01-boot-gate` (C-01 story live per PR), `binary-size` guard (<15 MiB musl; current 11.85 MiB), `dashboard-smoke` (traffic-seeded series assertions), proptest suite (6 properties) + `fuzz/` libFuzzer skeleton (out-of-band, I-04), `docs/src/operations/ci.md`.
- Metrics: `repair_hash_mismatch_total{source="repair"}`, `repair_ciphertext_only_total`, `startup_quarantine_swept`, `gc_reconcile_leaked_total`; `/metrics` now sets `uptime_seconds` per scrape.
- Config (additive): `storage.quarantine_max_bytes` (1 GiB) / `storage.quarantine_retention_secs` (7d), `auth.login_throttle.*`, `retention.reconcile_interval_secs` (3600); proto `Manifest.chunks_truncated`, `TriggerGCRequest.reconcile`, `GCResult` reconcile report fields.
- Tests: `tests/regression/{m04_validation,m07_repair_verify,m05_throttle,m01_reconcile}.rs`, `tests/property/validation.rs`. Suite: 251 green (166 unit + 19 gRPC + 16 REST + 42 regression/property + boot gate + 2 in-module quarantine suites counted in unit).

## 1.1.0 — Remediation wave 1 (2026-11-12)

Baseline: `e4cd5a6` (audit NO-GO, 5.1/10). This wave lands **P0 complete**
plus the first P1 batch, with a gating test per fix.

### Fixed
- **M-13** — cfg-gate unix 0o600 key-file semantics; logged non-unix fallback (`fix(M-13)`).
- **C-01** — fail-closed secret validation (all shipped literals rejected in every build, ≥32-byte JWT secret, ≥24-byte api key, argon2id-only users in release, ALL violations reported at once); secret-mounted image (no baked config), compose `secrets:` + nonroot + loopback binds; `hash-password` stdin variant; `docs/src/operations/bootstrap.md` (`fix(C-01)`).
- **C-03** — verified batch/single-chunk registration with in-transaction blob presence check; uploader self-heal (rewrite + retry); GC sweep ordering flip (re-check → unlink → finalize); `Conflict` → 409 / FAILED_PRECONDITION; `race_gc_upload` ×100 in the default suite (`fix(C-03)`).
- **C-02** — replica gRPC purge is metadata-only; `NodeRole` guard refuses refcount GC on replicas; cross-protocol purge parity test (`fix(C-02)`).
- **H-02** — bounded multipart metadata reader (per-field 4 KiB cap, 32-field cap, unknown fields drained never buffered) (`fix(H-02,H-04,H-05)`).
- **H-04** — server-side decompression clamp (`min(size_hint, chunking.max)`, Corruption on mismatch); REST `CatchPanicLayer` with the standard error envelope; `size_stored == blob.len()` verification on both replication apply paths.
- **H-05** — metric `route` label from `MatchedPath` (`:unmatched` bucket), gRPC `rpc_of` strict parse (`:unparsed`), `metrics.exposure` config (loopback default / api_key / public+warning) (`fix(H-02,H-04,H-05)`).
- **H-03** — magic-byte validation wired into the pipeline (`validation.mode: enforce|off`, default enforce, 415 both transports); `nosniff` + risk-based `attachment` disposition + `Vary: Authorization` on downloads (closes **L-10**) (`fix(H-03)`).
- **H-01** — per-chunk fused pipeline (compressed bytes never outlive their chunk; `payloads` retention deleted); `UploadGate` admission control on both transports (semaphore + byte budget, pre-stream rejection for known sizes, drop-guard release); byte-based L3 manifest weigher (`fix(H-01)`).
- **M-02** — replication metrics truth: `ops_enqueued_total` counter, true queue-depth gauge, `lag_ops` metric (`fix(M-02,M-06,M-14)`).
- **M-06** — boot-time sweep of stale `.tmp-*` blobs and spool files before listeners bind (`fix(M-02,M-06,M-14)`).
- **M-14** — bounded enqueue (`enqueue_timeout_ms`, default 5 s): async mode drops the op with counter + error log (the single sanctioned fail-open), sync mode fails the upload (`fix(M-02,M-06,M-14)`).
- **M-15** — CI pipeline: fmt/clippy/test on linux+windows, `cargo audit` + `cargo deny`, buf lint/breaking, docker build with the C-01 secretless-boot gate and `poc_expect_fail.sh` smoke (`fix(M-15)`).

### Added
- `deny.toml`, `.github/workflows/ci.yml`, `poc_expect_fail.sh`.
- Tests: `tests/regression/` (race_gc, replica_purge, hardening), `tests/regression_c01_boot.rs`; unit gates per fix. Suite: 143 unit + 19 gRPC + 16 REST + 21 regression.
- Metrics: `chunk_register_race_total`, `replication_ops_enqueued_total`, `replication_ops_acked_total`, `replication_lag_ops`, `replication_dropped_ops_total`, `startup_tmp_swept_total`.
- Config (additive): `replication.role`, `metrics.{exposure,api_key}`, `validation.mode`, `limits.{max_in_flight_bytes,max_concurrent_uploads}`, `cache.manifest_max_bytes`, `replication.enqueue_timeout_ms`.

### Changed
- `Conflict` gRPC mapping: AlreadyExists → FailedPrecondition (HTTP stays 409).
- Frozen evidence `audit-repro/` call sites adapted to post-C-03 signatures (scenario semantics unchanged; policy in `audit-repro/README.md`).

### Deferred (tickets — see REMEDIATION_REPORT.md)
- M-01, M-04, M-05, M-07, M-09, M-10, M-12, M-16, M-17, L-02, L-09 (P1 remainder)
- M-03, M-08, M-11, fsync group-commit, L-01, L-04, L-06, L-07, I-07, WAL spool (P2)
- I-01..I-06, L-03, L-05, L-08 (P3)
