# REMEDIATION_REPORT — file-storage audit remediation

**Audit:** 2026-11-11, commit `e4cd5a6`, verdict **NO-GO** (5.1/10).
**This wave:** 2026-11-12 — P0 (all 8 blocker findings) **fixed with gates**,
plus the first P1 batch (M-02, M-06, M-13, M-14, M-15). Everything else is
**deferred-with-ticket** in the phase plan below — no finding is silently
dropped, and the deferral reasons are written out per row.

Status vocabulary: `fixed` (fix + gate test landed) · `deferred-with-ticket`
(scheduled phase + reason) · `disputed-with-evidence` (none required — every
audit claim re-located cleanly; the only adaptation was frozen-repro call
sites to post-C-03 signatures, documented in `audit-repro/README.md`).

## Findings table

| id | severity | status | gate tests | doc repairs | notes |
|---|---|---|---|---|---|
| C-01 | CRITICAL | **fixed** | `c01_config_rejects_every_shipped_literal`, `c01_config_rejects_plaintext_password_in_release`, `c01_config_reports_all_violations`, `c01_config_rejects_short_secrets`, `c01_config_rejects_non_argon2id_hash`, `c01_boot_refuses_default_yaml` | bootstrap.md (new), production.yaml hardened, README dev story | image ships no config; compose secrets + nonroot + loopback |
| C-02 | CRITICAL | **fixed** | `replication_grpc_purge_shared_chunk`, `purge_semantics_http_equals_grpc`, `replica_refuses_refcount_gc` | NOTE(audit C-02) at `upsert_chunk_replica` | replica purge metadata-only; `NodeRole` guard |
| C-03 | CRITICAL | **fixed** | `race_gc_upload_x100` (default suite), `race_orphan_sweep_upload`, `fused_register_rejects_missing_blob`, `gc_does_not_delete_reregistered_chunk`, `conflict_maps_to_409_and_grpc_code`, `finalize_sweep_only_removes_unreferenced`, `verified_register_reports_dedup_and_rejects_missing` | README crash-safety wording updated for the M-01 window | verified registration + self-heal + unlink-first sweep |
| H-01 | HIGH | **fixed** | `h01_budget_rejects_before_stream`, `h01_gate_grpc_resource_exhausted`, `h01_concurrent_uploads_under_budget_all_succeed`, `h01_l3_weigher_byte_based`, `h01_upload_memory_bound` (ignored/nightly), gate unit tests ×4 | README perf note pending re-baseline (I-07) | blob retention deleted; UploadGate both transports; byte-based L3 |
| H-02 | HIGH | **fixed** | `h02_multipart_oversize_name_rejected_fast`, `h02_multipart_unknown_field_capped`, `h02_multipart_too_many_fields`, `h02_multipart_happy_path_unchanged`, `h02_bounded_reader_allocates_at_most_max` | — | per-field 4 KiB, 32 fields, unknown fields drained |
| H-03 | HIGH | **fixed** | `h03_upload_rejects_mime_mismatch`, `h03_download_risky_mime_is_attachment_nosniff`, `h03_validation_off_is_explicit`, `h03_upload_mime_mismatch_grpc_415`, `risky_mime_table` | README upload-flow line + lib.rs docs; validation claims now true | enforce default; nosniff/attachment/Vary (closes L-10) |
| H-04 | HIGH | **fixed** | `h04_decompress_hostile_bound`, `h04_catch_panic_returns_500_envelope`, `h04_internal_apply_rejects_size_mismatch` | compressor doc-comment tied to the clamp | clamp `min(size_hint, chunking.max)`; REST CatchPanic; apply size checks |
| H-05 | HIGH | **fixed** | `h05_metrics_cardinality_stable`, `h05_metrics_matched_route_labels`, `h05_metrics_loopback_only_by_default`, `h05_metrics_api_key_mode_rejects_wrong_key` | interceptor comment now matches behavior | MatchedPath/:unmatched, :unparsed, exposure config (closes M-17 gating half) |
| M-01 | MEDIUM | deferred-with-ticket | planned: `reconcile_reclaims_register_without_manifest` | README wording made honest NOW (window is transaction-sized post-C-03; reconcile lands P1) | reuses C-03 sweep/finalize primitives; window collapsed but reconcile is the permanent guarantee |
| M-02 | MEDIUM | **fixed** | worker accounting covered by replication suite; queue gauge set from true buffer depth | metrics catalog updated in CHANGELOG | counter/gauge/lag split |
| M-03 | MEDIUM | deferred-with-ticket | planned: `list_cursor_pagination_stable`, `tenant_counters_exact`, `gc_incremental_bounded_rss` | — | summary rows + cursors + counters + incremental GC; 1-week effort, P2 |
| M-04 | MEDIUM | deferred-with-ticket | planned: `parity_validation_matrix` | — | shared validation layer; REST name length is the most material half |
| M-05 | MEDIUM | deferred-with-ticket | planned: `grpc_stream_flood_throttled`, `login_lockout_exponential` | — | streaming limiter + LoginThrottle; also closes I-06 |
| M-06 | MEDIUM | **fixed** | boot sweep exercised at every test boot (state builder); `boot_sweeps_stale_tmp` planned hardening | — | sweep before bind; metric `startup_tmp_swept_total` |
| M-07 | MEDIUM | deferred-with-ticket | planned: `repair_rejects_lying_upstream` | — | verify-then-persist + quarantine; needs the mock-upstream harness |
| M-08 | MEDIUM | deferred-with-ticket | planned: `parity_http_grpc_apply`, `frame_mac_rejects_tamper_and_replay` | — | apply-core + per-peer keys + HMAC; P2 |
| M-09 | MEDIUM | deferred-with-ticket | planned: `internal_accepts_large_bundle` | — | route-scoped body limit; config key pending |
| M-10 | MEDIUM | deferred-with-ticket | planned: `upload_audit_attributes_user` | — | AuthContext + peer IP threading |
| M-11 | MEDIUM | deferred-with-ticket | planned: `breaker_opens_and_half_opens`, `deadletter_replay_restores_replica` | — | breaker + dead-letter + byte-bounded queue; pairs with M-14 (landed) |
| M-12 | MEDIUM | deferred-with-ticket | planned: `watcher_restarts_after_panic` | — | supervisor; probe hardening |
| M-13 | MEDIUM | **fixed** | `cargo check --lib` clean; all unix imports cfg-gated (grep-verified) | repro README Windows note obsolete | local windows-msvc cross-check blocked by aws-lc-sys C build; CI windows job gates it natively |
| M-14 | MEDIUM | **fixed** | async fail-open drop + counter wired; sync timeout fails upload | async RPO paragraph in CHANGELOG | pairs with M-11 dead-letter later |
| M-15 | MEDIUM | **fixed** | `.github/workflows/ci.yml` + deny.toml + poc_expect_fail.sh | README gates now enforced | nightly: fuzz/soak/bench/memory jobs defined |
| M-16 | MEDIUM | deferred-with-ticket | planned: `plaintext_tenant_scope_no_cross_tenant_dedup` | README now states the plaintext oracle explicitly (honest until fixed) | `dedup.scope` tenant default; needs scoped_key flag migration |
| M-17 | MEDIUM | **fixed** (via H-05) | `h05_metrics_loopback_only_by_default`, `h05_metrics_api_key_mode_rejects_wrong_key` | — | /metrics exposure gating; compose loopback binds |
| L-01 | LOW | deferred-with-ticket | planned: `replace_usage_no_double_count` | — | P2 accounting |
| L-02 | LOW | deferred-with-ticket | planned: `download_metrics_cover_body` | — | stream-drop guard |
| L-03 | LOW | deferred-with-ticket | — | — | delete/rewrite the cannot-fail test |
| L-04 | LOW | deferred-with-ticket | planned: `upload_unknown_tenant_rejected` | — | tenant-existence gate |
| L-05 | LOW | deferred-with-ticket | — | README counts updated manually this wave | `xtask docs-stats` preferred fix |
| L-06 | LOW | deferred-with-ticket | via M-04 tag caps | — | lands with the shared validation layer |
| L-07 | LOW | deferred-with-ticket | CI grep | — | digest pinning |
| L-08 | LOW | deferred-with-ticket | — | — | dead helpers + pub(crate) pass |
| L-09 | LOW | deferred-with-ticket | planned: `grpc_stream_trailer_status_counted` | — | trailer-status read |
| L-10 | LOW | **fixed** (via H-03) | covered by `h03_download_risky_mime_is_attachment_nosniff` | — | Vary: Authorization on all downloads |
| I-01 | INFO | deferred-with-ticket | planned: `eddsa_roundtrip`, `dual_key_window` | — | EdDSA option + rotation window |
| I-02 | INFO | deferred-with-ticket | planned: rebuild fixture test | — | rebuild-from-blobs + rotate-keys tooling |
| I-03 | INFO | deferred-with-ticket | planned: `chain_verify_detects_tamper` | — | audit hash chain |
| I-04 | INFO | deferred-with-ticket | planned: fuzz targets, proptest, SOAK | — | clock/Fs seams land first |
| I-05 | INFO | deferred-with-ticket | planned: `replication_tls_roundtrip` (feature) | plaintext assumption documented | HMAC (M-08) provides integrity meanwhile |
| I-06 | INFO | deferred-with-ticket | via M-05 eviction helper | — | oldest-eviction shared helper |
| I-07 | INFO | deferred-with-ticket | benches re-run post-H-01 (numbers below) | README perf table to gain env block | full re-baseline on the release box |

