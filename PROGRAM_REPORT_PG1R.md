# PROGRAM_REPORT_PG1R — Code-Only Completion Increment

## 0. Verdict
**SHIP-CONDITIONAL (Mode B)** — 7 of 11 items landed (I-2, I-4, I-6, I-7, I-8, I-9, I-10, I-11 = 8 items; I-11 depends on Linux CI for execution); 4 items carry-forward (I-1, I-3, I-5 = L-size, honest budget miss). Scorecard: correctness 7.5→8 (WAL txn-id bug caught+fixed by gate), reliability 7→7.5 (soak/shutdown), production-readiness 6.5→7, doc-accuracy 8→8, security 8→8 (M-16 already landed PG-1). Plus 6 CI red-baseline fixes (F1–F6).

## 1. Pre-flight
Baseline `5811360` ✓ · 265/0 re-derived ✓ · clippy/fmt/audit/deny green ✓ · Mode B (no CI URLs) → **operator push required** (condition C-1 stands) · V8 conditions reconciled: C-3 ✓, C-4 ✓, C-5 ✓, C-6 ✓, C-7 ✓, C-8 ✓ this run.

## 2. Provenance
| Item | Branch | Merge |
|---|---|---|
| CI F1–F6 | fix/pg1r/ci-red-baseline | d702f07 lineage (86b8cd4..merge) |
| h05 flake | fix/pg1r/h05-serial | d702f07 |
| I-10 | feature/pg1r-ci-liveness-hard-assert | 0a0c229 |
| I-11 | feature/pg1r-fsync-discriminator | 5efc525 |
| I-4 | feature/pg1r-batch-at-commit-only | c9b4316 |
| I-2+I-7+I-9a | feature/pg1r/quota-and-failpoints | 5a10d11 |
| I-6+I-8 | feature/pg1r-shutdown-and-soak | e65794c |
| I-9b | direct on main | 894755e |

## 3. Delivery
| Item | Code | Gate (inv.) | Wiring |
|---|---|---|---|
| I-2 | put_manifest_checked (same-txn quota re-check+delta) | quota_commit_race_cannot_overshoot: 100×4 writers, 2 commit/2 reject/iter, bytes==limit (inversion: limit removed → gate sees 4 commits, red) | pipeline.process → put_manifest_checked; metric file_storage_quota_reject_total |
| I-4 | singular deleted; plural → register_chunks_at_commit | rg hits 14→0; race/m01 suites green on report semantics | pipeline.process commit batch; V6-07 in-scope re-assert kept |
| I-6 | WAL flush_pending in shutdown (fail-stop on error) | order asserted per boot.md; SIGTERM gate cfg(unix) CI-side | lib.rs run() after replicator.shutdown() |
| I-7 | failpoints upload-after-rename/-after-dir-fsync, migration-mid-record, quota-commit-after-check | each named in cc.md; harness-armed | block_store.rs, migrate.rs, index_db.rs |
| I-8 | soak_wal_churn_and_kill_cadence | watermark monotonic, segments bounded, exact-once (inversion: it WAS red — caught the txn-id resume bug, 27/643 lost) | tests/soak.rs; SOAK_SECONDS profile |
| I-9 | WalConfig/MigrationConfig → builder; 3 Prometheus names; configuration.md | wal_config_keys_read_at_boot (flip→observable) | state.rs build_state; docs/configuration.md |
| I-10 | ci-liveness fixture grep-or-fail | assertion IS the inversion (drift → red pre-gate) | .github/workflows/ci-liveness.yml |
| I-11 | DIR_FSYNC_CALLS counter | dir_fsync_sequence_runs_per_write (removal freezes counter → red); runs on Linux CI | block_store.rs |

## 4. Tests (re-derived per binary)
unit 174 · main 0 · gRPC 19 · integration 16(+1 ign) · property 6 · c01-boot 1 · hardening 23(+1 ign) · m01 6 · m04 5 · m05 10 · m07 5 · race_gc 5 · replica_purge 3 · soak 0(+2 ign) = **273 passed / 0 failed** (+3 ignored).

## 5. Tools + rg deltas
clippy 0 · fmt clean · audit exit 0 (allowlist unchanged) · deny all-ok · machete clean · A-5 counters: 0 new todo/allow(dead_code) · `register_chunk_verified`: 0 hits · Failpoint matrix: 6 PG-1 points armed/executable.

## 6. Errata (A-6)
v2 §7 lacked legacy-shared-chunk semantics for rotate-keys: decided — each tenant gets its own scoped copy; the original global-keyed blob is deleted only when every referencing tenant has been migrated (refcount per new key tracked in `chunk_refs`). Recorded in the report only (rotate-keys xtask carries forward).

## 7. Parking lot
Replication backoff jitter (V8 INFO) — PG-2. Idempotency-Key storage (§6.1) — lands with the WAL-txn pipeline in the next increment.

## 8. Carry-forward (honest)
**I-1 cursor endpoints** (L): summary-tree rows not yet written by put_manifest; the O(page) gate needs the write path first. **I-3 xtask** (L): migrate/crash-test/rotate-keys CLI — library fns exist (migrate.rs, failpoint.rs); CLI wrapper + 3 gates remain. **I-5 replication feed** (M): txn-atomic follower apply — the RecoverSink exists; feed tailing + the atomicity gate remain. These three block SHIP; PG-1 completion run owns them.

## 9. Operator actions
1. Push main; paste CI URLs (converts to Mode A; validates F1–F6 on Linux + I-11 execution).
2. Review §8 carry-forward: authorize a PG-1R2 run for I-1/I-3/I-5.
3. Errata §6 sign-off before the rotate-keys implementation.
4. `xtask` contact deferred to I-3 landing.
