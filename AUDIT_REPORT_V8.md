# File Storage Re-Audit Report — V8 (R2: Delta Verification + Full Production Sweep)

**Audit date:** 2026-11-14 · **Tree:** post-V7 `main` @ `6a6756d` · **Auditor:** independent senior Rust systems/security review (V3→V6 lineage)

---

## 0. Verdict

**GO-WITH-CONDITIONS (conditional) — evidence Mode B.**

Scorecard (movement vs V6 shown): correctness 7→**7.5** · concurrency 7→**7.5** · security 7→**7.5** · resource-bounds 6→**7** · observability 6→**7.5** · reliability 7→**7** · code-quality 7→**8** · performance 6→**6.5** · production-readiness 7→**6.5** (re-graded against §7.7 lifecycle gaps, not just the open set) · doc-accuracy 7→**7.5** · **overall 6.8→7.2** (≥7.0 target met; no dimension <5.0).

The verdict is **conditional**, not unconditional: this audit ran in **Mode B** (no Linux CI evidence — see §2), so "operator push + green CI matrix on ubuntu-latest" is named condition **C-1**. No new CRITICAL/HIGH was found; the two §4 regressions are MEDIUM/LOW; §4.1's durability sequence is **intact in code** (cfg-gate verified) but **untested** — that is the wave's most important finding.

---

## 1. Executive Summary (≤400 words)

Wave 3's fixes are real. All 13 "fixed" dispositions were **confirmed** by re-running every named gate on this host (§3 table): the gate teeth probes prove the corrected tests discriminate (double-charge reintroduced → red; sweep counter zeroed → red), the lag gauge is published, the boot sweep is recursive and CAS-safe, the repair path has three honest postures, and the per-IP throttle ladders are independent with identity-free metrics. Provenance holds: 12 `fix(<ID>)` commits on `main`, every fixed row traceable.

The regression hunt found **two genuine findings and one integrity cluster**. Most important: **V8-REG-01** — the POSIX dir-fsync sequence (the invariant every durability claim rests on) has **no discriminating test**: on a scratch branch I removed both `#[cfg(unix)]` fsyncs and the full suite stayed 265/0 green. The code is correct by inspection and the cfg-gate shape is exactly right (Linux behavior unchanged — no HIGH regression), but the invariant is one careless refactor away from silently false. Second: **V8-REG-02** — the boot sweep deletes crash-orphaned `.gc-pending` blobs by name+age **without** the runtime sweep's resurrection re-check; a crash in the narrow re-check-fail→restore window leaves a live row whose last local copy is the pending file, which boot then destroys (detectable as a reconcile phantom, not repairable on a primary). Integrity: the V7 baseline doc's test itemization said 240 when the true total is 265 (miscount, reconciled in §5.1 with a per-binary table); the ci-liveness broken-config fixture lacks a post-sed hard assertion; and — found by this audit — `REMEDIATION_REPORT_V7.md` itself was never committed (fixed: now `6a6756d`).

The full sweep (§7) returned dispositions for all nine areas: two MEDIUM-eligible new findings (**V8-NEW-01**: the documented per-IP>per-username TTL ordering is not boot-validated; **V8-NEW-02/P2**: no upload idempotency story), one INFO cluster, and a strengths register of eleven verified production-grade assets. The gap map (§9) assigns every open row to V9 (the storage-format rewrite wave) or ops/docs actions, with **M-16 on a hard V9 deadline** — it has been deferred since V3 and its README caveat is accurate but is not a substitute for the fix on any multi-tenant plaintext deployment.

---

## 2. Evidence Base

**Mode: B** — the tree is sandbox-local; no GitHub CI runs exist yet (no remote push has occurred; `git log` shows no CI run URLs anywhere in the reports). Per rule 19 and §0.1, the verdict is therefore **conditional** and condition **C-1** ("operator push + green CI matrix on ubuntu-latest, URLs recorded") gates any production use. In Mode B, every §3 row's evidence source is **local reproduction on Windows (host-shaped caveat noted where the discriminating assertion is Linux-only)** — no row relies on a report claim alone.

