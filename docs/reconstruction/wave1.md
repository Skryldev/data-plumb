# Reconstruction — Wave 1 (V4 remediation, audit V3 → 18 P0/P1 fixes)

Provenance document per `REMEDIATION_REPORT.md` §"Wave 1" and iron rule 18
(V6-17): the Wave-1/2 work reached this tree through session recycles that
collapsed the per-finding branches; the state is committed, and THIS
document is the finding→file→gate map that restores the audit trail.

Wave-1 state marker in git history: single snapshot commit `a993ca7`
("feat: add content validation, regression/property tests, and
benchmarks"), preceded by V3-audit baseline `e4cd5a6`.

## C-01 — fail-closed config validation
- **Files:** `src/config.rs` (`FORBIDDEN_SECRET_MARKERS`, `SecurityProfile`,
  `validate_for` reporting all violations; ≥32-byte jwt / ≥24-byte api-key
  floors; plaintext `password` release-forbidden; argon2id PHC check),
  `Dockerfile` (no baked config, CMD → mounted secret), `docker-compose.yml`
  (`secrets:`, nonroot 65532, loopback binds, `FS_REPLICATION__ROLE`),
  `secrets/file-storage.yaml.example`, `config/production.yaml` (no working
  credentials), `src/lib.rs` (stdin hash-password), `docs/src/operations/bootstrap.md`.
- **Gates:** `c01_config_rejects_every_shipped_literal`,
  `c01_config_rejects_plaintext_password_in_release`,
  `c01_config_reports_all_violations`, `c01_boot_refuses_default_yaml`,
  `poc_expect_fail.sh` (C-01 sections).

## C-02 — replica purge metadata-only
- **Files:** `src/grpc/services/replication_service.rs::apply_delete_file`
  (metadata-only, mirrors HTTP), `src/config.rs` (`NodeRole`),
  `src/gc.rs` (refcount-GC refusal on replicas), `src/storage/index_db.rs`
  (`NOTE(audit C-02)` on `upsert_chunk_replica`).
- **Gates:** `replica_purge.rs` suite (`replication_grpc_purge_shared_chunk`,
  `purge_semantics_http_equals_grpc`, `replica_refuses_refcount_gc`).

## C-03 — verified registration + uploader self-heal
- **Files:** `src/storage/index_db.rs` (`register_chunks_verified`,
  `chunk_is_referenced`, `finalize_sweep`), `src/core/pipeline.rs` (fused
  per-chunk stage with 3-attempt rewrite self-heal → 409), `src/gc.rs`
  (`sweep_victims`), `src/storage/block_store.rs` (`path_exists`).
- **Gates:** `race_gc.rs` (`race_gc_upload_x100`, `race_orphan_sweep_upload`,
  `gc_does_not_delete_reregistered_chunk`, `fused_register_rejects_missing_blob`),
  `index_db` units (`verified_register_reports_dedup_and_rejects_missing`,
  `finalize_sweep_only_removes_unreferenced`).

## H-01 — bounded uploads (fused pipeline + UploadGate + byte-based L3)
- **Files:** `src/core/pipeline.rs` (no whole-file retention; per-chunk
  enqueue), `src/core/gate.rs` (new), `src/state.rs` (`upload_gate` wiring),
  `src/grpc/services/file_service.rs` + `src/api/handlers/upload.rs` (lease
  admission both transports), `src/cache/manifest_cache.rs` (byte weigher,
  `manifest_max_bytes`), `src/config.rs` (`max_in_flight_bytes`,
  `max_concurrent_uploads`).
- **Gates:** `hardening.rs` `h01_*` ×5 + `gate.rs` units ×4 +
  `h01_upload_memory_bound` (ignored; nightly).

## H-02 — bounded multipart
- **Files:** `src/api/handlers/upload.rs` (`read_bounded_field`,
  `drain_bounded`, 32-field cap), `src/api/middleware/bounded_field_probe.rs`.
- **Gates:** `hardening.rs` `h02_*` ×5.

## H-03 — magic-byte validation wired + safe serving
- **Files:** `src/core/pipeline.rs` (validate_head on first chunk),
  `src/config.rs` (`ValidationMode`), `src/api/handlers/download.rs`
  (`risky_mime`, nosniff, attachment, Vary — closes L-10),
  `src/error.rs` (`UnsupportedMediaType` → 415), `src/state.rs` (loud opt-out).
- **Gates:** `hardening.rs` `h03_*` ×5, `risky_mime_table` unit.

## H-04 — decompression clamp + REST CatchPanic + peer size checks
- **Files:** `src/core/compressor.rs` (`max_chunk_plain` from `chunking.max`),
  `src/api/router.rs` (CatchPanicLayer), `src/api/middleware/request_id.rs`
  (`panic_to_500`), `src/api/handlers/internal.rs` +
  `src/grpc/services/replication_service.rs` (`size_stored == blob.len()`).
- **Gates:** `hardening.rs` `h04_*` ×3.

## H-05 — metrics cardinality + exposure (closes M-17 gating half)
- **Files:** `src/api/middleware/request_id.rs` (MatchedPath/:unmatched),
  `src/grpc/interceptors/metrics.rs` (strict `rpc_of` → `:unparsed`),
  `src/api/middleware/metrics_guard.rs` (new; loopback/api-key/public),
  `src/api/router.rs` (metrics route guard), `src/config.rs`
  (`MetricsExposure`).
- **Gates:** `hardening.rs` `h05_*` ×4.

## M-02 / M-06 / M-13 / M-14 / M-15 / M-17 / L-02 / L-09
- **M-02:** `src/replication/replicator.rs` + `src/metrics.rs` (counter /
  gauge / lag split; caveated by V6-04 — lag half dead until Wave 3).
- **M-06:** `src/state.rs::boot_sweep_tmp` + `startup_tmp_swept_total`
  (caveated by V6-03 — non-recursive until Wave 3).
- **M-13:** `src/security/encryption.rs` (`open_new_key_file` cfg-gate).
- **M-14:** `src/replication/replicator.rs` (bounded enqueue timeout,
  counted async drop, sync fail).
- **M-15:** `.github/workflows/ci.yml`, `deny.toml`, `poc_expect_fail.sh`.
- **M-17:** via H-05 exposure gating.
- **L-02/L-09:** partial (Vary header; interceptor comment) — completed
  rows live in `REMEDIATION_REPORT.md`.
