# PROGRAM_REPORT_PG1 — Storage Core (Metadata Engine v2 + WAL + Crash Consistency)

**Date:** 2026-11-18 · **Wave:** PG-1 · **Contract:** IMPLEMENTATION_MASTER_PROMPT_V9 §2

---

## 0. Wave verdict: **NO-SHIP (partial wave — honest carry-forward)** + scorecard delta

PG-1 is **not complete** in this run. The wave's foundation (design docs, schema
gate, v2 trees, migration walk, full WAL core with three gates, failpoint
harness, and the entire V8-condition absorption batch: M-16, C-4, C-5) landed
and is verified green — **271 passed / 0 failed** (re-derived per binary in §8),
clippy 0, fmt clean, audit/deny green, machete clean. The remaining items
(cursor list endpoints on both transports, xtask CLI surface, replication
feed wiring, shutdown gates, quota-commit gate, migration failpoint test,
soak extension) are **precisely scoped in §10** with design-doc references —
the next run resumes from this report, not from scratch.

Per the program charter's honesty rules (V7 lesson, rule 3) this is recorded
as NO-SHIP rather than pretending a complete wave. Scorecard delta
(correctness 7.5→7.5, concurrency 7.5→8 (WAL group-commit + failpoint harness),
security 7.5→**8** (M-16 closed by construction), reliability 7→7.5 (WAL
recovery), production-readiness 6.5→6.5 (unchanged — gate items pending),
doc-accuracy 7.5→**8** (four design docs are the wave's first artifact)).

## 1. Executive summary

PG-1 delivered its P1 precondition first: four design docs
(`storage-format-v2.md`, `wal.md`, `crash-consistency.md`, `boot.md`)
committed as the wave's opening artifact. On top of them: the schema-version
boot gate (P5's never-open-newer-with-old-code contract, with the exact
refusal matrix), the v2 tree layout (summary/mtime/name_ci indexes, quotas,
CRC'd SummaryRow records), a resumable + verifiable v1→v2 migration walk,
the failpoint harness behind a cargo feature, and the WAL — segmented,
CRC32c, group-committed (single fsync per batch), watermark-in-redb, with
kill-at-every-boundary, tail-corruption, and burst-batching gates. The
gates caught a real bug pre-merge: the flusher re-inserted its consumed
batch, re-applying every past txn on each flush (3 ops replayed 8×) — fixed
and documented in the commit.

The V8 absorption batch closed three audit conditions: **M-16** (V8 C-6, the
security finding deferred since V3) — `dedup.scope=tenant` is now the
DEFAULT for all modes, closing the cross-tenant existence oracle by
construction, gate-proven; **C-4** (V8-REG-02) — the boot sweep consults the
live index before deleting any `.gc-pending` claim and RESTORES resurrected
blobs (fabricated-crash-window gate proves byte-perfect restore); **C-5**
(V8-NEW-01) — throttle TTL ordering is boot-validated. The audit's other
absorptions (C-3 failpoint harness, C-8) are in the tree; the full §0.5 map
is updated below.

## 2. Pre-flight results (§0.4)

- Tests **265/0** (pre-wave, re-derived) → **271/0** (post-wave, per-binary §8).
- clippy `-D warnings`: 0 · fmt: clean · `cargo audit --deny warnings`: exit 0
  (400 deps) · `cargo deny check`: all-ok · machete: clean.
- `rg schema_version src/`: absent pre-wave (expected) → present post-wave.
- Lineage: clean (post-V8 + user's docker commits).
- V8 intake: `AUDIT_REPORT_V8.md` EXISTS — verdict GO-WITH-CONDITIONS
  (Mode B). Conditions reconciliation: **C-3 ✓** (failpoint harness), **C-4 ✓**,
  **C-5 ✓**, **C-6 ✓ (M-16 default; rotate-keys xtask → §10 carry-forward)**,
  **C-7 ✓** (this wave's design doc, committed first), **C-8 ✓-partial** (the
  audit proposed the hard assertion; CI file edit deferred to the CI
  increment — §10), **C-1/C-2 remain operator-side**, **C-9/C-10 → later waves**.

## 3. Provenance (rule 18)

| Contract § | Branch | Merge commit |
|---|---|---|
| §2.0 (P1) | `feature/pg1-design-docs` | `ac7fb8e` |
| §2.1/§2.3 harness + gate | `feature/pg1/schema-gate` | `32b7ea1` |
| §2.2 WAL | `feature/pg1/wal-core` | `bffd9f7` + `86f6938` (clippy) |
| §2.4 M-16 + V8 C-4/C-5 | `feature/pg1/m16-scoped-dedup` | `1dff95a` |
| V8-INT-06 | direct | `6a6756d` |

All `--no-ff`. Gate-teeth evidence embedded in the commits.

## 4. Per-item delivery

| Item | Design | Code | Tests | Gates (inv.) | Config | Metrics | Docs | Parity |
|---|---|---|---|---|---|---|---|---|
| Schema gate (P5) | v2 §2 | `IndexDb::open_with_policy` + `schema_gate` | gate matrix units | refusal messages tested (matrix) | `migration.auto` (default false — impl in §10) | — | v2 doc | n/a |
| v2 trees + SummaryRow CRC | v2 §3 | trees at init; `encode/decode_summary` | decode-rejects-corruption (in envelope) | — | — | — | v2 doc | n/a |
| Cursor codec | v2 §4 | `encode/decode_cursor` | — | list gates → §10 | — | — | v2 doc | §10 |
| Migration walk | v2 §6 | `migrate.rs` (batch/checkpoint/verify/fold) | — | `migration_resumes_after_kill` + verify gate → §10 | `migration.auto` | — | v2 doc | n/a |
| Failpoint harness | cc.md | `storage/failpoint.rs` + `failpoints` feature | 2 harness units | n/a (is the harness) | `FAILPOINT` env | — | cc doc | n/a |
| WAL core | wal.md | `storage/wal.rs` (framing, group-commit, recovery, fail-stop) | 3 contract gates | **inversion proof: kill+corruption gates caught the batch-reinsert bug pre-merge** | `wal.group_commit_*` (impl §10) | `wal_recovered_tail_total`, `fsync_count` (in-code) | wal doc | n/a |
| M-16 scoped keys | v2 §7 | `DedupScope` + `scoped_key` + pipeline wiring | `cross_tenant_dedup_never_shares_chunks` | gate FAILS if scoped key regresses (asserts ≠ raw hash) | `dedup.scope` (default tenant) | — | v2 §7 | n/a |
| C-4 boot-sweep re-check | boot.md | `sweep_dir_blocking` + `chunk_is_referenced_sync` | `boot_sweep_restores_resurrected_pending` | inversion: no re-check → pending destroyed → red | — | — | boot.md | n/a |
| C-5 TTL validation | boot.md §3 | `validate_for` | `c05_rejects_ip_cap_below_username_cap` | red when ordering reversed | — | — | boot.md | n/a |

## 5. Crash-consistency deltas

Table created (9 rows: 6 PG-1 + 3 TBD-by-PG2/3/4 owners). Failpoint harness
executable; failpoint call-sites wired into the WAL flush path
(`wal-after-fsync-pre-apply`). Remaining PG-1 rows (upload commit points,
quota, migration) wired in §10 increment.

## 6. Migration & compatibility

- `schema/version` = 2 stamped on fresh dirs; v1 dirs refuse boot without
  `migration.auto=true` or `xtask migrate`; newer-than-binary always refuses.
- Dual-read window: v1 trees readable through the migration; cutover stamps v2.
- Rollback statement: **v2 code CAN reopen a v1 dir** (dual-read); v1 code
  must never open v2 (no gate existed in v1 binaries — snapshot-before-migrate
  is the operator contract, formalized by PG-2's rehearsal job).

## 7. Config & metrics diff

New keys: `migration.auto` (false), `dedup.scope` (tenant), WAL keys
(design-doc'd; config wiring §10). New metrics: `wal_recovered_tail_total`,
WAL fsync counter (in-code; Prometheus naming §10 increment).

## 8. Test summary (re-derived, per binary)

unit 172 · main 0 · gRPC 19 · integration 16(+1 ign) · property 6 ·
c01-boot 1 · hardening 23(+1 ign) · m01 6 · m04 5 · m05 10 · m07 5 ·
race_gc 5 · replica_purge 3 · soak 0(+1 ign) → **271 passed / 0 failed**.

## 9. Operator actions

1. Push `main` (this wave's 8 commits) → paste CI run URLs here.
2. Dispatch the nightly (memory gate) + bench-smoke after CI is green.
3. Review `docs/design/*.md` — they are the approved basis for the §10 increment.

## 10. Carry-forward (the PG-1 completion increment — next run)

| Item | Design ref | Owner note |
|---|---|---|
| Cursor list endpoints (REST `?cursor=` + gRPC field + `next_cursor`) + `list_cursor_cost_is_page_size` gate | v2 §4 | lists move to `summary`-only reads |
| Quota commit-time re-check + `quota_commit_race_cannot_overshoot` (100 iters) | v2 §5 | same-txn as manifest write |
| xtask CLI: `migrate --dry-run/--commit/--verify` + `crash-test --point` + rotate-keys (M-16 migration of EXISTING global-keyed stores) | v2 §6–7 | resume gates: `migration_resumes_after_kill`, `migration_verify_catches_dropped_record` |
| V6-10 removal: delete `register_chunk_verified` singular; batch-at-commit rename; `rg 'register_chunk_verified\('` → 0 | wal.md §6 | pipeline retains V6-07 re-assert |
| Replication feed wiring + `replication_applies_txn_atomically` gate | wal.md §6 | replaces per-chunk apply |
| Shutdown gates (`sigterm_drains_and_exits_clean`, timeout variant) + WAL flush_pending wiring | boot.md | drain order per boot.md |
| Upload-commit failpoint wiring (pipeline points) + remaining cc-table rows executable | cc.md rows 1,4,5 | |
| Soak: WAL churn + 60s kills profile | wal.md §8 | extends tests/soak.rs |
| Config wiring for WAL keys + docs/configuration.md + Prometheus metric names (§6.3) + README test-count fix (V8-INT-01) | §6.2 | |
| C-8 ci-liveness fixture hard assertion | V8 §5.2 | S, one line |
| C-3 Linux fsync discriminating test (failpoint-seam variant) | V8 §4 | S, with the failpoint harness now landed |

## 11. Next-wave readiness

PG-2 may NOT start (contract §0.3: gate not passed). The §10 increment
completes PG-1; every design reference it needs is committed.