**Host:** Windows 10 x64 (10.0.26100), 16-core, NVMe, rustc 1.99.1 MSVC. Linux-only assertions (`/proc` RSS, fsync behavior) are identified per-item and become parts of condition C-1/C-2 rather than silent pass-throughs.

**Re-derived numbers (rule 10):** tests 265/0 (per-binary table §5.1); `fix(` commits on main: **12** (preflight, V6-17, V6-05+06+OPS-03, V6-01/02, V6-03+04, OPS-01, OPS-02, V6-07/09/M-09/V6-11, M-10, V6-15, OPS-07, +2 docs/probe commits added by this audit); clippy `-D warnings`: 0; fmt: clean; `cargo audit --deny warnings`: exit 0; `cargo deny check`: all-ok.

---

## 3. V7 Confirmation Table (13 rows)

| # | Item | Method | Result | Evidence |
|---|---|---|---|---|
| 1 | V6-17 provenance | `git log \| grep fix(` = 12; every fixed row cross-checked to a commit; reconstruction docs spot-checked (3 findings/wave: C-01/H-03/M-05 files+gate names match tree) | **confirmed** | commit list §2; `docs/reconstruction/*` exist and match `tests/` names; BRANCH_PROTECTION.md + apply script present. *Found during check:* REMEDIATION_REPORT_V7.md itself was **untracked** — committed as `6a6756d` (V8-INT-06, fixed-in-audit) |
| 2 | V6-05/V6-06 CI repair + liveness | Read ci.yml c01-boot-gate (exit captured via file redirection + `$?`, no pipe; grep `invalid configuration\|jwt_secret\|api_key\|argon2\|password` — **ran the binary locally with a shipped-literal config**: output names `matches a shipped/known literal` → grep matches); ci-liveness.yml read end-to-end; nightly declares `schedule:` + `workflow_dispatch:`, memory job runs the ignored test, permissions on 5 jobs | **confirmed** (code-level; the hosted runs themselves are C-1) | Local inversion: broken config exits 1 naming the violation (V7 record; wording re-verified against lib.rs/config.rs strings) |
| 3 | V6-02/V6-01 gate | `rg take_permit` → only comments (structurally gone); permit acquired once into lease, released only by Drop; re-ran all 8 gate tests | **confirmed** | 8/8 green; §4.6 teeth probe: double-charge reintroduced → `h01_known_size_not_double_charged` **FAILED** (discriminates) |
| 4 | V6-03 boot sweep | Re-ran both gates; read walker: recursion `shard_depth + 2` (matches 2-level shard + file level), name filter `.tmp-*`/`gc-pending` only in CAS mode, age filter, count via JoinHandle return (no capture bug; no off-by-one — depth counts dirs, file level reached at depth≥2) | **confirmed** | 2/2 green; counter-zeroing probe → gate **FAILED** (teeth) |
| 5 | V6-04 lag | `rg acked` → worker flush decrement (replicator.rs:318-322), shutdown-drain ack (285-296), M-14 drop decrement (131-134); re-ran `lag_returns_to_zero_after_flush`; gauge publish site = single worker publisher | **confirmed** | gate green; `/metrics` rendering asserted inside the gate (local scrape semantics identical in-process) |
| 6 | V6-OPS-01/02/03 | compose: mem/cpus/logging + written no-healthcheck rationale (§5.5); `.cargo/audit.toml`: both IDs + rationale + expiry, **deny-warnings green and inversion-proven in V7 (re-verified exit 0 this audit)**; windows job `--locked`, dead skip gone | **confirmed** | audit/deny exits re-run today |
| 7 | V6-OPS-07 baseline | `docs/v7-baseline.md` complete; **Mode B** → row stays "fixed (subset)"; Linux matrix = condition C-1/C-2 | **confirmed as subset** | per §0.1 this row must not silently graduate |
| 8 | V6-07 re-assert | Gate re-ran (`race_gc` 5/5); **coverage matrix in §4.3** — all four commit paths argued; replica path covered by the NodeRole guard (gc.rs:100: refcount GC refused on replicas → no claims → no C-03 window) | **confirmed** | forced-interleaving gate green; matrix §4.3 |
| 9 | V6-09 repair posture | Re-ran `repair_key_missing_skips_quarantine`; read classification: keyed→verify; encryption-disabled→ciphertext-only (explicit posture, loud metric+warn); key-file-missing→`repair_key_missing_total`, Encryption error, no quarantine/CAS/L2. Ciphertext-only is config-gated (`encryption.enabled=false`), NOT a silent key-absent fallback (V6-09 fix distinguishes them) | **confirmed** | gate green; posture code retrieval.rs:193-231 |
| 10 | M-09/V6-11 | Re-ran `m09_internal_body_limit_route_scoped`; `default_internal_body()` = 10 GiB + 64 MiB in config code; raw stream enforces `max_body` mid-stream via the scan (abort → 413 class) and the gate asserts the public cap still 413s | **confirmed** | gate green; abort releases the lease structurally (lease lives in the stream; stream drop on abort releases) |
| 11 | M-10 audit actor | Re-ran `upload_audit_attributes_user_and_ip`; `rg Actor::System src/` → only audit.rs enum, replication apply (internal.rs:212, replication_service.rs:262 — genuinely cluster-initiated), and audit.rs test helpers. Zero in upload/download user paths | **confirmed** | gate asserts actor=admin + ip=127.0.0.1 |
| 12 | V6-15 per-IP throttle | Re-ran 3 gates; config: `ip_lockout_cap_ms` 1h > `lockout_cap_ms` 15min (ordering correct in DEFAULTS — boot validation gap = V8-NEW-01); REST handler passes `Some(peer.ip())`, gRPC passes `remote_addr()` — BOTH surfaces, BOTH dimensions; metrics dimension-only (no identity labels); residuals in throttling.md | **confirmed** (with V8-NEW-01) | 10/10 m05 gates green |
| 13 | Soak | `SOAK_SECONDS=15` slice: green (this audit); nightly job wired; 24h operator-dispatched | **confirmed (slice)** | §3.13 condition honored (not run 24h) |

