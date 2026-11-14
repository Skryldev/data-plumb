# Wave 3 Report — V6 Findings Closure (V7)

**Date:** 2026-11-13/14 · **Contract:** V7 implementation master prompt ·
**Full table:** `REMEDIATION_REPORT_V7.md` · **Baseline:**
`docs/v7-baseline.md`

## What landed

| Phase | Findings | Status |
|---|---|---|
| Pre-flight | Windows portability (6 failures) + unused import | **fixed** — base re-established green |
| W3-P0 | V6-17 (provenance), V6-05+V6-06 (CI), V6-01+V6-02 (gate), V6-03 (sweep), V6-04 (lag), OPS-01/02/03 | **fixed** |
| W3-P1 | V6-07 (re-assert), V6-09 (key posture), M-09 (body limit), V6-11 (budget charge), M-10 (audit actor), V6-15 (per-IP throttle), OPS-07 (baseline) | **fixed** |
| Deferred | V6-08+M-03, M-16, V6-10, V6-12/18, OPS-04/05 | ticketed (one shared storage-format rewrite; see report) |

## The fixes in one paragraph each

**Gate (V6-01/02).** `charge()` is now a no-op for declared leases (the
V6-02 double-charge aborted every declared upload over budget/2), and the
permit is acquired ONCE into the lease (the release/re-acquire
`.expect()` panic is structurally gone). Six gate tests.

**Boot sweep (V6-03).** Recursive to `shard_depth+2`, CAS-safe (only
`.tmp-*`/`*.gc-pending` are ever removed — a backdated live blob
survives, gated). The first implementation had a Copy-capture bug that
silently zeroed the swept count — the gate caught it before merge.

**Lag (V6-04).** The worker owns the lag counter: decrements per shipped
batch, logs shutdown-drain residue, publishes `replication_lag_ops`;
counted drops decrement (terminal state). Gate drives a real upstream.

**CI (V6-05/06 + rule 19).** c01-boot-gate captures the binary's exit
without a pipe and greps the real output wording; `ci-liveness.yml`
proves both load-bearing gates invert (locally verified: padded shipped
literal refused + planted handler violation trips the grep rule). Nightly
declared (schedule + dispatch) with the memory gate reachable — which
immediately surfaced that the memory gate itself had never run and its
budget arithmetic was wrong (fixed).

**Re-assert (V6-07).** `store_chunk` re-asserts the blob after the
registration commits (bytes still in scope): the sub-transaction C-03
window is closed — forced-interleaving gate proves the rewrite.

**Repair posture (V6-09).** Key-missing is a local misconfiguration, not
hostile bytes: dedicated metric, Encryption error, nothing quarantined.

**M-09/V6-11.** `/internal` gets its route-scoped body limit; the gate
also exposed that raw-body uploads never honored `DefaultBodyLimit` —
the raw stream now enforces `max_body` mid-stream and charges the lease.

**M-10.** Upload/download audit rows carry the real username + peer IP.

**V6-15.** Per-IP throttle ladder (own map, 1 h cap > username 15 min) on
both auth surfaces; identity-free dimension-tagged lockout metrics;
eviction-DoS resistance gated; residuals documented.

## Provenance

12 `fix(<ID>)` commits on per-finding branches, `--no-ff` merged;
`docs/reconstruction/{wave1,wave2}.md` restore the Wave-1/2 trail;
branch-protection contract + apply script for the operator (rule 18).

## Verification

265/0 tests (20+ new gates), clippy 0, fmt clean, audit `--deny warnings`
green (inversion-proven), deny all-ok, soak slice green, memory gate
fixed + exercised, bench re-run recorded in `docs/v7-baseline.md`.
