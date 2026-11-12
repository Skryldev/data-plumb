# REMEDIATION_PLAN — file-storage production audit (2026-11-11)

Findings reference `AUDIT_REPORT.md` §4 / `FINDINGS.csv`. Effort is
owner-agnostic (S <1d · M 1–3d · L 1w · XL >1w). Every item lists the test
that gates "done" and a verification command. Nothing here was applied —
the audit was strictly read-only.

## Dependency graph (what unblocks what)

```
C-01 (creds) ──────────────┐
C-02 (replica purge) ──────┤
C-03 (GC/upload race) ───┐ ├─► P0 exit gate (all four + H-*) ─► P1 ─► P2 ─► P3
M-13 (win build fix) ────┤ │      (M-13 also unblocks CI matrix + audit/deny runs)
H-01..H-05 ──────────────┴─┘
C-03 ──► M-01 (reconcile pass reuses reachability core)
H-01 ──► M-11/M-14 (streaming enqueue changes queue semantics — fix together)
M-04 ──► M-16 doc decision (shared validation layer)
M-15 (CI) ──► every later regression gate
```

Ordering rule inside P0: **C-01 first** (it is the only finding that is
exploitable by an anonymous outsider on a default deployment today), then
C-03/C-02 (data destruction), then the H-* hardening.

---

## P0 — Blockers (before ANY production traffic)

### P0.1 [C-01] Kill the shipped-credential bootstrap — S
- **Fix:** `config.rs::validate()` rejects: any user with `password` when
  `debug_assertions` are off (accept `password_hash` only), `jwt_secret`
  < 32 chars or matching a deny-list of repo-present literals
  (`change-me-in-production…`), `replication.api_key` < 24 chars or
  deny-listed (`internal-dev-api-key-change-me`,
  `cluster-api-key-change-me`). Dockerfile ENTRYPOINT takes the config
  path from `FS_CONFIG` env with **no** baked default.
  `docker-compose.yml` mounts real secrets (env/file) for jwt secret,
  argon2 hashes and per-node api keys; drop `user: "0:0"`.
- **Test:** unit test — `validate()` fails for each shipped literal;
  integration test — server with `password` field refuses to boot in
  release profile.
- **Verify:** `cargo test config` + attempt `docker compose up` with
  untouched config → container exits non-zero with actionable log.

### P0.2 [C-03] Close the GC/purge-vs-dedup-upload race — M
- **Fix (minimal, self-healing):** in `UploadPipeline::process`, after
  `register_chunks`, for every chunk whose report says *new* while its
  blob was seen as *pre-existing* (or simply: for every chunk), assert
  `blocks.exists(hash)`; if missing, rewrite from the still-retained
  `payloads` entry (until H-01 lands; afterwards re-stream from the
  spooled temp or re-enqueue from the client). Additionally, in
  `gc.rs::purge_soft_deleted`/`hard_purge`, re-read chunk meta after the
  unlink loop and resurrect metadata if a new row appeared
  (belt-and-braces); document the epoch protocol as the durable fix.
- **Test:** `race_gc_upload` — 100 iterations: slow upload (gated
  manually between `store_chunk` and `register_chunks` via a test-only
  hook on the pipeline struct, or by driving the components directly as
  `audit-repro/audit_repro.rs::c03_*` does) racing purge + GC; assert
  zero dangling manifests and all reads 200.
- **Verify:** `cargo test --test integration_test race_gc_upload -- --ignored --nocapture`.

### P0.3 [C-02] Replica purge must be metadata-only — S
- **Fix:** `replication_service.rs::apply_delete_file(purge=true)` drops
  the `decr_refcounts`/`blocks.delete` block; replicas reclaim via
  reachability GC (already implemented). Add a comment that refcounts are
  primary-only. Longer term (P2, M-08): delete one of the two internal
  protocols.
- **Test:** `replication_grpc_purge_shared_chunk` — apply two manifests
  sharing a chunk via `ReplicateBatch`, `DeleteFile(purge)` one, assert
  the other still streams; equivalent to `audit_repro.rs::c02_*` passing
  *after* the fix (it must fail before).
- **Verify:** `cargo test --test grpc_test replication_purge -- --nocapture`.

### P0.4 [H-02 + H-04 + H-05] Cheap input/metric hardening trio — S (combined <1d)
- **H-02:** bounded metadata-field reader in `upload_multipart`
  (`MAX_META_FIELD + 1` bytes via `field.chunk()` loop, abort over); never
  `.text()` unknown fields.