**13/13 confirmed. Zero "claimed". Zero "failed".**

---

## 4. Regression Findings

### [V8-REG-01] MEDIUM · durability — the dir-fsync sequence has no discriminating test
- **Evidence:** `rg 'sync_all|fsync' tests/` → **zero hits**. Scratch-branch proof (this audit): removed both `#[cfg(unix)]` dir-fsyncs in `block_store.rs` → **full suite 265/0 green**. The invariant V3 §3.1 praised ("directory fsync after rename — the step most implementations forget") is protected by nothing but code review.
- **Why not HIGH:** the fsync code is present and correct (gate shape verified §4.1); Linux behavior was NOT changed by the pre-flight fix; the failure mode is a *future* refactor silently dropping it.
- **Fix (S):** a Linux-only test that discriminates — e.g. a `#[cfg(unix)]` unit test using `strace`-free failpoint injection (extract `fsync_dir` behind a `DurabilityPolicy` seam; a test policy records calls; assert `write()` invoked it), plus the stronger CI-side option: an strace wrapper job asserting `fsync(dirfd)` appears between rename and return on a scripted upload. Wave: **V9 start** (condition C-3).
- **Pre-flight sibling fixes checked:** EXE probe uses compile-time `consts::EXE_SUFFIX` (no behavioral change); loopback abort tolerance is Ok-arm-gated (Linux 413 assertion intact, no sleeps added); recorder reorder strengthens (initializes before increment). No Linux-behavior change in any hunk of `fix(preflight)` — read end-to-end.

