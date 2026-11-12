# File Storage Re-Audit Report — V6

**Audit date:** 2026-11-12 · **Tree:** post-Wave-2 working tree on `main` (V3 baseline commit `e4cd5a6` + ~2,577 insertions across 42 files + 7 new modules) · **Auditor:** independent senior Rust systems/security review (ZCode, V3 lineage)

---

## 0. Verdict

**NO-GO** — by a narrow margin and for a small, well-defined V7 scope.

The Wave-1/2 remediation is substantially real: all three V3 CRITICALs are closed at the code level, the shared validation layer, verify-then-persist repair, reconcile pass, and two-phase GC barrier are genuine, well-tested engineering. But this audit found (a) a **HIGH functional regression in the core upload path** (gRPC declared-size uploads are double-charged against the admission budget and abort mid-stream whenever size > budget/2 — V6-02), (b) a **load-bearing CI security gate that provably can never pass** (V6-05 — the "C-01 boot story verified LIVE in CI" claim is not supportable), (c) the **M-06 boot sweep is ineffective** for its actual targets (`.tmp-*` and `.gc-pending` live in shard subdirectories the non-recursive sweep never visits — V6-03), and (d) a narrow **residual C-03 window** plus a dead replication-lag metric (V6-07/V6-04). Conditions to flip to GO-WITH-CONDITIONS: the six V7-P0 items in §14, all small (S effort each).

---

## 1. Executive Summary (≤500 words)

The V6 re-audit read 100% of the Wave-1/2 change surface (every diff against the V3 baseline `e4cd5a6`, all seven new modules end-to-end, the CI workflow, all gate scripts, and the regression/property suites) and re-verified every closed finding against code rather than the remediation report.

**Confirmation:** 21 of 23 re-verified rows are `fixed` or `fixed-with-caveat` (§15). C-01's fail-closed validation, secretless image, and compose hardening are real and strong. C-02's replica purge is metadata-only on both internal protocols with a NodeRole guard. C-03's verified registration + uploader self-heal + two-phase GC barrier collapse the original whole-stream race window to a sub-transaction sliver. H-02/H-03/H-04/H-05 are correct as designed, with exhaustive gates.

**Regression:** the fixes introduced four real bugs. The most serious: the gRPC upload bridge charges every streamed byte **on top of** the declared-size reservation, so any declared upload over half the byte budget aborts mid-stream (V6-02, HIGH). `UploadGate` releases and re-acquires its permit across an unguarded gap and `.expect()`s success — a preemption lottery panic in the request path (V6-01). The M-02 "lag" half of the replication observability fix is dead code: `acked()` has zero call sites, so the lag gauge is never published and `queue_depth()` counts monotonically (V6-04). And the CI workflow's c01-boot-gate job has two independent defects (pipeline exit-code capture without pipefail; a grep for a word the binary never prints) that make it permanently red — meaning either CI never ran on this tree or its failures were ignored; the "verified LIVE" claim fails (V6-05, plus the unreachable nightly job that hosts the only H-01 memory-bound gate, V6-06).

**Discovery:** the boot sweep is non-recursive and never cleans what it claims to clean (V6-03); reconcile materializes every manifest despite its "batched reads" doc (V6-08); a replica missing a tenant key quarantines valid ciphertext as "hostile" (V6-09); per-chunk registration replaced the V3 single-transaction batch with per-chunk fsynced transactions (V6-10); REST chunked uploads never touch the byte budget (V6-11); the `no-handler-validation` grep has trivial evasions (V6-13); login throttling has no per-IP layer on gRPC and enables arbitrary-username lockout DoS (V6-15, moderated).

**Deferred re-assessment (§13):** M-09, M-10, M-16 should be pulled into V7 (all unblocked by Wave-2 structure); M-12 stays deferred; M-03's priority rises (reconcile added a second full-scan path).

The codebase is close. The V7-P0 set is six small fixes plus an honest re-run of CI; with those, a bounded launch is defensible.

---

## 2. Methodology & Evidence Base

**Baseline:** the working tree was diffed against the V3-audited commit `e4cd5a6` (`git diff HEAD -- src/` → 42 files, +2,577/−447, plus untracked new modules), giving an exact, closed change surface. Every changed file and all seven new modules were read end-to-end: `core/validation.rs` (372), `core/gate.rs` (187), `core/bounded_map.rs` (112), `security/throttle.rs` (185), `storage/quarantine.rs` (206), `api/middleware/{metrics_guard,bounded_field_probe}.rs`, plus the reworked `gc.rs` (499), `index_db.rs` (1,363), `pipeline.rs` (401), `retrieval.rs` (324), `config.rs` (808), `state.rs` (276), `upload.rs` (389), `file_service.rs` (572), `replicator.rs` (344), and all remaining diffs. CI workflow, `scripts/*.sh`, `poc_expect_fail.sh`, `fuzz/`, all regression/property tests, README/CHANGELOG and the Wave-2 docs were read.

**Host limitation (same as V3, honesty clause):** Windows host, no WSL/Docker, so the 253-test suite, benches, clippy, and the Rust reproductions could not be executed here (M-13 fixed the compile barrier; the sandbox simply lacks the toolchain). Consequences: (a) findings are graded `High (trace)` / `Suspected`, never `Confirmed (executed)` — except the CI static checks, which **were executed** (`bash audit-repro-v6/ci-checks.sh` → 8/8 defect rows verified on this host); (b) bench re-run, RSS-bound, fuzz, and `buf breaking` re-verification are marked NOT RE-RUN (§7, §9); (c) test counts were re-derived by grep: **253 test functions** (160 src unit + 19 gRPC + 16 REST + 45 regression/property + 1 C-01 boot + 2 misc) vs. the claimed 251/`143 unit` — minor count drift in the claims, same ballpark.

**Provenance caveat (finding V6-17):** the remediation reports describe 18 Wave-1 + 13 Wave-2 commits on `fix/*` branches and a Wave-1 squash snapshot `f47c474`; this repository's history contains **none of that** — `main` still points at `e4cd5a6` and the entire Wave-1+2 tree is uncommitted working-tree state with no fix branches. The audit therefore verified the *state*, not the *history*; the branch/commit protocol claims are unverifiable from this repo.

**Effort split:** ~30% confirmation, ~40% regression+discovery, ~30% deferred re-assessment + verdict (per charter §0.1).

