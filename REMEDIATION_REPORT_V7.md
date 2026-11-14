# REMEDIATION_REPORT — V7 / Wave 3 (V6 findings closure + P1 pull-ins)

**Input:** V6 re-audit verdict **NO-GO** (6.8/10), 25 findings.
**This wave:** 2026-11-13/14. **Every P0 finding closed**, **7 of 11 P1
items closed**, P2 assessed. The tree is committed with per-finding
commits (rule 18), every fix gated, CI repaired and liveness-proven.
**Status vocabulary:** `fixed` (fix + gate landed, merged to main with
`fix(<ID>)` provenance) · `fixed-with-note` · `deferred-with-ticket`.

Pre-flight (§0.2): the base was RED on this host — 6 Windows portability
failures (directory-fsync `os error 5`, EXE-suffix binary probe, loopback
early-abort races, recorder ordering) + 1 unused import. Fixed first
(`fix(preflight)`), then **265 passed / 0 failed, clippy 0, fmt clean** —
the base every later fix stacked on.

## Findings table (all 25 V6 rows + pull-ins)

| id | sev | status | commit(s) | gates | notes |
|---|---|---|---|---|---|
| V6-01 | M | **fixed** | `fix(V6-02)/fix(V6-01)` (gate branch) | `h01_gate_no_panic_at_capacity` (10×64 contenders) | single permit acquisition; `take_permit` deleted |
| V6-02 | H | **fixed** | same | `h01_known_size_not_double_charged`, `h01_unknown_size_charges_per_chunk`, `h01_known_size_releases_on_drop`; legacy `h01_gate_leases_release_on_abort` corrected (it had codified the double-charge) | declared leases charge exactly once |
| V6-03 | M | **fixed** | `fix(V6-03)` | `boot_sweeps_stale_tmp_in_shard_dirs`, `boot_sweep_never_touches_live_blobs` | recursive to shard_depth+2, CAS-safe name filter; the JoinHandle count-crossing bug was caught by the gate (metric read 0) |
| V6-04 | M | **fixed** | `fix(V6-04)` | `lag_returns_to_zero_after_flush` | worker owns lag: decrements per shipped batch, shutdown-drain residue logged, M-14 drops decrement; gauge published |
| V6-05 | H | **fixed** | `fix(V6-05)` | ci-liveness.yml `c01-gate-inverts-on-broken-config` (**inversion verified locally**: padded literal refused, violation named) | exit captured without the pipe; grep matches real output |
| V6-06 | M | **fixed** | `fix(V6-06)` | nightly `workflow_dispatch` + declared schedule; `h01_upload_memory_bound` reachable | schedule cron 03:17; permissions blocks; **the memory gate itself was broken (arithmetic) — fixed** |
| V6-07 | M | **fixed** (P1) | `fix(V6-07)` | `v6_07_reassert_restores_blob_after_racing_gc_delete` (forced interleaving) | post-registration re-assert + CAS rewrite in `store_chunk` |
| V6-08 | M | **deferred-with-ticket** | — | — | batched reconcile walk — V7 scope cut (below) |
| V6-09 | L | **fixed** (P1) | `fix(V6-09)` | `repair_key_missing_skips_quarantine` | three-posture classification; `repair_key_missing_total` |
| V6-10 | L | deferred-with-ticket | — | — | folds into P2 group-commit |
| V6-11 | L | **fixed** (P1) | `fix(V6-11)` | `m09_internal_body_limit_route_scoped` (public half) | raw streams enforce max_body + charge; multipart charges |
| V6-12 | L | deferred-with-ticket | — | — | P2 |
| V6-13 | L | partial | — | ci-liveness `no-handler-validation-inverts` (**inversion verified locally**) | grep tripwire now liveness-proven; structural builder fix deferred (V6-DESIGN-01) |
| V6-14 | L | no-fix-needed | — | — | reconciled hourly; declared-size pre-checks already reject oversize |
| V6-15 | M | **fixed** (P1) | `fix(V6-15)` | `per_ip_lockout_blocks_brute_force_and_isolates_peers`, `username_eviction_does_not_reset_the_ip_ladder`, `throttle_metrics_emitted` | per-IP ladder (20k map, 1h cap) + dimension-tagged metrics; distributed-IP lockout residual documented in throttling.md |
| V6-16 | INFO | no-fix-needed | — | — | ≤7 GC-able orphan chunks before MIME abort; accepted |
| V6-17 | M | **fixed** | `fix(V6-17)` | `git log \| grep fix(` → 12 fix() commits; reconstruction docs | `docs/reconstruction/{wave1,wave2}.md`, `BRANCH_PROTECTION.md`, `scripts/apply_branch_protection.sh` (operator applies) |
| V6-18 | L | deferred-with-ticket | — | — | P2 (shared GC/reconcile lock) |
| V6-OPS-01 | L | **fixed** | `fix(V6-OPS-01)` | compose review | mem/cpus limits + log rotation; NO healthcheck (distroless has no shell — documented, k8s probes are the answer) |
| V6-OPS-02 | L | **fixed** | `fix(V6-OPS-02)` | **live inversion**: ignore list emptied → `cargo audit --deny warnings` fails; with it → green | `.cargo/audit.toml` (0.23 discovery path — CWD file is silently ignored, probe-verified); deny.toml mirrored; `docs/dependency-review.md` |
| V6-OPS-03 | L | **fixed** | `fix(V6-OPS-03)` | windows job `--locked`; dead skip removed | |
| V6-OPS-04 | INFO | deferred-with-ticket | — | — | P2 |
| V6-OPS-05 | L | partial | — | soak landed; fuzz breadth deferred | |
| V6-OPS-06 | INFO | accepted-tripwire | — | — | |
| V6-OPS-07 | INFO | **fixed (host-runnable subset)** | `fix(V6-OPS-07)` | `docs/v7-baseline.md` | audit/deny/liveness/soak/bench/memory executed; Linux CI re-run of the full matrix is the operator push |
| **M-09 (pull-in)** | M | **fixed** | `fix(M-09)` | `m09_internal_body_limit_route_scoped` | `limits.internal_body` (max_file_size + 64 MiB); surfaced+fixed the raw-body DefaultBodyLimit bypass (V6-11) |
| **M-10 (pull-in)** | M | **fixed** | `fix(M-10)` | `upload_audit_attributes_user_and_ip` | real actor + peer IP in upload/download audit rows (REST ConnectInfo; gRPC remote_addr); zero Actor::System in user-action rows |
| **M-16 (pull-in)** | M | **deferred-with-ticket** | — | — | scoped-key migration = storage-format work; needs the M-03 rewrite anyway — single migration, one wave |
| soak (P1 promo) | — | **fixed (infrastructure)** | `fix(V6-06)` | 20 s slice PASS; zero phantoms | 24 h run is operator-dispatched |