### [V8-REG-02] LOW-MEDIUM · boot sweep deletes `.gc-pending` without the runtime re-check
- **Evidence:** `state.rs:293-300` — stale `.gc-pending` removed by name+age only. Runtime sweep ordering (gc.rs:238-273) is finalize→claim→re-check→restore/delete; a crash between re-check-fail (row live) and `restore_claimed` leaves row-live + canonical-missing + pending-holding-the-last-copy. Boot then destroys that copy. Detected later as a reconcile phantom; **not repairable on a primary** (Repair re-pulls nothing).
- **Window:** two adjacent fs ops mid-crash — narrower than C-03 itself, hence not HIGH.
- **Fix (S):** before deleting a `.gc-pending`, call `chunk_is_referenced`; live → rename back to canonical (restore), stale → delete. Wave: **V9 start** (condition C-4).
- **§4.5 stress bounds:** live canonical blob survives (gate exists); fresh `.tmp-` survives (age gate exists); the resurrected-pending case is the gap above. Boot sweep has no snapshot-staleness hazard (it reads FS + live index only).

### §4.2 REST × lease model — **clean**
Raw: no reserve+charge (declared reserves once; `Content-Length` known → declared lease → charge no-op; chunked → unknown lease → charges per chunk, aborts at budget). Multipart: spool charges per chunk (V6-11). The one double-charge shape (gRPC's old bug) has no REST instance. Abort releases the lease (stream-owned). Oversize-abort residue: registered chunks pre-abort are the V6-14 transient, healed by reconcile — coupling to V6-08 noted in §6.

### §4.6 gate teeth — **confirmed discriminating** (probes executed, then restored)
1. double-charge reintroduced → `h01_known_size_not_double_charged` red ✓
2. memory fixture: arithmetic now tests the real boundary (2.5 GiB admits both leases; Linux RSS assertion = the discriminator, Windows-skipped → part of C-2) ✓
3. sweep counter zeroed → `boot_sweeps_stale_tmp_in_shard_dirs` red ✓

---

## 5. Report-Integrity Findings

| ID | Finding | Disposition |
|---|---|---|
| **V8-INT-01** (MEDIUM) | V7 baseline itemized 240 tests (160+19+17+43+1) vs the claimed 265. **Re-derivation (per-binary, this host):** unit 166 · main 0 · gRPC 19 · integration 16(+1 ign) · property 6 · c01-boot 1 · hardening 23(+1 ign) · m01 6 · m04 5 · m05 10 · m07 5 · race_gc 5 · replica_purge 3 · soak 0(+1 ign) → **265 passed / 0 failed**. The TOTAL was right; the itemization miscounted (unit is 166 not 160; regression+property+soak is 58+6+1-ign not 43). Corrected table lives here; the baseline doc should adopt it (docs-only). |
| **V8-INT-02** (LOW-MEDIUM) | ci-liveness broken-config fixture: sed has no post-application hard assertion (`grep -q 'change-me-in-production' /tmp/broken.yaml \|\| exit 1`) and line 35 is a stray duplicate sed with `\|\| true`. **Proposed one-line fix in place** (not applied — audit is read-only for product/CI). The violation-probe arm names the planted file (assertion 2 satisfied). |
| **V8-INT-03** | **clean** — audit.toml ↔ deny.toml ↔ dependency-review.md all cross-reference; both IDs in both files with rationale+expiry. (Proposed: a CI job diffing the ID sets — optional.) |
| **V8-INT-04** (LOW) | V7 said "five open tickets"; the actual open set is 8 IDs / 4 groups. Corrected by §7's exact conditions list. |
| **V8-INT-05** | **clean** — healthcheck disposition is WRITTEN (compose comments: distroless has no shell; kubernetes.md shows `livenessProbe`). |
| **V8-INT-06** | REMEDIATION_REPORT_V7.md was **untracked** — provenance gap in V7's own deliverable. **Fixed during this audit** (`6a6756d`). |
| **V8-INT-07** (LOW, informational) | Poison-handling inconsistency: hasher/lease mutexes use `.expect()` (poison unreachable in practice — no panicking region inside the guard scope) while throttle/rate-limiter recover via `into_inner()`. Standardize on recovery. |

---

## 6. Deferred Re-assessment

| Item | Assessment | Deadline |
|---|---|---|
| V6-08+M-03 | Grouping sound **only with a design**: condition **C-7** requires `docs/design/storage-format-v2.md` (cursor walk, summary rows, dual-read migration) before the rewrite wave starts. | V9 |
| **M-16** | Security finding open since **V3**. Interim mitigation verified: README:171 carries the plaintext-oracle caveat (survived V7 doc churn). **HARD DEADLINE: V9.** Plaintext mode must not serve multi-tenant production before scoped keys land (condition **C-6**). | **V9 (hard)** |
| V6-10 | Correct grouping (registration fsync shape is what the rewrite touches). | V9 |
| V6-12 | No severity change (bounded dir). | V9/V10 |
| V6-18 | Shared-dir still documented unsupported; unchanged. | V10 |
| V6-13-structural | Tripwire liveness-proven (§5.2's hardening strengthens it); builder remains defense-in-depth. | V10 |
| V6-OPS-04/05 | Phases confirmed (P2); fuzz vectors cheap — consider pulling into V9. | V9/V10 |
| V6-14 | Rationale ("reconcile heals") **inherits V6-08's scale risk** — at 1M files the healing pass itself is the memory problem. Coupling noted; resolves with the rewrite. | V9 |
| V6-16 | Comment now matches behavior (`pipeline.rs:144-146` states first-chunk enforcement + concurrent siblings accurately). | closed |

---

## 7. Conditions List (EXACT — one bullet per ID, phase + deadline)

- **C-1** — Operator push of `main`; full CI matrix green on ubuntu-latest (incl. c01-boot-gate, ci-liveness, nightly memory job, windows, audit/deny/buf); run URLs recorded in REMEDIATION_REPORT_V7.md §summary. **Gate for ANY traffic. Deadline: before launch.**
- **C-2** — Linux bench + RSS baseline recorded from the CI nightly artifacts (closes I-07; supersedes the Windows numbers in v7-baseline.md as the authoritative table). **Deadline: with C-1's first nightly.**
- **C-3** — V8-REG-01: add a Linux-discriminating test for the dir-fsync sequence (failpoint seam or strace CI job). **V9 start. Owner: storage.**
- **C-4** — V8-REG-02: boot-sweep resurrection re-check (restore-not-delete for live rows). **V9 start. S.**
- **C-5** — V8-NEW-01: boot-validate throttle TTL ordering (`ip_lockout_cap_ms >= lockout_cap_ms`) + any impossible-combination lint added in V9's config pass. **V9 start. S.**
- **C-6** — **M-16 scoped-key default + migration — HARD DEADLINE V9.** No multi-tenant production on plaintext mode before it lands. (README caveat is mitigation, not fix.)
- **C-7** — `docs/design/storage-format-v2.md` (cursor walk + summary rows + dual-read migration + M-16 key scoping + V6-10 group-commit) **approved before the rewrite wave starts**.
- **C-8** — V8-INT-02 liveness fixture hard assertion (+ remove the stray sed). **V9 start. S.**
- **C-9** — V8-REG-01's test plus the backup restore-drill test (§9 gap map rows 1–2) land in **V9**'s first CI increment.
- **C-10** — Remaining P2 (V6-12, V6-18, V6-13-structural, OPS-04, OPS-05, idempotency design, versioning statement, capacity guide, SLOs) — **V9/V10**, owners per gap map.

---

## 8. Strengths — Production-Grade Today (≥8, evidence-backed)

All items re-verified **this audit** (proven, Mode B local; Linux-CI-dependent items noted):

1. **Two-phase GC delete barrier** — forced-interleaving gate (`race_gc` 5/5 green) + teeth-probe verified; the runtime ordering (finalize→claim→re-check→restore/delete) read line-by-line. *(Boot-side gap is V8-REG-02, filed separately.)*
2. **Verify-then-persist repair with three postures** — `m07` gates green incl. lying-upstream rejection and key-missing-no-quarantine; postures read in code.
3. **UploadGate lease model** — 8 unit gates green; teeth probe proves the charge-once contract is enforced by test, not convention.
4. **Boot sweep CAS-safety** — live-blob survival gate + stale-artifact gate + counter-teeth probe (3 gates, all discriminating).
5. **Per-IP + per-username throttle** — 10 m05 gates green; isolation and eviction-independence proven; identity-free metric labels (H-05 discipline held).
6. **CI liveness (rule 19), locally proven** — both inversions (boot-gate red on broken config; grep-rule red on planted violation) executed on this host.
7. **Provenance chain (rule 18)** — 12 per-finding commits on main; reconstruction docs verified against the tree; the audit itself repaired the one gap (V8-INT-06).
8. **Reconcile in-txn re-walk** — m01 gates green incl. the C-03-pattern defense (`reconcile_in_txn_reverification_blocks_c03_race`).
9. **Fail-open throttle / fail-closed auth split** — `verifier_error_does_not_throttle` green; rule-17 ordering read in `login()`.
10. **Supply-chain gates with live inversion** — `cargo audit --deny warnings` green with the list, proven red without it (V7 record; exit-0 re-verified here); deny all-ok.
11. **Honest docs** — M-16 caveat (README:171), ciphertext-only residual (threat-model), quotas-not-enforced, no-healthcheck rationale: all survived V7's doc churn (checked §6/§5.5).

## 9. Full-Sweep Results & Gap Map

**Per-area dispositions** (dedup rule 11 applied; V8-NEW items carry IDs):

| Area | Disposition |
|---|---|
| 7.1 API parity | **One P2 (V8-NEW-02)**: no upload idempotency keys — client retry after timeout creates a second manifest (chunks dedup; name-replace soft-deletes the old; no budget/chunk leak → P2 design gap, not P1). Parity matrix: M-04/M-10 hold; ListFiles 1M-tenant materialization maps to V6-08+M-03 (no double-report). `buf breaking` needs a tagged baseline commit for its `--against` (ops note). Versioning statement absent beyond `/api/v1`/`.v1` (docs-only P2). |
| 7.2 Config surface | **V8-NEW-01 (MEDIUM)**: `ip_lockout_cap_ms < lockout_cap_ms` is config-expressible and boot-silent — violates the documented V6-15 ordering invariant; add to `validate_for`. Otherwise clean: all Wave-3 keys have serde defaults, examples, and docs; C-01 secret floors enforced. |
| 7.3 Failure injection | ENOSPC-mid-write probe absent (P2 — tmpfs probe recommended for V9's CI); crash-window coverage = the runtime barrier gates + soak; clock sites: boot sweep uses wall-clock ages (documented, matches orphan semantics), throttle uses wall-clock ms (backward jump shortens lockouts — fail-open direction, acceptable); degenerate inputs covered by property+fuzz+chunker tests; H-04 clamp verified again (allocation never driven by recorded sizes). |
| 7.4 Unsafe/panic | **Clean.** 2 `unsafe` blocks (config.rs env-var test helper — justified by test isolation). Panic inventory: 14 non-test sites, all boot-time one-shots or logic invariants (table in audit notes; zero network-reachable). INFO: poison-handling inconsistency (V8-INT-07). |
| 7.5 Tenant isolation | Clean at the deployed posture. `/internal` auth: constant-time api key both transports (loopback binds default in compose); plaintext-mode dedup oracle = M-16 (§6, C-6); dedup-hit timing intra-tenant: documented accepted-risk absent → one-line threat-model addition (docs-only); tenant deletion/crypto-shredding story absent → gap map row. Quota race: N/A (no quotas — documented). |
| 7.6 Replication | Consistency model **documented and matches code** (eventual, bounded ≤queue_capacity window, ADR-linked). Queue bounded (mpsc + M-14 counted drop + lag metric now live). Apply-failure: bounded retry + backoff, lag residue visible. INFO: backoff without jitter (herd needs ≥2 followers — P3). Same-path concurrent writers: single-writer-per-name invariant is emergent (last-committed manifest wins the name index; deterministic by UUIDv7 order) — documented in index tests, add one sentence to replication.md (docs-only). |
| 7.7 Ops lifecycle | Backup: procedure + restore-to-replica validation trick documented (backup-restore.md) but **no rehearsed automated restore test** → gap-map row 1 (V9, S — restore fixture into temp dir + verify, per contract's P1 call). Key-compromise runbook EXISTS (runbook.md Scenario 5) → gap row closed. Graceful shutdown: joint drain + replication flush + final-flush logging (M-02/M-14/V6-04) — code-verified; kill-procedure documented. Capacity guide, SLOs, correlation-id spanning REST→replication: absent → gap rows (docs-only P2). Upgrade marker: redb has no schema-version gate → gap row (V9 with the format rewrite — natural fit). |
| 7.8 Code quality | Typed errors throughout (`thiserror` at the core, anyhow only at the binary edge); no-handler-validation holds (newest handlers read end-to-end, not just grepped); zero TODO/FIXME/XXX/HACK markers; `cargo tree --duplicates`: known duals only (getrandom majors, two codecs by design). Test hygiene: sleeps only in soak/mtime-aging (justified); no locale-dependent assertions. |
| 7.9 Docs accuracy | README claims spot-audited this round: test count now reconciled (V8-INT-01 fix should update README:181 "131 unit" → 166 — docs-only); threat model updated for Wave-3 surfaces (throttling.md residuals, repair postures) — **current**; runbook "first 10 minutes" checklist exists in troubleshooting.md (adequate). |

**Gap map to senior product** (all rows verified against the tree):

| Capability | Current state | Gap | Effort | Wave |
|---|---|---|---|---|
| Backup restore rehearsal | Procedure + manual replica-trick documented | No automated drill test | S | V9 |
| Dir-fsync discriminating test (V8-REG-01) | Code correct, untested | Failpoint seam or strace CI job | S | V9 |
| Upgrade/rollback marker | No schema version gate | Add + policy ("new opens old; never reverse") | S | V9 (with rewrite) |
| M-16 scoped keys | Caveat-only | Migration + default flip | M | **V9 hard** |
| V6-08+M-03 summary rows/cursors | Full materialization today | Rewrite + design doc (C-7) | L | V9 |
| V6-10 group-commit | Per-chunk txns | Fold into rewrite | M | V9 |
| Upload idempotency (V8-NEW-02) | Retry = second manifest | Design (idempotency key or name-addressed PUT) | M | V9/V10 |
| Boot-validation lint (V8-NEW-01) | Missing | validate_for extension | S | V9 |
| SLOs + alert rules | Metrics exist, no targets | docs + dashboards | S | docs/V10 |
| API versioning statement | Implicit (`/api/v1`, `.v1`) | Deprecation policy page | S | docs |
| Capacity guide | Absent | Derive from Linux baseline (C-2 artifacts) | S | docs |
| Tenant lifecycle/crypto-shredding | Absent (key destroy exists via file rm; no purge-xtask) | xtask + runbook section | M | V10 |
| Distributed-throttle note | Documented (throttling.md) | — | — | closed |
| Consistency semantics page | Documented + matches code | Add single-writer sentence | XS | docs |

---

## 10. V9 Plan Skeleton

1. **Design gate (C-7):** `docs/design/storage-format-v2.md` — cursor walk, summary rows, dual-read migration, tenant-scoped keys (M-16), group-commit (V6-10), schema-version marker.
2. **Rewave execution:** migration + M-16 flip + V6-08/M-03/V6-10 behind the format work; V8-REG-01/02, V8-NEW-01, V8-INT-02 as the S-effort opening increment.
3. **Ops increment:** backup restore-drill test (C-9), Linux baselines (C-2), fuzz vectors + non-UTF8, capacity guide from CI artifacts.
4. **Docs increment:** idempotency design, versioning policy, SLOs, consistency-page sentence, README test-count correction (V8-INT-01).
5. **Exit:** re-audit V9 → **GO** requires C-1..C-7 done + M-16 landed + Linux baseline (I-07 closed).

*The one-sentence test: a fresh engineer learns that the P0 surface is fixed and verified locally but awaits its first Linux CI run (C-1); that 10 named items remain with owners, phases and an M-16 hard deadline; that eleven production-grade assets are proven and safe to build on; and that the gap map above is the exact distance to launch.*