- **H-04:** `decode()` clamps to `min(cr.size_original, chunking.max as u64 + 1)`
  and returns `Corruption` beyond; `tower_http::catch_panic` layer added
  to the REST router (gRPC already has recovery).
- **H-05:** metrics middleware records only `MatchedPath` (fallback label
  `:unmatched`); `rpc_of` parses `/pkg.Service/Method` and falls back to
  `:unparsed`; `/metrics` optional api-key/bound-interface config.
- **Tests:** multipart oversize `name`/`tags`/unknown rejected with ≤64 KiB
  resident (assert via body limit + status 400); `decompress_hostile_bound`
  returns Corruption (convert the audit repro's `should_panic` into an
  `is_err()`); metrics series count constant across 500 junk paths.
- **Verify:** `cargo test multipart hostile metrics_cardinality` + rerun
  `audit-repro/curl-poc.sh` §H-02/§H-05 → all FAIL→PASS flips.

### P0.5 [H-01] Bound upload memory — M
- **Fix:** stream `ChunkPayload`s into the replication queue as chunks
  complete (inside the `buffer_unordered` stage, after blob write);
  `StoredChunk` loses its `blob` field; `stored` keeps only refs; enqueue
  failures abort the upload *before* `put_manifest` (correct: unreplicated
  file must not commit in sync mode; in async mode fail-open with an
  alert — decide and document, see M-14). Add global admission:
  `Semaphore(max_concurrent_uploads)` + reject when
  `expected_size/Content-Length` × in-flight > RAM budget.
- **Test:** two 2 GiB uploads on a 2 GiB-capped test config complete with
  RSS < 1.5 GiB (soak-lite with `procfs`/`GetProcessMemoryInfo` probe);
  oversize `Content-Length` → 413 pre-stream.
- **Verify:** `cargo test --test integration_test upload_memory_bound -- --ignored`.

### P0.6 [H-03] Wire (or remove) validation + safe serving — S
- **Fix:** call `validator.validate_head(declared_mime, first_bytes)` with
  the first chunk in `store_chunk` (or pre-chunk in the adapter); serve
  downloads with `X-Content-Type-Options: nosniff` and
  `Content-Disposition: attachment` for `text/*`, `application/xhtml`,
  `image/svg+xml` unless the client asked otherwise.
- **Test:** mismatch upload → 400 (validator's own unit tests already
  cover the logic); stored-PNG-as-html download carries nosniff +
  attachment.
- **Verify:** `cargo test validator upload_mime download_headers`.

### P0.7 Checklist closes
- Re-run on Linux: `cargo audit`, `cargo deny check` (§9 unresolved),
  `cargo test`, `cargo bench --bench throughput` (README table re-baseline, I-07).
- Execute `audit-repro/` end-to-end; file issue for any repro that now
  *passes* (fixed) and delete it from the catalog with a note.

**P0 definition of done:** C-01/C-02/C-03 + H-01..H-05 flipped ✅ in §11;
all new tests green in CI (M-15 minimal pipeline may land here in S);
`audit-repro/curl-poc.sh` reports 0 PASS-of-a-bug; `cargo audit` clean or
ticketed.

---

## P1 — First 30 days (remaining HIGH n/a; HIGH-severity leftovers + selected MEDIUM)