---

## 3. Strengths (≥12, with evidence)

1. **C-01 fail-closed validation is genuinely thorough** — `config.rs:560-660`: all violations collected in one pass, ≥32-byte JWT / ≥24-byte api-key floors, shipped-literal deny-list (`FORBIDDEN_SECRET_MARKERS`, config.rs:10-21) enforced in **both** profiles, plaintext passwords rejected in release builds, argon2id-PHC shape-checked; gated by `c01_config_rejects_every_shipped_literal` etc. and the binary-level `regression_c01_boot.rs`.
2. **The deployment story matches the code** — Dockerfile has no baked config (`CMD ["--config", "/run/secrets/file-storage.yaml"]`), compose uses `secrets:`, nonroot `65532`, loopback port binds, per-node `FS_REPLICATION__ROLE`.
3. **M-04's shared validator is real DRY** — REST and gRPC handlers reduce to field extraction + `validate_upload` (`upload.rs:124-138,248-267`, `file_service.rs:83-107`); `list_filtered` (index_db.rs:758-795) is the single list/count path; the `include_deleted` admin gate exists in exactly one place (`validation.rs:126-141`) and both transports now 403/PERMISSION_DENIED viewers.
4. **The two-phase GC barrier is correctly ordered** — `gc.rs:238-273`: pre-check → in-txn `finalize_sweep` (index_db.rs:333-353, removes only refcount-0 rows) → atomic `.gc-pending` rename claim (block_store.rs:174-186) → post-claim resurrection re-check → delete-or-restore; and `restore_claimed` handles the "canonical rewritten meanwhile" case explicitly instead of renaming over it (block_store.rs:188-203).
5. **C-03's verified registration collapses the original window to one syscall** — `register_chunks_verified` performs the blob-existence check *inside* the registration transaction and refuses to write rows for vanished blobs (index_db.rs:239-291); the uploader self-heals by CAS-rewrite (`pipeline.rs:347-373`) with a bounded retry and a clean 409.
6. **H-01's fused pipeline actually deletes whole-file retention** — `StoredChunk` carries metadata only (`pipeline.rs:44-58`); replication enqueues per chunk while bytes are local (`pipeline.rs:377-388`); `GateLease` releases on Drop for success/error/panic paths (`gate.rs:60-110`), with gate unit tests covering release-on-abort and budget-line aborts.
7. **H-02's bounded multipart readers are textbook** — `read_bounded_field` errors on the first byte past the cap (`upload.rs:357-374`), unknown fields are drained not buffered, and the field count is capped at 32; verified against the V3 PoC shape in `poc_expect_fail.sh` (64 MiB tags part → 413).
8. **H-03 is wired with an explicit, loud opt-out** — `pipeline.rs:166-170` validates the declared MIME against the first chunk's magic bytes inside the pipeline (enforce default, `ValidationMode::Off` warns at boot, state.rs:110-116); serving side adds `nosniff`, risky-MIME `attachment`, and `Vary: Authorization` (`download.rs:222-273`) closing L-10 as claimed.
9. **H-04's clamp is wired from the real chunking bound** — `with_max_plain(chunking.max)` at state.rs:75-79; `decompress` clamps to `min(size_hint, max_chunk_plain)` and treats any hint mismatch as Corruption, never an allocation size (`compressor.rs:83-110`); peer-supplied `size_stored` is cross-checked on both internal apply paths (`internal.rs:177-184`, `replication_service.rs:204-211`).
10. **M-07 verify-then-persist is correctly ordered for keyed nodes** — `repair_chunk` decodes-for-verify (through the same clamped `decode`) and BLAKE3-checks the plaintext *before* `blocks.write`; mismatches quarantine, invalidate L2, tick the alarm metric, and return Corruption (`retrieval.rs:180-255`); quarantine is content-addressed, byte-bounded with oldest-mtime eviction, and lives outside the CAS walk (`quarantine.rs`).
11. **M-01's tombstone decision really is in-txn re-verified** — `tombstone_unreferenced_chunks` re-walks the manifest table inside the same `Durability::Immediate` transaction that zeroes the refcounts (index_db.rs:382-436); because redb write transactions are serialized, a manifest commit either precedes (seen) or follows (its register resurrects the refcount) — the C-03 pattern is structurally defended here.
12. **BoundedMap kills the wholesale-clear bug class** — `core/bounded_map.rs`: one oldest-entry eviction per insertion beyond the cap, shared by the login throttle and the per-tenant RPC governor (I-06 closed); the governor's fresh-bucket accounting even avoids the old first-request-free-token quirk (rate_limit.rs:66-89).
13. **The regression suites are behavioral and adversarial** — `race_gc_upload_x100` runs real concurrent upload/purge tasks with randomized in-window pauses and asserts read-back integrity (`tests/regression/race_gc.rs`); `poc_expect_fail.sh` re-runs the V3 black-box PoCs as expect-fail against a live compose stack.
14. **Docs honesty remains above the bar** — README states the M-16 plaintext-oracle caveat with the open ticket (README:171); threat-model.md:87 documents the ciphertext-only residual with the exact runbook guidance; `docs/src/security/throttling.md` and `operations/bootstrap.md` exist.

---

## 4. Findings Catalog

Table format: `id | severity | category | file:line | title | impact | exploitability | confidence | fix | effort | phase | status`.
Full detail per finding; reproductions in `audit-repro-v6/`.

### [V6-01] MEDIUM · regression(H-01) · `src/core/gate.rs:118-134` — UploadGate permit release/re-acquire race panics in the request path
`acquire_permit` does `try_acquire_owned().map(|_| ())` — the permit is **dropped (released)** inside the closure; `take_permit` then re-acquires and `.expect("permit reserved by acquire_permit")`s. A sibling task stealing the released permit between the two calls panics. Interleaving: `audit-repro-v6/INTERLEAVINGS-v6.md §V6-01`; hammer test `audit_repro_v6.rs::v6_01_*`. Impact: spurious 500/INTERNAL for uploads near `max_concurrent_uploads` capacity; violates the zero-panic-in-request-path bar the remediation itself set. Fix: return the permit from `acquire_permit` and move it into the lease (S).