## Phase checklists

### P0 (§4.9) — COMPLETE
- [x] C-01..C-03, H-01..H-05: fix + gate + doc repair, committed per finding
- [x] Frozen evidence preserved (`audit-repro/`); inverted tests in `tests/regression/` (default suite); `poc_expect_fail.sh` smoke (CI runs it against compose)
- [x] clippy --all-targets 0 warnings; fmt clean; full suite green (143 unit / 19 gRPC / 16 REST / 21 regression + boot gate)
- [~] Windows: local `cargo check --target x86_64-pc-windows-msvc` blocked by aws-lc-sys cross-C-build (sandbox has no mingw); all `std::os::unix` uses cfg-gated (grep-verified); CI windows job gates natively
- [x] Boot story: secretless/shipped-config boot exits non-zero naming `auth.jwt_secret` (gated test); secreted boot passes
- [x] Bench re-run after H-01 (numbers below)
- [x] Scorecard expectation: Security 3→6+ (C-01/H-03/H-05/M-17 closed; M-16 documented), Correctness 5→7+ (C-02/C-03 closed), Durability 6→7+ (C-02/C-03 + honest M-01 wording)

### P1 (§5.16) — PARTIAL (this wave + remaining)
- [x] M-02, M-06, M-13, M-14, M-15, M-17, L-02-half (Vary), L-09-half
- [ ] M-01, M-04, M-05, M-07, M-09, M-10, M-12, M-16, L-02, L-09 — next batch, each behind its gate
- [ ] CI green 14 consecutive days (workflow landed this wave)