| Item | Fix sketch | Test / gate | Effort |
|---|---|---|---|
| M-13 win build | cfg-gate unix perms block | `cargo check --target x86_64-pc-windows-msvc` in CI matrix | S |
| M-15 CI | GH Actions: fmt+clippy+test(linux,win)+audit+deny+buf | green run on main | S |
| M-01 leak reconcile | periodic pass: chunks-table hashes vs manifests reachability → zero & sweep like orphans; report metric | crash-sim test (register w/o manifest → reconcile reclaims) | S |
| M-04 parity | shared validation module both transports; admin-gate `include_deleted`; truncate gRPC GetManifest chunks (1000 + `chunks_truncated`); correct gRPC list `total` | cross-protocol parity test (same inputs → same validation outcomes) | M |
| M-05 rate limits | tenant-keyed limiter call at entry of streaming RPCs; per-username login backoff (exponential lockout w/ audit event) | streaming flood test → RESOURCE_EXHAUSTED; login lockout test | M |
| M-06 tmp sweeps | boot sweep of `.tmp-*` in blob dirs + `temp_dir` older than `orphan_min_age` | kill -9 loop ×50 → disk delta = 0 after restart | S |
| M-07 repair poisoning | decrypt+verify `plain_hash` BEFORE persisting pulled blob; on mismatch evict L2 + quarantine file + `hash_mismatch{source="repair"}` | unit test with lying upstream server | M |
| M-09 internal body limit | `RouteLayer` body-limit override (2 GiB or `max_file_size`+) for `/internal` | 1.5 GiB bundle resync E2E | S |
| M-10 audit attribution | pass `AuthContext` + peer IP into pipeline/download audit | audit row asserts username+ip | S |
| M-12 supervision | `tokio::spawn` → supervised loop (restart + metric `watcher_restarts`); gRPC probe calls `verify_writable` | panic-injected watcher (test hook) recovers ≤2 intervals | S |
| M-14 enqueue timeout | `timeout(cfg, send())` → async mode: drop op + `replication_dropped_ops_total`; sync mode: fail upload | replica-down soak: uploads keep succeeding (async) with metric ticking | S |
| M-16 oracle decision | threat-model page: accept/document plaintext oracle **or** tenant-prefixed keys behind config (default off) | doc PR + (if coded) dedup test asserting no cross-tenant hits | S/M |
| M-17 metrics gating | `metrics_bind: 127.0.0.1` default or api-key | compose scrape still works | S |
| L-02 L-09 metrics truth | stream-drop guards record duration/status; read grpc-status from trailers | unit tests on the guards | S |

**P1 definition of done:** §3.3/3.5/3.6/3.10 partials → ✅; zero `Suspected`
findings remain unresolved; CI green 2 weeks; `race_gc_upload` and crash
suite in the default `cargo test` run.

---

## P2 — Days 30–90 (perf, protocol unification, ops depth)

1. **M-03 scans** (L): summary records in `name_index` values (name/mime/
   sizes/tags) → O(page) lists; cursor pagination; incremental GC
   checkpoint; `global_stats` cached + recomputed on mutation.
   *Gate:* 1M-file fixture — list p99 < 100 ms; GC tick RSS delta < 50 MB.
2. **M-08 internal protocol unification** (M): one apply-core; per-peer
   api keys; HMAC over batch frames; purge semantics identical across
   HTTP/gRPC (delete C-02's twin outright). *Gate:* property test:
   applying any op-sequence via either transport yields identical state.
3. **M-11 shipper resilience** (M): byte-bounded queue; circuit breaker
   (open after N failures, half-open probe); dead-letter file + replay
   tool; shutdown flush errors logged + counted. *Gate:* chaos soak
   (random peer restarts, 1 h) — zero silent losses (dead-letter matches
   dropped counter).
4. **fsync group-commit** (S/M): batch `sync_all(dir)` per pipeline
   window. *Gate:* upload bench ≥1.5× on NVMe, no durability regression
   in crash suite.
5. **L-01/L-04/L-06/L-07** (S each): usage accounting at replace;
   tenant-existence gate at upload; tag caps; digest-pinned bases.
6. **I-07 perf re-baseline** (S): re-run benches on the production-shaped
   box; update README table with environment.
7. **H-01 follow-up** (M): disk-backed replication WAL spool (ops
   replayable after crash — closes the async RPO window documented in
   README). *Gate:* kill-primary soak → replica converges to zero
   documented loss with WAL enabled.

**P2 exit:** scorecard Performance ≥8, Reliability ≥7, Ops ≥7; §3.7/3.13
partials ✅.

---

## P3 — Backlog (hardening, refactors, future-proofing)

- I-01 RS256/EdDSA + dual-secret rotation window for zero-downtime secret rolls.
- I-02 rebuild-from-blobs tool (`index rebuild` subcommand: walk blobs →
  re-derive ChunkMeta → manifests from bundle backups or client hints) +
  key-rotation/re-encryption runbook & tooling.
- I-03 audit hash-chaining (`prev_hash` per row) + export tool.
- I-04 fuzz (`decode_batch`, `decode_file_bundle`, multipart, range
  parser), proptest (chunker offsets, parse_range), 24 h SOAK with RSS
  tracking, fault-injection seams (`FsProvider`, `Clock`).
- L-05/L-08/L-10/I-06 doc/dead-code/nits; `pub(crate)` discipline pass.
- L-03 test deletion; duplicate `constant_time_eq` consolidation.
- Optional TLS feature for the replication client (I-05) if cluster
  networks stop being trusted.

**P3 exit:** scorecard Code Quality ≥8, Testing ≥8; audit re-run verdict
target: **CONDITIONAL-GO**.
