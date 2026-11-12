# REMEDIATION_PLAN_V7 — from the V6 re-audit

Findings reference `AUDIT_REPORT_V6.md` §4 / `FINDINGS_V6.csv`. Effort:
S <1d · M 1–3d. Nothing here is applied — the audit was read-only.

## Dependency graph

```
V6-17 (commit tree) ──► everything (gates need a committed baseline)
V6-05 + V6-06 (CI repair) ──► every later gate can actually run
V6-02 + V6-01 (gate fixes) ─┐
V6-03 (recursive sweep) ───┼─► P0 exit gate ─► P1 ─► P2
V6-04 (lag wiring) ────────┘
V6-07 (post-commit re-assert) ──► optional: closes C-03 permanently
V6-08 ──► M-03 (P2 summary rows) share one storage-layer change
M-16 pull-in depends on M-04 (done, V6-verified)
```

## P0 — Blockers (all S; ~1 week elapsed including CI re-run)

| Item | Fix | Gate test | Verify |
|---|---|---|---|
| V6-17 | Commit the working tree; reconstruct per-finding `fix(<ID>)` history on branches (or a documented squash with the report as provenance); branch-protect `main` + workflows | history exists; `git log --oneline \| grep fix(` non-empty | `git log` / `git branch` |
| V6-05 | ci.yml c01-boot-gate: `set -o pipefail`, capture exit before the pipe (or drop tee), grep `invalid configuration\|jwt_secret`; add a workflow-liveness self-check (every job must be able to pass on a known-good tree) | workflow self-check job green; boot-gate green on correct binary AND red when the binary is patched to boot | push a deliberately-broken branch, observe the gate go red |
| V6-06 | add `on: schedule: - cron: '0 3 * * *'`; move `h01_upload_memory_bound -- --include-ignored` into a reachable (non-nightly) memory job or the docker job; add minimal `permissions:` blocks | memory job green in CI artifacts (RSS curve logged) | CI run |
| V6-02 | `charge` only for unknown-size leases in the gRPC bridge; known leases reserve once (and reject mid-stream only if `streamed > expected`) | `h01_known_size_not_double_charged`: declared upload of budget/2+δ completes; `h01_known_overrun_rejected`: streaming past declared aborts | `cargo test --test regression_hardening h01` |
| V6-01 | `acquire_permit` returns the `OwnedSemaphorePermit`; lease takes it; delete `take_permit` | `h01_gate_no_panic_at_capacity`: 64 concurrent reservations on a 1-permit gate, multi-thread runtime, zero caught panics ×10 runs | same |
| V6-03 | `boot_sweep_tmp` walks recursively to `shard_depth + 1` (reuse the `list_all` walker shape); sweep `.tmp-*` and `*.gc-pending` by name regardless of level; keep age filter | `boot_sweeps_stale_tmp`: plant artifacts in shard dirs, build_state, assert gone (was *planned* in Wave-1 table, never landed) | `cargo test boot_sweeps_stale_tmp` |
| V6-04 | pass the lag counter into `worker_loop`; call `acked(n)` after each successful flush; decrement on the M-14 drop path | `lag_returns_to_zero_after_flush`: enqueue N ops against a mock upstream, assert `queue_depth() == 0` and `replication_lag_ops` published | `cargo test lag_returns_to_zero` |
| P0 checklist | re-run on Linux: `cargo test --locked --all-targets -- --include-ignored`, clippy `-D warnings`, fmt, `cargo audit --deny warnings` (+ create `audit.toml`, V6-OPS-02), `cargo deny check`, `buf lint && buf breaking`, benches incl. a ≥512 MiB upload fixture; execute `audit-repro-v6/` end-to-end and invert each repro into `tests/regression/` | all green in repaired CI | CI run |

**P0 definition of done:** scorecard Observability/Production-readiness rows
flip to GO; §10's four false doc rows corrected; verdict re-assessed to
GO-WITH-CONDITIONS.

## P1 — First 30 days

1. **V6-07** — post-commit `path_exists` re-assert + single rewrite in
   `store_chunk` (bytes still in scope). Gate: extend `race_gc_upload_x100`
   with a forced-interleaving variant that would fail without the re-assert.
2. **V6-09** — `repair_key_missing` classification (skip quarantine, own
   metric, distinct error). Gate: keys_dir-wiped replica test.
3. **V6-15** — per-IP `BoundedMap` throttle on REST + gRPC login (fail-open,
   rule 17), `login_throttle_{locked,evicted}_total` metrics, document the
   arbitrary-username-lockout residual in throttling.md. Gates: eviction
   DoS test (10k+ usernames, victim ladder unaffected beyond design),
   per-IP lockout test.
4. **Pull-ins (§13):** M-09 `/internal` route-scoped body limit; M-10 real
   actor+IP in upload/download audit rows (drop the last `Actor::System`);
   M-16 `dedup.scope=tenant` default + scoped-key migration behind a flag.
   Each behind its V4-planned gate test.
5. **V6-08 + M-03 (storage half)** — batched/curated manifest walk for
   reconcile + list summary rows. Gate: synthetic 100k-file fixture,
   reconcile peak RSS bounded, list p99 < 100 ms.
6. **V6-OPS-01/03/05** — compose limits + HEALTHCHECK; windows `--locked`
   + skip cleanup; fuzz breadth (mime/tags/expected_size vectors + corpus).
7. Soak promotion: 24 h upload/delete/GC loop with RSS + disk-growth
   assertions (catches V6-03/V6-10-class drift continuously).

**P1 exit:** §11 rows all GO or GO-WITH-CONDITION with named conditions;
`race_gc_upload_x100` green ×10 consecutive; zero dead CI jobs.

## P2 — Days 30–90

- **V6-10 + group-commit**: micro-batch chunk registrations (e.g. per
  pipeline window) to restore ≤1 fsynced txn per ~8 chunks; measure at
  ≥512 MiB.
- **V6-12**: in-memory quarantine byte accounting; dir walk at boot only.
- **V6-18**: unify the cross-process lock (GC + reconcile) with mtime
  heartbeat; document shared-dir support policy.
- **M-12** watcher supervision + gRPC probe write-check parity.
- **V6-14/V6-16** polish; audit.toml maintenance cadence.
- Remaining V3-P2: M-08 protocol unification (parity test now guards the
  seam), M-11 breaker + dead-letter replay, WAL spool (closes async RPO).
- V3-P3 unchanged (EdDSA/rotation, rebuild tool, audit hash chain,
  `pub(crate)` pass).

**P2 exit target:** re-audit verdict GO for a bounded launch; scorecard
≥8.0.