### [V6-02] HIGH · regression(H-01) · `src/grpc/services/file_service.rs:162-183` + `gate.rs:88-110` — Declared-size gRPC uploads double-charged; abort mid-stream above budget/2
`try_reserve_known(expected)` reserves `expected` bytes, then the stream bridge calls `lease.charge(n)` for **every** Data message of the same upload — total budget consumption 2×size, abort when `expected + streamed > budget`. Any declared upload with `expected_size > max_in_flight_bytes/2` (e.g. 1.1 GiB against the 2 GiB default, while `max_file_size` defaults to 10 GiB) fails mid-stream despite being admissible; smaller ones double-consume and starve other uploads. Arithmetic reproduction: `audit_repro_v6.rs::v6_02_*`. Gate tests missed it because their fixtures satisfy 2·S ≤ budget. Fix: charge only unknown-size leases (S). Exploitability: Trivial (any gRPC client setting expected_size). Confidence: High (trace; the arithmetic is deterministic).

### [V6-03] MEDIUM · regression(M-06/W1-C-03) · `src/state.rs:242-274` — Boot sweep is non-recursive: shard-dir `.tmp-*` and `.gc-pending` files are never cleaned
`boot_sweep_tmp` `read_dir`s only `storage.root` and `storage.temp_dir` (top level). Every `.tmp-<uuid>` blob is created in a shard dir (`block_store.rs:72`, `dir = root/ab/…`), and every `.gc-pending` claim likewise (`block_store.rs:174-186`) — neither directory level is ever visited, and the root-level name/age filters never match anything that exists there. The M-06 fix ("crash loops no longer grow disk monotonically") and the W1-C-03 boot-recovery claim ("boot sweep discards crash-orphaned `.gc-pending` files") are both **ineffective as implemented**: crash loops leak full-size blob copies into shard dirs indefinitely. Reproduction: `audit_repro_v6.rs::v6_03_*`. Fix: recursive walk to `shard_depth + 1` mirroring `list_all` (S).

### [V6-04] MEDIUM · regression(M-02) · `src/replication/replicator.rs:96-141,276-292` — Replication lag observability is dead code; counter counts monotonically
`acked_lag` increments on every enqueue **including dropped ops** (the M-14 timeout path returns `Ok(())` without decrementing), and `Replicator::acked()` — the only decrementer and the only caller of `set_replication_lag` — has **zero call sites**; `worker_loop` computes `let acked = …; let _ = acked;`. Consequently `file_storage_replication_lag_ops` is never published, and `queue_depth()` grows forever. M-02's claimed "counter/gauge/lag split" is one-third real (the queue-depth gauge is correct). Reproduction: `audit_repro_v6.rs::v6_04_*`. Fix: pass an `Arc<AtomicU64>` (or the lag handle) into `worker_loop` and call `acked(n)` after each successful flush; decrement on drop (S).

### [V6-05] HIGH (process/meta) · regression(CI-W2/M-15) · `.github/workflows/ci.yml:58-71` — c01-boot-gate job can never pass; "verified LIVE in CI" unsupported
Two independent defects: (a) `binary | tee /tmp/boot.txt; code=$?` captures **tee's** exit status (no `pipefail`), so `test "$code" -ne 0` always fails the step; (b) the required `grep -qiE "violation"` can never match — the boot error is `invalid configuration` (lib.rs:88) plus per-field messages; the word "violation" exists only in comments/test names. **Executed verification:** `bash audit-repro-v6/ci-checks.sh` → 8/8 defect rows confirmed on the audit host. Consequence: the C-01 live-enforcement claim fails, and either this job has never run green on this tree or its red status was ignored — both are process failures for a gate the remediation calls load-bearing. Fix: `set -o pipefail` (or run the binary without the pipe), grep for `invalid configuration|jwt_secret` (S).

### [V6-06] MEDIUM · regression(CI-W3/4/H-01) · `.github/workflows/ci.yml:131-140 (nightly)` — Nightly job is unreachable; the only H-01 memory-bound gate never runs anywhere
The `nightly:` job is gated on `github.event_name == 'schedule'`, but the workflow's `on:` defines **no `schedule:` trigger** (only push/pull_request) — the event can never fire. The job contains the only reference to `h01_upload_memory_bound -- --include-ignored` (the #[ignore]d RSS-bound test, hardening.rs:742-747). The H-01 "O(chunk) memory under load" claim is therefore enforced nowhere. Also noted: no `permissions:` block on the workflow (github-script comment step depends on repo-default token rights), and the `binary-size` job runs only on PRs. Fix: add `schedule: [{cron: ...}]`, move the memory gate to a reachable job, add minimal `permissions:` (S).

### [V6-07] MEDIUM · missed-by-V3-variant / fixed-with-caveat(C-03) · `index_db.rs:239-291` vs `gc.rs:238-273` — Residual sub-transaction window: GC claim+delete between upload's blob check and commit leaves a dangling row
The blob-existence check runs inside the upload's write transaction, but the GC barrier's claim/read/delete are filesystem-and-read-txn operations that can interleave: upload sees blob present (t0) → GC claims (t1) → GC's `chunk_is_referenced` read sees no committed row (t2, MVCC) → GC deletes the pending copy (t3) → upload commits (t4) → live row, blob gone. Window = the upload transaction body (sub-ms to low-ms with `Durability::Immediate` fsync) — far too narrow for `race_gc_upload_x100` to reliably hit, which is why the gate stays green. Downstream: reconcile reports it as a phantom; Repair does not fix phantoms on a primary. Trace + enforced-ordering test: `INTERLEAVINGS-v6.md §V6-07`, `audit_repro_v6.rs::v6_07_*`. Cheap complete fix: post-commit `path_exists` re-assert + one rewrite in `store_chunk` while the bytes are still in scope (S). *Why V3's C-03 fix verdict was caveated:* the V3 report anticipated "epoch/second-chance"; the landed design narrows but does not eliminate the window.

### [V6-08] MEDIUM · new(M-01) · `src/storage/index_db.rs:355-368,382-436` — Reconcile materializes every manifest and every refcount>0 row despite "batched reads" doc
`collect_referenced_hashes` documents "Batched reads keep peak memory at the referenced-set size" but calls `all_manifests()` — a full `Vec<Manifest>` including every chunk vector; `tombstone_unreferenced_chunks` additionally buffers every refcount>0 row. On a 1M-file store this is the V3 M-03-scale memory hazard inside a *new*, hourly background path (plus the `all_manifests()` calls already in GC). Reproduction (peak-set semantics): `audit_repro_v6.rs::v6_08_*`. Fix: stream manifests via a range iterator inside the read txn; feed the tombstone walk from a cursor (M). Related: the reconcile lock is stealable after 1h (`gc.rs:427-446`) on the assumption passes are short — which this very finding undermines; and `phantom_manifests` counts phantom *chunk rows*, not manifests (naming).