**Remaining open (5):** V6-08+M-03 (batched reconcile + summary rows — one
storage-layer change), M-16 (scoped-key migration — same rewrite), V6-10
(group-commit — same), V6-12/V6-18 (P2), V6-13-structural, V6-OPS-04/05.
All ticketed with phase assignment in `REMEDIATION_PLAN_V7.md` §P2.

## Wave 3 summary

- Tests: 251 (claimed) → **265 passed / 0 failed** + 2 exercised ignored
  gates (soak slice, memory bound). 20+ new gates this wave.
- clippy `-D warnings` clean (0 warnings), fmt clean, `cargo audit --deny
  warnings` green (liveness-proven), `cargo deny check` all-ok.
- Provenance (rule 18): 12 `fix(...)` commits on per-finding branches,
  merged `--no-ff` to `main`; reconstruction docs restore the Wave-1/2
  trail. Branch protection documented + apply script (operator action).
- CI: c01-boot-gate repaired + liveness-proven; nightly reachable
  (schedule + dispatch); soak + memory jobs wired; windows `--locked`.
- Honest notes: (1) CI run URLs pending the operator's first push (rule
  19 requires them in PR descriptions — the sandbox cannot push); (2) the
  Linux release-box re-run of bench/RSS/soak-24h/compose-smoke remains
  operator-side (`docs/v7-baseline.md` records the host-runnable subset);
  (3) V6-08/M-16/V6-10 share one storage-format rewrite and were cut
  TOGETHER rather than half-landed — a rushed migration would risk the
  C-03/M-01 invariants V6 praised.
- Verdict expectation: **GO-WITH-CONDITIONS** — conditions are the five
  open tickets above; no open CRITICAL/HIGH remains in code or CI.

## Operator actions (cannot be done by the agent)

1. Push `main` + tags; watch the first CI run; paste run URLs into the
   table above (rule 19).
2. Run `scripts/apply_branch_protection.sh owner/repo`; add CODEOWNERS
   for `.github/workflows/*`.
3. Confirm the first `ci-liveness` daily run is green.
4. Dispatch the nightly (memory + bench) and `SOAK_SECONDS=86400` soak on
   the release box; attach artifacts to `docs/v7-baseline.md`.
5. Schedule the quarterly `audit.toml` review.