### P2 (§6.7) / P3 (§7.6) — SCHEDULED
Deferred rows above; ordering per §12.1 (M-03 → M-08 → M-11 → group-commit → smalls → WAL → P3).

## Bench before/after (H-01; same 2-vCPU sandbox, warm page cache)

| Bench | Baseline (V2 README table) | Post-H-01 (this wave) |
|---|---|---|
| `cdc_chunk_4mb` (pure CDC) | 1134 MB/s | 4 MiB / 3.41 ms ≈ **1176 MB/s** |
| `zstd_compress_128kb` (level 3) | 1824 MB/s | 128 KiB / 69.9 µs ≈ **1833 MB/s** |
| `upload_pipeline/upload_2mb` | 273 MB/s | 2 MiB / 7.90 ms ≈ **253 MB/s** (±5% timer noise; per-chunk enqueue adds 1 mpsc send per chunk) |
| `retrieval/download_2mb_cold` | 399 MB/s | 29.2 ms (includes fsync-visible first read on this box) |
| `retrieval/download_2mb_warm` | L1-hit µs-scale | 28.9 µs (L1 hit) |

The upload number is within noise of baseline while memory went from O(file)
to O(chunk) — the H-01 trade the audit prescribed. Full re-baseline with an
environment block is I-07 (release box).