### [V6-09] LOW · new(M-07) · `src/storage/retrieval.rs:196-215` — Missing tenant key is misclassified as hostile upstream bytes
With encryption enabled but the tenant key file absent (wiped/misconfigured `keys_dir`), `decode` fails at decrypt and `repair_chunk` takes the "undecodable = hostile" branch: valid ciphertext is **quarantined** and the caller gets `Corruption` — indistinguishable from a compromised primary, with no `repair_key_missing` signal (the V6 charter explicitly asked for this distinction). Evidence: `audit_repro_v6.rs::v6_09_*`. Impact: evidence pollution displacing real hostile bytes (bounded), confusing alarms, persistent read failures misattributed to the peer. Fix: classify decrypt-with-missing-key separately (metric + error variant, skip quarantine) (S).

### [V6-10] LOW · regression(H-01 design trade) · `pipeline.rs:334-367` — Per-chunk registration replaced the per-file single-fsync transaction
V3's `register_chunks` batch committed all of a file's refcounts in one `Durability::Immediate` transaction; the fused pipeline registers per chunk — N chunks now cost N fsynced write transactions (plus N blob fsyncs + N dir fsyncs). The 2 MiB bench (≈16-64 chunks) hides this within noise (253 MB/s claim is plausible); a 1 GiB upload (~8k chunks) pays ~8k extra fsyncs. Durability semantics remain sound (each txn consistent; M-01 reconcile heals the wider abort-leak windows). Note + possible group-register micro-batch (S/M, P2 with the deferred group-commit item).

### [V6-11] LOW · regression(H-01) · `src/api/handlers/upload.rs:63-79` — REST chunked uploads never charge the byte budget
The REST handler reserves by `Content-Length` (or unknown) but never calls `lease.charge` while streaming (the only call site is the gRPC bridge). Chunked transfer-encoding (no Content-Length) uploads are bounded only by the concurrency semaphore and the post-hoc `max_size` check in the pipeline (which fires *after* chunks are stored). Combined with V6-02, the two transports implement opposite wrong models. Fix: charge in the raw-body stream adapter, skip charging known-size leases (S).

### [V6-12] LOW · new(M-07) · `src/storage/quarantine.rs:56-105` — Quarantine store does a full directory scan+sort per hostile chunk
A hostile upstream can trigger repair mismatches continuously; each `store` lists and stats the entire quarantine dir and sorts by mtime. With the 1 GiB default bound and 8 KiB chunks that is up to ~131k files scanned per hostile chunk — an O(n) I/O amplifier on the replica's async pool under sustained attack (bounded by `quarantine_max_bytes`, so disk is safe; CPU/IO churn is not). Fix: track a running byte total in memory; only walk on boot (S).

### [V6-13] LOW · new(M-04) · `scripts/check_no_handler_validation.sh:19-21` — The no-handler-validation grep has trivial evasions
The pattern matches `validate_*|check_size|len() <digits>`; `name.chars().count() > 255`, comparisons against named constants (`MAX_NAME`), or renamed helpers evade it. **Executed verification:** `ci-checks.sh` §V6-13 shows three accepted evasions. The structural alternative the charter suggests (constructible-only-in-core `UploadRequestView`) is absent — `UploadRequestView` is a plain pub struct any handler fills. Fix: private-field builder in `core::validation` + keep the grep as a tripwire (V6-DESIGN-01, S/M).

### [V6-14] LOW · new · `pipeline.rs:199-208` — Over-size aborts leak registered chunks until the hourly reconcile
The `size_original > max_size` check fires per stored chunk *after* store/register/enqueue — an over-size upload leaves registered-but-unmanifested chunks (the M-01 leak shape) reclaimed only by the next reconcile (default 1h) or admin trigger. Self-healing exists; note for capacity/ops expectations (INFO-adjacent; documented behavior of reconcile metric). No fix required beyond optionally checking declared size pre-stream (already done for expected_size; Content-Length over max_file_size is pre-rejected at upload.rs:47-50 — so this path is mostly chunked/gRPC-undeclared).

### [V6-15] MEDIUM · fixed-with-caveat(M-05) · `security/throttle.rs`, `auth.rs:133-166` — No per-IP login layer on gRPC; eviction reset; arbitrary-username lockout DoS; no throttle metrics
(a) gRPC `AuthService` has no interceptor; the shared governor is per-tenant and wired only into File/Admin + Upload/Download streaming — gRPC login has **only** the per-username throttle. (b) The 10k-entry insertion-order map lets an attacker evict lockout state with 10k+ unknown-username attempts, resetting a victim's exponential ladder (each new failure rebuilds it from 1s, so the win is a modest brute-force rate multiplier — MEDIUM not HIGH). (c) ~11 bad passwords lock **any known username** for 15 min (rule-17 tradeoff, but not documented as a residual in throttling.md). (d) No `login_throttle_*` metrics — lockouts visible only via audit rows. Trace: `INTERLEAVINGS-v6.md §V6-15`. Fix: per-IP throttle layer (shared BoundedMap) on both auth surfaces + metrics + doc the lockout-DoS residual (M).

### [V6-16] INFO · regression-note(H-03) · `pipeline.rs:150-190` — Validation runs concurrently with sibling chunk writes; "before any persistence" comment overstates
`validate_head` executes inside chunk 1's async body while `buffer_unordered(8)` drives chunks 2..8 concurrently — an invalid-MIME upload can persist up to 7 chunks before the error aborts the stream. Orphan blobs only (GC-able); fix by gating the buffer on first-chunk validation or accept + fix the comment (S/INFO).

### [V6-17] MEDIUM (process) · new · repo history — Wave-1/2 commit/branch provenance is unverifiable in this repository
`main` is unchanged from the V3 baseline (`e4cd5a6`); the entire Wave-1+2 state is uncommitted working-tree changes; the claimed 18+13 `fix(<ID>)` commits, `fix/*` branches and squash snapshot `f47c474` do not exist here. The audit verified the tree state, not the claimed change-control history; for a system whose remediation process is now part of the security story (per-finding gates), the history should be committed and branch-protected (S, process).

