# Design: Crash Consistency — the invariant table (living contract)

Status: APPROVED (V9 PG-1, contract §2.3). This file is THE source of
truth for every multi-step mutation. Every later wave APPENDS rows (P4)
and marks its owner. Every crash point is executable via the failpoint
harness (`FAILPOINT=<name>` behind the `failpoints` cargo feature;
`xtask crash-test --point <name>` for operators).

## Harness

`failpoint!("_name")` compiles to a no-op unless the `failpoints` cargo
feature is on; with the feature, `FAILPOINT="name1,name2#2"` arms named
points (the `#n` form fires only on the n-th hit). Tests run with
`--features failpoints`; production builds carry zero cost.

## The table

| # | mutation | steps | crash point (failpoint name) | post-crash state | recovery owner | test |
|---|---|---|---|---|---|---|
| 1 | upload commit | blob write → fsync → rename → **dir fsync** → register at commit (WAL txn) → redb apply | `upload-before-blob-fsync` / `upload-after-rename` / `wal-before-fsync` / `wal-after-fsync-pre-apply` / `wal-after-apply-pre-watermark` | orphan blob (no row) → orphan sweep; WAL txn absent → pre-txn; committed → WAL replay re-applies | boot sweep + WAL replay | `wal_kill_at_every_record_boundary`, failpoint unit per point |
| 2 | delete + two-phase GC finalize | soft-delete → (grace) → finalize row → claim `.gc-pending` → resurrection re-check → delete/restore | `gc-after-claim` (process death here) | pending + no row → boot sweep garbage-collects; row re-registered meanwhile → uploader rewrote canonical (V6-07) | boot sweep (with V8 C-4 resurrection re-check: pending whose row is LIVE is restored, not deleted) | `boot_sweeps_stale_tmp_in_shard_dirs` + C-4 gate `boot_sweep_restores_resurrected_pending` |
| 3 | replica apply | fetch → verify-then-persist → txn-atomic WAL-fed apply | `replica-apply-mid-txn` | watermark-aligned state, no partial manifest | watermark replay | `replication_applies_txn_atomically` |
| 4 | migration record copy | read v1 row → write v2 set → checkpoint state | `migration-mid-record` | checkpoint state names the last done key; resume re-does it (idempotent upsert) | xtask resume | `migration_resumes_after_kill` |
| 5 | quota commit | re-check quotas → manifest write (same redb txn) | `quota-commit-overshoot` | txn aborted; WAL has no commit; reserve released | WAL (no commit) | `quota_commit_race_cannot_overshoot` |
| 6 | boot sequence | lock → WAL recovery → redb open + schema gate → boot sweep → listeners | `boot-after-recovery-pre-sweep` | recovery-complete, sweep pending → sweep runs at next boot | boot order (boot.md) | boot-order gate |
| 7 | **owner: TBD-by-PG2** snapshot / scrub-heal | — | — | — | PG-2 appends rows + failpoint tests (P4) | — |
| 8 | **owner: TBD-by-PG3** version write/purge · signed-URL issuance · pipeline enqueue/state · index update | — | — | — | PG-3 appends rows + failpoint tests (P4) | — |
| 9 | **owner: TBD-by-PG4** audit rotate/append chain | — | — | — | PG-4 appends rows (P4) | — |

## Boot sequence (formalized; see boot.md)

lock → WAL recovery → redb open (+ schema version gate) → boot sweep
(tmp/gc-pending with C-4 re-check) → scrub schedule → listeners bind.
The c01 boot-gate story extends: a v2 dir opened by an old binary
refuses BEFORE any write surface binds.