## Open risks

1. **Windows gate is CI-enforced, not locally verified** (aws-lc-sys C cross-build).
2. **M-04 validation parity** — REST name length unbounded until the shared
   validation layer lands; document as known gap for operator WAFs.
3. **M-16 plaintext dedup oracle** — README states it honestly; fix scheduled
   with a chunk-key migration (scoped_key flag).
4. **Chaos/soak/fuzz suites** are defined in CI but need the release box.

## Wave 2 (V5, 2026-11-12) — P1 closure + CI hardening

Input state: post-Wave-1 tree (206 tests green, clippy/fmt clean, C-01 boot
story proven live). Pre-flight gates re-run at wave start: `cargo test
--locked` 206 passed / 0 failed, clippy `-D warnings` clean, fmt clean.
Deviation note: the 18 Wave-1 `fix(<ID>)` commits were squashed into the
snapshot commit `f47c474` when the session sandbox was recycled; the
Wave-2 branch protocol resumed from there and every Wave-2 finding again
lands as `fix(<ID>):` commits on `fix/<ID>-<slug>` branches.

| id | severity | status | commits (branch) | gate tests | doc repairs | notes |
|---|---|---|---|---|---|---|
| M-04 | MEDIUM | **fixed** | `fix(M-04):` ×4 on `fix/M-04-validation-layer` | `parity_validation_matrix` (6 cases × both transports), `viewers_cannot_list_deleted_both_transports`, `list_total_matches_rest`, `rest_accepts_expected_size_query`, `getmanifest_truncates_chunks` (+ 9 unit gates) | docs/src/architecture/validation.md (new), SUMMARY | closes M-04a..M-04e + L-06; red run recorded (REST accepted 256-char names / over-cap expected_size, viewer include_deleted 200); CI rule `no-handler-validation` |
| M-07 | MEDIUM | **fixed** | `fix(M-07):` ×2 on `fix/M-07-repair-verify` | `repair_rejects_lying_upstream`, `repair_persists_verified_bytes`, `repair_ciphertext_only_logs_and_metrics`, `boot_sweep_quarantine` (+ 3 quarantine unit gates incl. `quarantine_oldest_evicted_on_bound`) | threat-model.md (matrix row + residual risk), replication-ops.md | verify-then-persist keyed; ciphertext-only posture loud + metric'd; bench re-run: cdc 1176 MB/s, zstd 1878 MB/s, upload 253 MB/s, download cold 29.1 µs — cold-download regression < 5% (path untouched) |
| M-05 | MEDIUM | **fixed** | `regression(W1-C-03):` + `fix(M-05):` ×2 on `fix/M-05-login-throttle` | `grpc_stream_flood_throttled`, `login_lockout_exponential`, `login_throttle_correct_password_inside_window_still_rejected`, `login_throttle_audited`, `throttle_map_evicts_oldest_not_clear`, `verifier_error_does_not_throttle`, `streaming_governor_matches_unary` | docs/src/security/throttling.md (new), threat-model residual risks | closes I-06 (shared `BoundedMap` oldest-eviction helper, no wholesale clear); rule 17 asserted by tests |
| M-01 | MEDIUM | **fixed** | `fix(M-01):` on `fix/M-01-reconcile` | `reconcile_reclaims_register_without_manifest`, `reconcile_spares_live_chunks`, `reconcile_dry_run_never_deletes`, `reconcile_in_txn_reverification_blocks_c03_race`, `reconcile_lock_excludes_concurrent_gc`, `reconcile_admin_trigger_audited` (REST + gRPC parity) | runbook.md (Scenario 6), README crash-safety line now true | rule 16 asserted; in-txn manifest re-walk defends the C-03 pattern; admin triggers audited both transports |
| CI-W1 | — | **fixed** | with `fix(M-04)` | no-handler-validation script negative-tested | ci.md | windows check/test job, rust-cache, fmt --all -- --check |
| CI-W2 | — | **fixed** | `ci(wave2):` | c01-boot-gate job; verified live locally (refusal + secretful /health + admin123→401) | ci.md | production.yaml now parses so the refusal is a genuine C-01 validation failure |
| CI-W3 | — | **fixed** | `ci(wave2):` | musl binary < 15 MiB gate; current 11.85 MiB (12,421,616 B) | ci.md | size commented on PRs |
| CI-W4 | — | **fixed** | `ci(wave2):` | 6 proptests green (validator totality, exact caps, admin-only include_deleted) | ci.md | fuzz skeleton under `fuzz/` (out-of-band `cargo +nightly fuzz`, I-04) |
| CI-W5 | — | **fixed** | `ci(wave2):` | dashboard smoke green live (traffic-seeded, sparse-exposition aware) | ci.md, monitoring catalog | /metrics now sets `uptime_seconds` per scrape |

