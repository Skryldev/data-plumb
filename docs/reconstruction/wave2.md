# Reconstruction — Wave 2 (V5 remediation: M-04, M-07, M-05, M-01, W1-C-03 regression, CI-W1..W5)

Companion of `docs/reconstruction/wave1.md` (read its header for why this
document exists). Source of truth: `REMEDIATION_REPORT.md` §"Wave 2" and
`docs/src/operations/wave2-report.md`.

## M-04 — shared validation layer (closes M-04a..e + L-06)
- **Files:** `src/core/validation.rs` (new — the single validator:
  name 1..=255B/C0-C1 ban, ≤32 tags ×1..=64B, mime ≤128B,
  `expected_size` ≤ max_file_size pre-admission, admin-only
  `include_deleted`), `src/api/handlers/{upload,list}.rs` +
  `src/api/dto/upload.rs` (`expected_size` query), `src/grpc/services/file_service.rs`
  (view + shared list/count), `src/storage/index_db.rs::list_filtered`,
  `src/grpc/convert.rs::manifest_to_proto_capped` (+ proto 14
  `chunks_truncated`), `scripts/check_no_handler_validation.sh`.
- **Gates:** `m04_validation.rs` ×5, `property/validation.rs` ×6,
  `parity_validation_matrix`, `list_total_matches_rest`,
  `getmanifest_truncates_chunks`, `viewers_cannot_list_deleted_both_transports`.

## M-07 — verify-then-persist repair + quarantine
- **Files:** `src/storage/retrieval.rs::repair_chunk` (decode-for-verify →
  BLAKE3 → quarantine/invalidation/alarm), `src/storage/quarantine.rs` (new),
  `src/state.rs` (quarantine boot sweep), `src/config.rs`
  (`quarantine_max_bytes`, `quarantine_retention_secs`).
- **Gates:** `m07_repair_verify.rs` ×4, quarantine units ×3
  (`quarantine_oldest_evicted_on_bound`, …).

## M-05 — streaming governor parity + login throttle (closes I-06)
- **Files:** `src/security/throttle.rs` (new), `src/core/bounded_map.rs`
  (new — shared oldest-eviction), `src/security/auth.rs` (throttle
  before argon2; rule-17 ordering), `src/state.rs` (shared
  `rate_limiter` in AppState), `src/grpc/interceptors/rate_limit.rs`
  (BoundedMap buckets), `src/grpc/services/{file_service,auth_service}.rs`
  (streaming entry checks; throttled audit action), `src/api/handlers/auth.rs`,
  `src/security/audit.rs` (`LoginThrottled`), `src/error.rs` (`Throttled` → 429),
  `src/config.rs` (`ThrottleConfig` default ON).
- **Gates:** `m05_throttle.rs` ×7 (`login_lockout_exponential`,
  `verifier_error_does_not_throttle`, `streaming_governor_matches_unary`,
  `throttle_map_evicts_oldest_not_clear`, …).

## M-01 — reconcile pass (permanent guarantee behind the C-03 window)
- **Files:** `src/gc.rs` (`reconcile`, `ReconcileMode/Report`,
  `acquire_reconcile_lock` + guard, `spawn_reconcile`),
  `src/storage/index_db.rs` (`collect_referenced_hashes`,
  `tombstone_unreferenced_chunks`), `src/api/handlers/admin.rs` +
  `src/grpc/services/admin_service.rs` (audited reconcile triggers),
  `proto` `TriggerGCRequest.reconcile` (additive), `src/config.rs`
  (`reconcile_interval_secs`).
- **Gates:** `m01_reconcile.rs` ×6 (`reconcile_in_txn_reverification_blocks_c03_race`,
  `reconcile_lock_excludes_concurrent_gc`, `reconcile_admin_trigger_audited`, …).

## W1-C-03 — two-phase GC delete barrier (Wave-1 residual race)
- **Files:** `src/gc.rs::sweep_victims` (pre-check → in-txn finalize →
  `.gc-pending` claim → resurrection re-check → delete/restore),
  `src/storage/block_store.rs` (`claim_for_gc`, `restore_claimed`,
  `delete_claimed`, `GC_PENDING_SUFFIX`, `find_pending`),
  `src/api/handlers/delete.rs` + `file_service.rs::purge_file` (route through
  `purge_chunks_now`).
- **Gates:** `race_gc_upload_x100` ×5-green claim; `finalize_sweep_only_removes_unreferenced`.

## CI-W1..W5
- **CI-W1:** windows job + rust-cache + fmt (caveat V6-OPS-03: skip prefix
  dead, `--locked` missing — Wave 3 fixes).
- **CI-W2:** `c01-boot-gate` job (caveat V6-05: pipe/grep defects — Wave 3
  repairs + `ci-liveness.yml`).
- **CI-W3:** `binary-size` job (musl < 15 MiB).
- **CI-W4:** proptest job + `fuzz/` skeleton.
- **CI-W5:** `dashboard-smoke` job + `scripts/dashboard_smoke.sh` +
  `uptime_seconds` per scrape.
- **Nightly:** bench/memory job (caveat V6-06: no schedule trigger — Wave 3
  repairs).