### [V6-18] LOW · deferred-reassess · `gc.rs:427-462` — Cross-process locking is reconcile-only; GC sweeps and multi-process shared data dirs are unguarded; stale-lock steal after 1h
The in-process `busy` mutex excludes GC/reconcile within one process, and the create-new lock file excludes concurrent *reconciles* cross-process — but GC passes have no cross-process lock, and a reconcile running >1h has its lock stolen (assumption "passes are short" contradicted by V6-08's scale analysis). Shared-dir deployments are unsupported-but-unguarded. Fix: same lock for both, heartbeat/refresh the mtime (S).

### Ops/LOW additions (no separate deep-dives)
- **V6-OPS-01** compose has no `mem_limit`/`cpus`/logging-size limits; Dockerfile lacks `HEALTHCHECK` (distroless note aside). (LOW)
- **V6-OPS-02** no `audit.toml`; the "2 allowed advisories" are cargo-audit *warnings* by default, not a reviewed allow-list — acceptance rationale undocumented; `cargo audit` in CI runs without `--deny warnings`. (LOW)
- **V6-OPS-03** `--skip platform_unix` in the windows job matches **zero** test names (grep-verified) — the skip is dead (if the full suite is green on windows, remove it; if not, the job is red). Also `cargo test` there lacks `--locked`. (LOW)
- **V6-OPS-04** `dashboard_smoke.sh` asserts series *existence* (with traffic seeding — good) but never values; acceptable, note. (INFO)
- **V6-OPS-05** fuzz target covers `name` only (fixed mime, empty tags, no expected_size) and skips non-UTF8 (fine for a `&str` API, but tags/mime paths are unfuzzed). (LOW)
- **V6-OPS-06** binary-size cap is static (15 MiB); baseline never updates on shrink — acceptable, documented per charter hint. (INFO)
- **V6-OPS-07** fuzz/`cargo audit`/`deny`/`buf breaking` re-runs not executable on this host — §9 marked unresolved-in-audit. (INFO)

---

## 5. Architecture Assessment

Post-Wave-2 the layering is materially cleaner than V3: `core::validation` is a real seam (handlers extract, core decides — enforced culturally by CI and structurally by nothing, see V6-13); `core::gate` and `security::throttle` are small, single-purpose controls shared across transports via `AppState`; storage internals (`register_chunks_verified`, `finalize_sweep`, `claim_for_gc`) now form a coherent CAS-lifecycle protocol shared by GC, purge and reconcile.

The mud that remains: (1) **the upload admission model is split-brained** — REST never charges streamed bytes, gRPC double-charges declared ones (V6-02/V6-11); the correct model (reserve-known-or-charge-streamed, never both) exists in `gate.rs`'s API but neither transport implements it. (2) **The replication observability layer is half-wired** — counters exist, the worker sets the depth gauge, but the lag plumbing dead-ends (V6-04). (3) **Boot-time hygiene is centralized in `state.rs` but written against the wrong directory shape** (V6-03) — the sweep should reuse `BlockStore`'s own walker. (4) M-08's dual internal protocols remain (deferred), with the HTTP/gRPC purge parity now correct (C-02) but apply-core still duplicated.

Best three decisions of the wave: the in-transaction blob verification (C-03), the in-txn manifest re-walk (M-01), and the two-phase claim ordering (W1-C-03) — together they convert the V3 "refcounts hope" model into a verify-then-commit protocol at every transition. Worst three: the gate's release/re-acquire permit dance (V6-01), the gRPC charge-on-top-of-reservation (V6-02), and shipping a CI gate whose correctness was asserted rather than run (V6-05).

---

## 6. Security Assessment (threat-model re-walk)

Per the charter, the re-walk covers every asset with post-Wave-2 barriers and residuals:

| Asset | Reach | Auth | Integrity | Availability | Durability | Wave-2 delta / residual |
|---|---|---|---|---|---|---|
| Per-tenant blobs | REST/gRPC public; internal API | JWT role+tenant; api key | BLAKE3 verify every L1 miss; **repair verify-then-persist (M-07)**; H-04 clamp | UploadGate (buggy halves V6-02/11); throttle+governor | WAL 64/5s async (drop counted, M-14) | M-07 closed the poison-on-repair hole for keyed nodes; ciphertext-only posture documented; **key-missing misclassified (V6-09)** |
| Manifests | same | tenant predicate everywhere (verified per handler) | msgpack; size caps on read paths (M-04c) | reconcile lock (stealable, V6-18) | redb Immediate | phantom detection added; phantoms not repaired |
| JWT secret | config/secret file | C-01 floors+deny-list; rotation runbook scenario 4 exists | — | login throttle (residuals V6-15) | — | still single-secret HS256 (I-01 deferred) |
| Cluster api key | internal API, gRPC replication | constant-time; ≥24 bytes; per-node in compose | HMAC still absent (M-08 deferred) | — | plaintext channel (documented) | unchanged deferral |
| Argon2 hashes | secret file | argon2id-only in release (C-01) | PHC shape check | **throttle before verify protects the 64 MiB budget** | — | rule 17 verified in code (VerifierError not counted) |
| Replication channel | cluster net | api key | M-07 verify for keyed; size_stored check (H-04) | enqueue timeout + counted drop | async RPO documented | **dropped ops still count into "lag" (V6-04)** |
| Admin endpoints | REST/gRPC | admin role; reconcile/GC audited (M-01) | — | reconcile lock; no cooldown → admin can spam full-store walks (charter V6-SEC-03 analog: LOW, admin-only) | audit rows | new reconcile trigger surface audited on both transports |
| Metrics endpoint | `/metrics` | loopback default / api-key / public+warn (H-05) | — | bounded labels | — | solid; compose binds loopback |
| Quarantine dir | filesystem only | — | content-addressed; outside CAS walk | **byte-bounded, oldest-evict (V6-12 scan churn)** | boot sweep (works — dir is flat) | new ops surface, runbook covers inspection |
| Reconcile lock | filesystem | create_new file | — | exclusive cross-process | steal after 1h | V6-18 |
| `.gc-pending` claims | filesystem | — | two-phase ordering | — | **boot recovery claim false (V6-03)** | crash between phases leaks blob copies forever |
| CI workflows | repo | GITHUB_TOKEN only; no `pull_request_target`; secrets self-generated in-job ✓ | — | — | — | **c01 gate broken (V6-05), nightly dead (V6-06), no permissions block** |

New attack surface introduced by the fixes, summarized: quarantine (bounded, noisy-but-safe), throttle map (eviction games, V6-15), reconcile lock (steal, V6-18), `.gc-pending` (leak, V6-03), CI (the actual weakest link — V6-05/06/17). An adversary who knows the patch set probes: declared-size gRPC uploads (V6-02 — free mid-stream aborts = targeted upload DoS for any user), gate-capacity concurrency (V6-01 500-lottery), and throttle eviction (V6-15). All three are Wave-1/2-introduced.

---

## 7. Performance Assessment

**Bench re-run: NOT POSSIBLE on this host** (no Rust toolchain execution; charter §2.1 requires documented host numbers — recorded as an audit gap). The Wave-2 claimed numbers (cdc 1176 MB/s, zstd 1878 MB/s, upload 253 MB/s, download-cold 29.1 µs) are consistent with the V3-era measurements and the code shapes read; no perf *defects* found beyond: V6-10 (per-chunk fsynced transactions — invisible at 2 MiB, material at GB scale; recommend a ≥512 MiB bench fixture before V7 sign-off), the M-07 cold-repair claim ("path untouched") which is **accurate for the cold-download bench** because the repair path only triggers on replicas with `upstream_url` and a blob miss — the bench runs a primary, so it never exercises repair; the repair gates use a mock upstream instead. That answers charter hint §11.2: the bench doesn't exercise repair, and a repair-path bench should be added (LOW). Reconcile wall-clock on large stores is governed by V6-08's full materialization — the 1h default interval is fine only below ~100k files by estimation; add the M-03 summary work before scale (P2 as planned). No flamegraph possible (toolchain); static review found no new lock-on-hot-path (throttle `check` is a mutex'd map read — sub-µs at 10k entries; acceptable, note for V7 per-IP addition to keep the same discipline).

---

## 8. Test Gap Analysis

**What's tested well:** the C-03 race family (x100 stress + enforced interleavings + barrier units), the C-01 boot story (Rust-level, binary-driven), M-04 parity matrix, M-07 repair with a real mock upstream, throttle rule-17 semantics, property tests for validator totality and caps, black-box expect-fail PoCs in CI's docker job.

**Gaps (ordered by risk):**
1. **V6-02's shape is untested:** no gate test with `expected_size > budget/2` (all fixtures satisfy 2·S ≤ budget) — add `h01_known_size_not_double_charged`.
2. **V6-01's shape is untested:** no concurrency test at exact gate capacity on a multithread runtime.
3. **Boot-sweep effectiveness** (`boot_sweeps_stale_tmp` is *planned* per the Wave-1 table and does not exist) — shard-dir leftovers prove the gap (V6-03).
4. The H-01 RSS-bound test exists but lives only in the unreachable nightly job (V6-06).
5. CI job liveness itself: no workflow-level self-check that gates can pass (V6-05).
6. Crash-recovery of `.gc-pending` between phases: asserted in comments, never tested by kill-point injection.
7. Repair-path bench absent (§7); soak/fuzz remain skeletons (CI-W4 acknowledged).
8. Windows job skip-prefix matches nothing (V6-OPS-03) — either dead skip or red job, unverified either way.

---

## 9. Dependency & Supply Chain

**NOT RE-RUN on this host** (no cargo-audit/deny in sandbox) — the charter's requirement to re-verify the "2 allowed" advisories is **unresolved in this audit**: there is no `audit.toml` in the tree, so the "allowed" framing rests on cargo-audit's default of treating unmaintained advisories as warnings, with no recorded rationale (V6-OPS-02). `deny.toml` exists (bans/sources/licenses) but was not executed. Cargo.lock present and CI builds are `--locked` ✓ (windows test job misses `--locked`, V6-OPS-03). `Cargo.toml` diff added no new runtime dependencies (verified — new code uses existing crates). V7 P0 checklist must include a documented `audit.toml` + `cargo audit --deny warnings` with explicit ignores.

---

## 10. Documentation Drift

| Claim | Source | Reality | Verdict |
|---|---|---|---|
| "C-01 boot gate verified LIVE in CI" | REMEDIATION_REPORT Wave-2 / CI-W2 | job doubly broken, cannot pass (V6-05) | **false** |
| "boot sweep discards crash-orphaned .gc-pending files" | REMEDIATION_REPORT W1-C-03 note | sweep never visits shard dirs (V6-03) | **false** |
| "M-06 fixed — crash loops no longer grow disk" | Wave-1 table | ineffective for blob tmp files (V6-03) | **false** |
| "counter/gauge/lag split" (M-02) | Wave-1 table | counter ✓, gauge ✓, **lag dead** (V6-04) | partial |
| "unknown sizes charge as they stream and abort at the budget line" | gate.rs:14-16 doc | true only for gRPC-unknown; gRPC-known double-charges, REST never charges (V6-02/11) | **false** |
| "Batched reads keep peak memory at the referenced-set size" | index_db.rs:355-356 doc | `all_manifests()` full materialization (V6-08) | **false** |
| "M-07 … cold-download regression <5% (path untouched)" | Wave-2 table | accurate — the cold bench runs a primary, repair never triggers; claim meaningless for repair itself | misleading-but-technically-true |
| 251 tests (143 unit) | REMEDIATION_REPORT Wave-2 | 253 by grep (160 src unit) | minor drift |
| README "18 gRPC tests" | README:211 | 19 | minor drift (V3 L-05 persists) |
| "clippy -D warnings clean / fmt clean / audit 0 / deny clean / buf clean" | Wave-2 summary | not re-runnable here; code-level reading found no lint-visible issues; audit/deny unresolved (§9) | unverified |
| M-16 plaintext oracle | README:171 | still open, honestly documented with ticket | true/honest |
| ciphertext-only residual | threat-model.md:87 | accurate, including "run the documented compose mode" (compose is consistently unencrypted, so shipped stack verifies) | true |
| runbook scenarios 1-6, bootstrap.md, throttling.md, ci.md, replication-ops.md | docs/ | present and consistent with code read (reconcile scenario 6 matches handler surface; rotation scenario exists) | true |
| Wave-1/2 commit/branch protocol | REMEDIATION_REPORT | absent from this repo's history (V6-17) | **unverifiable** |

---

## 11. Production Readiness Checklist (V3 §11 re-scored)

- 3.1 Correctness & concurrency: **GO-WITH-CONDITION** — barrier/ordering sound (C-03 caveat V6-07), M-01 in-txn claim verified; conditions: V6-01/02/07.
- 3.2 Security & threat model: **GO-WITH-CONDITION** — C-01/H-03/H-04/H-05/M-05/M-07 real; conditions: V6-15 (per-IP), V6-09.
- 3.3 Resource bounds: **GO-WITH-CONDITION** — gate exists but split-brained (V6-02/11); quarantine bounded (V6-12 churn); throttle map bounded.
- 3.4 Observability: **NO-GO** until V6-04/V6-05/V6-06 — the lag story and the CI enforcement of the metrics/health story are broken; M-01/M-07 metric families verified present in `metrics.rs`.
- 3.5 Reliability & durability: **GO-WITH-CONDITION** — async RPO honest, M-14 works; condition: V6-03 (crash-loop disk growth) + V6-18 note.
- 3.6 Code quality: **GO** — new modules documented, pub-API docs present, one request-path `expect` (V6-01) to remove; `unsafe` none.
- 3.7 Performance: **GO-WITH-CONDITION** — untested at scale post-fusion; condition: GB-scale bench + V6-10 assessment.
- 3.8 Production/deployment: **GO-WITH-CONDITION** — image/compose strong; conditions: HEALTHCHECK + resource limits (V6-OPS-01), reachable nightly/memory gate.
- 3.9 Supply chain: **GO-WITH-CONDITION** — condition: audit.toml rationale (V6-OPS-02).
- 3.10 Documentation: **GO** apart from the four false rows in §10 (fix wording with V7).

---

## 12. Scorecard

| Dimension | V3 | V6 | Justification |
|---|---|---|---|
| Correctness | 5 | **7** | C-02/C-03/W1-C-03 genuinely closed; −1 for V6-02 (upload break), −1 shared with concurrency for V6-07 |
| Concurrency | (in correctness) | **7** | verify-then-commit protocol everywhere; residual sub-txn window + gate race |
| Security | 3 | **7** | C-01/H-03/H-05/M-05/M-07 real, deployment hardened; −1 V6-15, −1 CI-gate integrity (V6-05) |
| Resource bounds | (in perf/dos) | **6** | gate split-brained (V6-02/11), memory gate unreachable (V6-06); throttle+quarantine bounded |
| Observability | 6 | **6** | rich new families but lag dead (V6-04) and enforcement broken (V6-05) |
| Reliability | 5 | **7** | M-14 real, reconcile safety net; −1 for V6-03 crash-loop leak |
| Code quality | 7 | **8** | clean seams, docs, tests-as-design; one request-path expect + dead acked() plumbing |
| Performance | 6 | **6** | unchanged claims, unverified at scale; per-chunk txn trade (V6-10) |
| Production readiness | 4 | **7** | image/compose/CI structure strong; CI liveness + ops gaps cap it |
| Doc accuracy | 8 | **7** | still honest on residuals, but four remediation-report claims are now false |
| **Overall** | 5.1 | **6.8 / NO-GO** | below the 7.0 GO-WITH-CONDITIONS bar — entirely due to small, fixable V7-P0 items |

---

## 13. Deferred Items Re-assessment

| ID | Original deferral reason | V6 re-assessment | Recommendation |
|---|---|---|---|
| M-09 (internal body limit) | router-layer churn | M-05/H-05 work already made `/metrics` a route-layered route; an `/internal` route-scoped limit is now a ~10-line change in the same pattern | **Pull into V7** (S) |
| M-10 (audit actor/IP) | invasive plumbing | `AuthContext` + `UploadRequestView` now flow through both transports; the upload/download audit call sites are single lines in pipeline/download | **Pull into V7** (S) — it also fixes the last V3-era "system-actor" gap |
| M-12 (watcher supervision) | unchanged | unchanged code; gRPC probe still metadata-only vs REST write-probe | **Keep deferred** (P1 of V7 at latest) |
| M-16 (plaintext dedup oracle) | waiting on M-04 | M-04 stable; README already documents the accepted risk with the `dedup.scope` design | **Pull into V7** (M) — charter expected this |
| P2 M-03 (O(n) scans) | scale work | priority **up**: reconcile (V6-08) added two more full-materialization paths; list/count unchanged | V7-P1, paired with V6-08 |
| P2 M-08 (protocol unification) | scale work | unchanged; C-02 parity test now guards the seam | keep P2 |
| P2 M-11 (breaker/dead-letter) | scale work | M-14 landed the bounded-enqueue half; drops are counted and alertable — the residual is replay tooling | keep P2; alert rule on `replication_dropped_ops_total` now |
| P2 group-commit / WAL spool | perf/durability | fold V6-10 (per-chunk txn fsyncs) into the group-commit item | keep P2, add V6-10 |
| P3 fuzz/soak | seams | fuzz skeleton exists but name-only (V6-OPS-05); soak charter item partially unblocked | **Promote soak + fuzz breadth into V7-P1** |
| I-01/I-02/I-03 | future | unchanged | keep P3 |

---

## 14. V7 Remediation Plan

See `REMEDIATION_PLAN_V7.md` for the full plan. Summary: **P0 (blockers, all S effort)** — V6-02 double-charge, V6-01 permit race, V6-05 CI gate repair, V6-06 nightly trigger, V6-03 recursive sweep, V6-04 lag wiring; plus commit the working tree with per-finding history (V6-17) and re-run the full CI matrix red→green. **P1 (30 days)** — V6-07 post-commit re-assert, V6-09 key-missing classification, V6-15 per-IP throttle + metrics, pulled M-09/M-10/M-16, V6-08/M-03 batched reconcile, soak/fuzz breadth, V6-OPS-01/02/03. **P2** — V6-10 group-commit fold-in, V6-12 quarantine accounting, V6-18 lock unification, M-12, remaining P2/P3.

**P0 exit criteria:** all six gate tests land (named in the plan) and the CI matrix — including a repaired c01-boot-gate and a reachable nightly/memory job — is green on the committed tree; `audit-repro-v6/` inverted tests added to `tests/regression/`.

---

## 15. Wave-2 Confirmation Table

| ID | Verification method | Result | Regression test | Notes |
|---|---|---|---|---|
| C-01 | code read (config.rs:560-660, Dockerfile, compose, secrets/) + `regression_c01_boot.rs` read | **fixed** | `c01_*` ×6 + boot test | CI workflow half broken (V6-05) — the code fix stands on its own gates |
| C-02 | code read (replication_service.rs:272-303, gc.rs:100-105 NodeRole guard, internal.rs) | **fixed** | `replication_grpc_purge_shared_chunk`, `purge_semantics_http_equals_grpc`, `replica_refuses_refcount_gc` | purge paths now identical; refcount GC refused on replicas |
| C-03 | code read (register_chunks_verified + self-heal + barrier) + trace analysis | **fixed-with-caveat** | `race_gc_upload_x100`, `race_orphan_sweep_upload`, `fused_register_rejects_missing_blob`, `gc_does_not_delete_reregistered_chunk`, `conflict_maps_to_409…` | caveat = V6-07 sub-txn window; x100 gate cannot certify a µs window |
| H-01 | code read (fused pipeline, gate, L3 byte weigher manifest_cache.rs:20-32) | **fixed-with-caveat** | `h01_*` gates ×5 + units ×4 | caveats: V6-01 panic race, V6-02 double-charge, V6-11 REST bypass; RSS gate unreachable (V6-06) |
| H-02 | code read (bounded readers upload.rs:357-386, caps) | **fixed** | `h02_*` ×5 + poc_expect_fail 413 | field count cap also present |
| H-03 | code read (pipeline wiring + serving headers + risky_mime table) | **fixed** | `h03_*` ×5 | note V6-16 (concurrent sibling writes before abort) |
| H-04 | code read (clamp from chunking.max at state.rs:75-79; CatchPanicLayer router.rs:103; apply checks) | **fixed** | `h04_*` ×3 | compressor tests extended for u64 hints |
| H-05 | code read (MatchedPath request_id.rs:26-32; rpc_of strict metrics-interceptors:41-60; metrics_guard) | **fixed** | `h05_*` ×4 | exposure default loopback ✓ compose ✓ |
| M-02 | code read + call-site grep | **fixed-with-caveat** | worker accounting covered by replication suite | lag half is dead code (V6-04); queue-depth gauge itself correct |
| M-06 | code read of `boot_sweep_tmp` vs artifact locations | **disputed** | `boot_sweeps_stale_tmp` listed as *planned*, absent | sweep never reaches `.tmp-*`/`.gc-pending` (V6-03) |
| M-13 | code read (cfg-gated open_new_key_file encryption.rs:138-160) | **fixed** | CI windows job | compile verified by reading; suite not runnable here |
| M-14 | code read (timeout + counted drop + sync fail replicator.rs:96-133) | **fixed** | replication suite + drop counter | dropped ops still pollute the dead lag counter (V6-04) |
| M-15 | workflow read + scripts read | **fixed-with-caveat** | CI is the gate | caveats V6-05/06 + V6-OPS-03; no `permissions:` block |
| M-17 | (via H-05) | **fixed** | `h05_metrics_*` | — |
| L-10 | (via H-03) | **fixed** | covered by h03 download gate | Vary present on all downloads |
| M-01 | code read (reconcile, in-txn re-walk, lock, admin triggers audited) | **fixed-with-caveat** | `reconcile_*` ×6 | caveats: V6-08 materialization + lock steal + phantom non-repair |
| M-04 | code read (validation.rs, both transports, list_filtered, capped GetManifest) | **fixed** | `parity_validation_matrix` ×6, `viewers_cannot_list_deleted_both_transports`, `list_total_matches_rest`, `getmanifest_truncates_chunks` + 9 units | grep rule holes → V6-13 (doesn't unfix M-04) |
| M-05 | code read (throttle wiring, shared governor, streaming entry checks) | **fixed-with-caveat** | `grpc_stream_flood_throttled`, `login_lockout_exponential`, `verifier_error_does_not_throttle`, `streaming_governor_matches_unary`, ×7 total | caveats → V6-15 |
| M-07 | code read (repair_chunk ordering, quarantine, decode clamp reuse) | **fixed-with-caveat** | `repair_rejects_lying_upstream`, `repair_persists_verified_bytes`, `repair_ciphertext_only_logs_and_metrics`, `boot_sweep_quarantine` + 3 units | caveats → V6-09, V6-12; ciphertext-only documented |
| W1-C-03 | code read (two-phase barrier + block_store claim/restore/delete) | **fixed-with-caveat** | `race_gc_upload_x100` ×5 green claim | caveats: boot-recovery claim false (V6-03), residual V6-07, V6-18 cross-process GC |
| CI-W1 | workflow read | **fixed-with-caveat** | windows job exists | `--skip platform_unix` dead; missing `--locked` (V6-OPS-03) |
| CI-W2 | workflow read + executed static checks | **disputed** | — | job cannot pass (V6-05); Rust-level boot test is sound |
| CI-W3 | workflow read | **fixed** | binary-size job | static cap acceptable (V6-OPS-06); PR-only; permissions note |
| CI-W4 | proptest/fuzz read | **fixed-with-caveat** | 6 proptests green claim | fuzz name-only (V6-OPS-05) |
| CI-W5 | script read | **fixed-with-caveat** | dashboard smoke | existence-only assertions (V6-OPS-04); depends on the broken CI to actually run |

**Meta-acceptance check (charter §9):** every closed finding has a §15 row ✓; every new/regression finding has an `audit-repro-v6/` artifact ✓ (V6-05/06/13 executed, rest trace-grade); missed-by-V3 items carry methodology explanations (V6-07: V3 stopped at the fix design, not the implementation race; V6-08: the path didn't exist at V3; V6-17: V3 audited a committed tree, V6 audits an uncommitted one) ✓; threat model covers every charter asset ✓; deferred table complete ✓; bench re-run **not possible** — documented in §2/§7 as the audit's one unresolved evidence gap ✓; scorecard filled ✓; verdict with condition list ✓.