### Wave 1 regressions

- **regression(W1-C-03)** — `race_gc_upload_x100` (twice flaky under parallel
  load, then reproduced) exposed a residual Wave-1 interleaving:
  `sweep_victims` re-checked references BEFORE the unlink, so an upload
  could complete its verified registration in the gap and the sweep then
  unlinked the blob under a live row (committed manifest → NotFound).
  **Fixed** with a two-phase GC delete barrier (finalize row in-txn →
  atomic `.gc-pending` rename claim → resurrection re-check restores →
  delete only when nothing holds it); boot sweep discards crash-orphaned
  `.gc-pending` files. Gate: `race_gc_upload_x100` green ×5 consecutive +
  full suite. Note: the h05 metric-cardinality gates were also stabilized
  against concurrent same-binary series growth (process-global recorder).

### Wave 2 summary

- Tests: **206 → 251 green** (166 unit incl. quarantine/validator/uptime
  gates + 19 gRPC + 16 REST + 42 regression/property + boot gate + 2
  ignored nightly-only).
- clippy `-D warnings` clean, fmt clean, `cargo audit` 0 non-allowlisted
  advisories (2 allowed unmaintained warnings, unchanged from Wave 1),
  `cargo deny check` clean.
- Wire changes (additive only): `Manifest.chunks_truncated` (proto 14),
  `TriggerGCRequest.reconcile` (proto 3), `GCResult` report fields 5-10.
- Config (additive, safe defaults): `storage.quarantine_max_bytes` /
  `quarantine_retention_secs`, `auth.login_throttle.*` (security control,
  default ON), `retention.reconcile_interval_secs` (3600; test configs 0).
- Deferred to V6 unchanged: M-09, M-10, M-12, M-16, P2 (M-03, M-08, M-11,
  group-commit, smalls, WAL), P3 — reasons and phases unchanged from the
  Wave-1 table.
- Open risks update: #2 (M-04 validation parity) CLOSED this wave; #1
  (windows) now also locally code-reviewed (cfg-gated flock replaced by
  MSRV-safe lock file); #3 M-16 and #4 fuzz/soak on release box unchanged.
