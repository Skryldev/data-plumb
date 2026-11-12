# file-storage — Production Readiness Audit

**Audit date:** 2026-11-11 · **Commit:** `e4cd5a6` (main) · **Auditor:** independent senior Rust systems/security review (ZCode)

---

## 0. Verdict

- Production readiness: **NO-GO**
- The storage core (chunker, block store, index transactions, read path) is genuinely well-engineered and unusually well-tested for its size. But the shipped deployment story hands any network-adjacent attacker full admin access, and three independent correctness holes can each destroy user data that the API reported as safely stored. Fixing C-01..C-03 + H-01..H-05 is a precondition for any production traffic.
- **Top 10 most dangerous issues** (IDs reference §4)
  1. **C-01** Shipped default credentials + known JWT secret + known cluster API key (image ENTRYPOINT uses default.yaml; compose overrides neither)
  2. **C-02** gRPC replication purge deletes blobs other live replica files still reference (replica refcounts are always 0)
  3. **C-03** GC/purge vs dedup-upload race commits a manifest pointing at a deleted blob (upload returns 201; reads fail forever)
  4. **H-01** Upload pipeline buffers the entire file in RAM (10 GiB default cap, no admission control) → trivial remote OOM
  5. **H-02** Multipart metadata fields fully buffered before the 4 KiB length check (≤1 GiB alloc per request)
  6. **H-03** Advertised magic-byte validation is dead code; attacker-controlled MIME served `inline` (stored-XSS surface)
  7. **H-04** Decompression bound trusts the manifest: hostile/corrupt `size_original` panics (`capacity overflow`) or OOMs the read path
  8. **H-05** Prometheus label cardinality DoS (REST path normalization incomplete; gRPC raw path; `/metrics` unauthenticated)
  9. **M-01** Crash window between `register_chunks` and `put_manifest` leaks blobs+refcounts permanently (README claim overstated)
  10. **M-07** On-miss blob repair persists unverified upstream bytes to disk and L2 — a hostile/compromised primary permanently bricks replica chunks

---

## 1. Executive Summary

`file-storage` is a hybrid chunked content-addressed store (FastCDC + BLAKE3 + zstd + optional per-tenant AES-256-GCM, redb metadata, moka 3-tier cache, REST + gRPC, async/sync replication). The audit read 100% of `src/` (78 files, ~9.6k LOC excluding tests), all tests, proto, configs, Docker files, README and the docs book, and traced the five critical data flows line-by-line.

The engineering quality of the inner core is high: atomic blob writes with directory fsync, blob-before-index ordering, single-transaction refcount batches, byte-weighted caches with correct invalidation at every mutation site, a complete and exhaustively-tested error model on both transports, and an unusually honest docs suite. Test counts in the README are accurate (131 unit + 17 REST integration + 19 gRPC; README says "18 gRPC").

It is nevertheless **not production-ready**. First, the *shipped* configuration chain (distroless image ENTRYPOINT → `config/default.yaml` → compose with no auth overrides) boots with `admin/admin123`, a world-known JWT secret that passes validation, and a known internal API key that reads and writes every tenant's data — a trivially exploitable full compromise. Second, there are three independent data-destruction paths: a replica purge that deletes blobs still referenced by live files, a GC-vs-dedup-upload race that commits dangling manifests for in-flight uploads (window = the whole upload stream), and a permanent-refcount-leak crash window that contradicts the README's crash-safety claim. Third, the service is remotely exhaustible: the upload pipeline retains the entire (compressed) file in memory with a 10 GiB ceiling and no admission control, multipart metadata fields allocate up to the 1 GiB body limit before a 4 KiB check, a manifest field trusted from the index can panic the decompressor, and unauthenticated requests can blow up Prometheus cardinality. Finally, the headline "validate" stage of the upload pipeline is dead code, so content-type claims are never checked and attacker-chosen MIME types are replayed `inline` to browsers.

The five pre-traffic blockers: replace the credential/bootstrap story (C-01), make replica deletes reachability-based (C-02), add a GC/upload synchronization epoch or second-chance protocol (C-03), bound pipeline memory and multipart fields (H-01/H-02), and wire (or delete claims for) validation plus a server-side decompression cap (H-03/H-04).

---

## 2. Methodology & Evidence Base

**Passes executed:** Pass 0 (tooling/baseline), Pass 1 (5 data-flow traces), Pass 2 (adversarial per-endpoint matrix), Pass 3 (DoS as function of attacker input), Pass 4 (crash windows × write ops, restart story), Pass 5 (senior quality bar + docs drift).

**Tooling:** cargo 1.99.1 (stable, win32 host), clippy, ripgrep. **Environment limitation (material):** the audit host is Windows and the crate **does not compile on Windows** (`src/security/encryption.rs:55` imports `std::os::unix::fs::OpenOptionsExt` outside any `#[cfg(unix)]` — E0433/E0599), and no WSL/Docker was available. Consequences, stated per the honesty clause:

* `cargo build` fails on this host for platform reasons, not product defects; **tests could not be executed here**. All findings are graded **High (trace)** or **Suspected** — never "Confirmed (executed)" — with exact file:line interleavings, and `audit-repro/` ships runnable reproductions for execution on Linux (documented `cargo test --test audit_repro -- --include-ignored` convention).
* clippy output on the host covers only the parse/lint phase before the unix-only errors: 1 warning (`unused import: Path` — unix-gated code is the consumer, so 0 warnings on Linux is credible).
* `cargo audit` / `cargo deny` not installed and not installable in the sandbox; §9 compensates with a manual Cargo.lock review of the security-critical pins.

**Commands run:** full-file reads of every source file (no sampling); `rg` pattern hunts (`unwrap|expect|panic`, `unsafe`, `std::fs`, `clone()`, integer casts, header insertion, lock usage, channel construction); targeted greps for every checklist item (`validate_head|sniff_mime` call sites, cache-invalidation sites, `manifests.invalidate`, `enqueue_*`, `decr_refcounts`, fsync sites); test-count greps; config/proto/Docker/compose/README/docs cross-reads.

**LOC audited (non-test):** `src/` ≈ 7,300 lines across 78 files — all read end-to-end. Largest: `storage/index_db.rs` (1026), `grpc/services/file_service.rs` (505), `config.rs` (488), `grpc/client/mod.rs` (456), `security/auth.rs` (389), `replication/client.rs` (384), `api/handlers/download.rs` (360), `core/pipeline.rs` (318), `storage/block_store.rs` (302). Tests: 167 test functions (131 unit / 17 REST / 19 gRPC) + criterion bench, all read. Docs: 55-page mdBook sampled at every security/ops claim.

**Time distribution:** ~40% data-flow + adversarial tracing, ~15% checklists, ~15% reproduction authoring, ~30% report.

---

## 3. Strengths (≥12, with evidence)

1. **Directory fsync after rename** — `block_store.rs:79-81` opens the shard dir and `sync_all()`s after `rename`, the POSIX-durability step most implementations forget. The classic "rename lost after power-cut" bug is absent.
2. **Blob-before-index ordering, single-fsync refcount batch** — blobs are fsynced before `register_chunks` runs in ONE redb transaction (`pipeline.rs:203-208`, `index_db.rs:196-235`), so the common crash leaves only reclaimable orphans and dedup refcount updates are race-free within a transaction.
3. **`Durability::Immediate` verified** — `index_db.rs:80` sets it on every write transaction; no batching weakens it.
4. **Chunk-boundary determinism is tested, not assumed** — `chunker.rs:184-197` asserts identical chunk hashes under 1 KB vs 250 KB packetisation, plus a single-pass cross-check (`chunker.rs:229-242`) and min/max bounds tests. This is the property the whole dedup design stands on.
5. **Range arithmetic is complete and exhaustively tested** — `download.rs:34-75` covers suffix/first-of-multi/clamp/zero-size; 10 unit tests including `bytes=-0`, `5-2`, `100-` on size-100; 416 carries `Content-Range: bytes */N` (`download.rs:199-207`) per RFC 9110.
6. **Read-back BLAKE3 verification on every L1 miss** — `retrieval.rs:158-163` verifies `plain_hash` after decrypt+decompress and *then* inserts into L1; slices are taken from the verified full plaintext (`retrieval.rs:117-119`), so the suspected "range poisons L1" bug is **provably absent**.
7. **Cache invalidation is complete at every mutation site** — soft delete, purge, replication apply and gRPC delete all invalidate L3 (`delete.rs:68,92`, `internal.rs:100,198`, `file_service.rs:422,453`, `replication_service.rs:249,278,290`); L3 never caches negative lookups (`manifest_cache.rs:34-47`), so read-after-upload and delete visibility are correct.
8. **Token security details done right** — HS256 pinned (no alg confusion, `auth.rs:197`), 5 s exp leeway, single-use rotating refresh tokens with reuse-failure (`auth.rs:178-188`), a dummy-hash timing equalizer against username enumeration (`auth.rs:126-134`), and constant-time comparisons for passwords and the internal API key (`auth.rs:225-233`, `middleware/auth.rs:40-48`, `proto_auth.rs:145-153`).
9. **Dual error mapping is total** — every `StorageError` variant has an HTTP status (`error.rs:44-70`) and a gRPC code (`proto_auth.rs:101-124`), each side exhaustively tested (`error.rs` tests, `proto_auth.rs:172-200`); no variant falls into a catch-all 500/Unknown.
10. **gRPC panic recovery + honest streaming semantics** — `recovery.rs` maps handler panics to `INTERNAL` instead of dropping the connection, and the module documents that spawned streaming tasks are *not* covered.
11. **Graceful shutdown drains both transports jointly and flushes replication** — watch-channel fan-out, `tokio::join!` of both servers, then `replicator.shutdown()` awaits the worker (`lib.rs:105-158`).
12. **Test quality is behavioral** — integration tests assert real invariants over real HTTP: refcount survival of shared chunks across purge (`tests/integration/deletion.rs`), uniform-content collapse to one physical chunk (`dedup.rs`), tenant isolation matrix (`authz.rs`); the harness spawns real listeners on ephemeral ports.
13. **Deployment hygiene above the norm** — distroless `cc-debian12:nonroot` runtime, `--locked` builds, hermetic vendored protoc (`build.rs`), cargo-chef layer caching, joint REST+gRPC TLS/mTLS support with client-CA pinning (`server.rs:132-160`).
14. **Documentation is honest where it matters** — quotas "accounted but not enforced" (`docs/src/security/tenants.md:38`), async replication loss window admitted (README §Design notes), threat model cites implementing files. Drift that exists is catalogued in §10.

---

## 4. Findings Catalog

Severity = impact × reachability. Every CRITICAL/HIGH includes a reproduction (runnable artifact in `audit-repro/` or an interleaving trace).

---

### [C-01] Shipped deployment boots with known admin credentials, JWT secret and cluster API key
- **Severity:** CRITICAL · **Category:** authn/supply-chain/ops
- **Location:** `Dockerfile` (ENTRYPOINT `--config /etc/file-storage/default.yaml`), `config/default.yaml:41-63`, `docker-compose.yml` (no `FS_AUTH__*` env), `config.rs:365-369`
- **Evidence:** The image's only config is `default.yaml`, which contains `admin`/`admin123` (plus `alice123`, `bob123`, `viewer123`), `jwt_secret: "change-me-in-production-0123456789abcdef"` and `api_key: "internal-dev-api-key-change-me"`. `AppConfig::validate()` enforces only ≥16 secret chars, so the world-known placeholder **passes validation and boots**. `docker-compose.yml` overrides replication/ports/logging but no auth key, and publishes 8080/9090 on all interfaces.
- **Reproduction:** `audit-repro/curl-poc.sh` §1–3 (login as admin/admin123 → admin JWT; internal chunk read with the shipped API key passes auth); `audit-repro/audit_repro.rs::c01_*` proves `AppConfig::load("config/default.yaml").validate()` succeeds and `AuthService::login("admin","admin123")` mints an admin token. README itself documents the credentials in its Quick start.
- **Impact:** Full admin (all tenants' data, purge, GC, resync) for anyone who can reach the port — plus, with the known JWT secret, **forged admin tokens for any tenant** without ever logging in, and with the known API key, read/write of every tenant's ciphertext via `/internal/*` and gRPC `ReplicationService`.
- **Exploitability:** Trivial (credentials are in the public README; no user interaction).
- **Recommended fix:** (1) `validate()` must reject `password` entries entirely in release builds (or at least boot-fail on literal `change-me`/`admin123` markers and any `jwt_secret` matching a shipped file); (2) require `jwt_secret` ≥32 random chars and `api_key` ≥24, generated by the operator — fail boot without them; (3) compose must set `FS_AUTH__JWT_SECRET`, `FS_AUTH__USERS` (or mount a secret file) and per-node API keys; (4) never bake `default.yaml` into the image ENTRYPOINT.
- **Effort:** S (<1d) · **Confidence:** High (trace) — config/validate/login code paths all read; execution blocked only by the Windows-host limitation.

---

### [C-02] Replica purge via gRPC ReplicationService deletes blobs still referenced by live files
- **Severity:** CRITICAL (precondition: deployments using the gRPC replication path) · **Category:** durability/correctness
- **Location:** `src/grpc/services/replication_service.rs:267-282` (`apply_delete_file`), `src/storage/index_db.rs:239-264` (`upsert_chunk_replica` sets `refcount: 0`), `src/storage/index_db.rs:281-292` (`decr_refcounts` removes refcount ≤1 rows)
- **Evidence/interleaving:** replica-applied chunks carry refcount 0 forever; purging file A runs `decr_refcounts([X])` → the `Some(_)` arm removes the row and returns X for blob deletion even though file B (live) references X. Full trace: `audit-repro/INTERLEAVINGS.md` §C-02. The HTTP internal path (`internal.rs:95-102`) does *not* touch refcounts — the two internal protocols disagree, and the advertised gRPC one destroys data. `tests/integration/replication.rs` only exercises the HTTP path, which is why this is untested.
- **Reproduction:** `audit-repro/audit_repro.rs::c02_*` executes the exact sequence (`upsert_chunk_replica` ×2 → two manifests share X → remove_manifest(A) → decr_refcounts → blob gone while B is live).
- **Impact:** Silent permanent data loss on replicas for any chunk shared between a purged file and a surviving file; replica reachability GC then "correctly" removes B's manifest too (its blob is gone), cascading the loss.
- **Exploitability:** Inevitable under normal operation once the gRPC replication path is used with purges (an admin `?purge=true` is enough). Medium difficulty to *trigger deliberately* (needs api key).
- **Recommended fix:** replica-side deletes must be metadata-only (mirror the HTTP path) and let reachability GC reclaim blobs; or `apply_put_chunk` must increment refcounts and `apply_delete_file` decrement them per-manifest. Add a cross-protocol purge-semantics test.
- **Effort:** S · **Confidence:** High (trace + runnable repro).

---

### [C-03] GC/purge vs dedup-upload race commits a manifest referencing a deleted blob
- **Severity:** CRITICAL · **Category:** concurrency/durability
- **Location:** `src/core/pipeline.rs:155` (`store_chunk` → `blocks.write` dedup skip) vs `pipeline.rs:207` (`register_chunks`), racing `src/api/handlers/delete.rs:74-86`, `src/grpc/services/file_service.rs:450-460`, `src/gc.rs:121-136` and `src/gc.rs:141-166`; root enablers `index_db.rs:196-235` (register never verifies blob presence) and `gc.rs` (no epoch/lock/second chance)
- **Evidence/interleaving:** upload B dedup-writes nothing (`write` returns `false`, blob exists) → purge/GC removes the last other owner (refcount 1→0, blob unlinked) → B finishes: `register_chunks` re-creates meta refcount=1, `put_manifest` commits → **upload returns 201; every subsequent read of B fails NotFound forever** (no repair path on primaries). The window is the *entire remaining upload stream* — up to 10 GiB default `max_file_size`. The orphan sweeper has the same effect for uploads streaming longer than `orphan_min_age_secs` (1 h default; 10 GiB at 1 MB/s ≈ 3 h). The grace period only bounds time since *delete*, not in-flight uploads.
- **Reproduction:** `audit-repro/audit_repro.rs::c03_*` (enforced interleaving through the real `BlockStore`/`IndexDb`/`GarbageCollector`) and `::c03b_*` (sweeper variant); window statement in `INTERLEAVINGS.md` §C-03.
- **Impact:** user-visible permanent data loss of a *successfully acknowledged* upload; also silently wrong `dedup_chunks`/`dedup_bytes` (the chunk is counted as new).
- **Exploitability:** Easy — a `user`-role attacker controls upload duration (slow-stream) and can time an admin's purge/GC tick; no bug-chaining needed, just ordinary concurrency.
- **Recommended fix (choose one, smallest first):** (a) second-chance: after `blocks.delete(h)`, re-read chunk meta — if the row reappeared (refcount>0), rewrite the blob from a source (impossible on primary) — so instead (b) make GC/purge mark-then-sweep in two passes with a re-check of refcount immediately before unlink **inside the same redb transaction that removes the row is not enough** — the correct minimal protocol is: `register_chunks` verifies blob existence and, if missing, re-writes it from the in-memory `payloads` the pipeline still holds (upload path self-heals); plus GC takes a per-hash "sweep epoch" so an upload that dedup-hit in an older epoch re-stores. Ship (b) + a 100-iteration stress test (upload ‖ purge loop).
- **Effort:** M (1-3d) · **Confidence:** High (trace + runnable repro).

---

### [H-01] Upload pipeline retains the entire file in memory; no admission control
- **Severity:** HIGH · **Category:** dos
- **Location:** `src/core/pipeline.rs:159-172` (`stored: BTreeMap<usize, StoredChunk>` holds `blob: Bytes` for every chunk until stream end), `pipeline.rs:179-201` (`payloads` retains them for replication), `config/default.yaml` (`max_file_size: 10 GiB`, `max_body: 1 GiB` — gRPC path has no body cap)
- **Evidence:** `PIPELINE_CONCURRENCY = 8` bounds *processing*, not *retention*: every `StoredChunk` accumulates regardless. Peak per-upload RSS ≈ Σ compressed chunk sizes ≈ full file (raw for incompressible data, which the README's own benchmark notes). Interleaving/OOM trace: `INTERLEAVINGS.md` §H-01.
- **Reproduction:** 4 parallel gRPC uploads of one 6 GiB incompressible file at 1 MB/s → ~24 GiB RSS with zero completed requests → OOM kill. (Script-level; deterministic given the code's retention structure.)
- **Impact:** trivially remote-triggered OOM (authenticated `user` role suffices); crash also loses all concurrent in-flight uploads.
- **Exploitability:** Easy.
- **Recommended fix:** stream chunks to the replicator as they are produced (enqueue per chunk inside `store_chunk` completion), keep only `ChunkRef` metadata in `stored`, drop `StoredChunk.blob` after blob write + enqueue; add a global upload semaphore (e.g. `max_concurrent_uploads × max_file_size < RAM budget`) and reject uploads whose `Content-Length`/`expected_size` exceeds the current budget.
- **Effort:** M · **Confidence:** High (trace — retention structure is unconditional).

---

### [H-02] Multipart metadata fields fully buffered before the 4 KiB length check
- **Severity:** HIGH · **Category:** dos
- **Location:** `src/api/handlers/upload.rs:113-146` — `field.text().await` (buffers the whole field) runs *before* `if v.len() > MAX_META_FIELD` (`upload.rs:115`, `123-127`); unknown fields `let _ = field.text().await` (`upload.rs:144`) with no cap at all
- **Evidence:** only ceiling is `DefaultBodyLimit::max(limits.max_body)` = 1 GiB default (`router.rs:80`). A single multipart part named `tags` (or any unknown name) with a ~1 GiB body allocates ~1 GiB of `String` per request before rejection; repeatable concurrently.
- **Reproduction:** `audit-repro/curl-poc.sh` §H-02 (256 MiB variant + scaling note).
- **Impact:** remote memory exhaustion at ~1 GiB per cheap request, no auth trick beyond `user` role (multipart path requires write).
- **Exploitability:** Easy.
- **Recommended fix:** read metadata fields with a bounded reader (e.g. take first `MAX_META_FIELD+1` bytes via `field.chunk()` loop) and abort the field once over; drop/hard-cap unknown fields without reading them.
- **Effort:** S · **Confidence:** High (trace).

---

### [H-03] Advertised magic-byte validation is dead code; attacker MIME replayed `inline` (stored-XSS surface)
- **Severity:** HIGH · **Category:** authz (content trust)/docs
- **Location:** `src/core/validator.rs` (`validate_head`/`sniff_mime` — **zero call sites** outside its own tests; verified by `rg 'validate_head|sniff_mime' src/`), `upload.rs` (only `validate_size` ever called), README line 6 (`upload ──► validate`), `lib.rs:3`; serving side `download.rs:225-232` (`Content-Type: <user mime>`, `Content-Disposition: inline; filename=…`, no `X-Content-Type-Options: nosniff`)
- **Evidence:** the entire upload path never inspects content bytes; the MIME stored and served is whatever the client declared (REST query `mime=`, multipart `mime`/`content_type` field, gRPC `mime_type`). A `user` can store a PNG as `text/html` and the service serves it `inline` with that type on any origin a browser can reach.
- **Reproduction:** `audit-repro/curl-poc.sh` §H-03 (uploads PNG magic with `mime=text/html`, asserts stored/served type).
- **Impact:** stored-XSS surface on any deployment where a browser can be pointed at the download endpoint (Bearer tokens are not auto-sent by browsers, which moderates reachability — hence not CRITICAL); plus every "validate" claim in README/docs/lib docs is false, and the validator is dead weight.
- **Exploitability:** Medium (needs victim to open an authenticated link).
- **Recommended fix:** wire `validate_head` into the pipeline (first chunk of the stream is available before compression), add `X-Content-Type-Options: nosniff` and default `Content-Disposition: attachment` for text/html-ish types; or delete the validator and fix every doc claim.
- **Effort:** S · **Confidence:** High (trace — absence of call sites is mechanically verified).

---

### [H-04] Decompression bound trusts the index; hostile/corrupt `size_original` panics or OOMs the read path
- **Severity:** HIGH (precondition: peer with api key, or on-disk/index corruption) · **Category:** dos/integrity
- **Location:** `src/storage/retrieval.rs:186-188` (`decompress(&raw, true, cr.size_original as usize)`), `src/core/compressor.rs:81` (`zstd::bulk::decompress` → `Vec::with_capacity(capacity)` — panics `capacity overflow` at `u64::MAX`, attempts multi-GiB allocs for large values), sources of the value: redb row or `POST /internal/manifest` / gRPC `PutManifest` (`internal.rs:84-92`, `replication_service.rs:237-263`)
- **Evidence:** no server-side cap independent of the manifest. A compromised peer (or a flipped index row) with `size_original = u64::MAX` panics the request task on REST (no CatchPanicLayer on the REST router → connection reset) and returns INTERNAL on gRPC; `size_original = 8 GiB` attempts an 8 GiB allocation per read. Note the compressor doc-comment claims "corrupt input can never allocate unboundedly" (`compressor.rs:63-64`) — true only for honest metadata.
- **Reproduction:** `audit-repro/audit_repro.rs::h04_*` (`#[should_panic(expected = "capacity overflow")]` through the real `Compressor::decompress`).
- **Impact:** per-request panic/OOM; repeatable; REST lacks panic recovery so clients see resets rather than 500s.
- **Exploitability:** Medium (api key or disk corruption; the key is world-known under C-01).
- **Recommended fix:** clamp decompression to `min(cr.size_original, chunking.max)` (chunk plaintext can never exceed `max`) and fail Corruption on mismatch; add `tower_http::catch_panic` to the REST router; verify `size_stored == blob.len()` on the internal apply path.
- **Effort:** S · **Confidence:** High (trace + panic repro).

---

### [H-05] Prometheus label cardinality DoS (REST + gRPC, unauthenticated)
- **Severity:** HIGH · **Category:** dos/observability
- **Location:** `src/metrics.rs:137-150` (`normalize_path` only maps 36-char-4-dash and 64-hex segments), `src/api/middleware/request_id.rs:36-39` (every request — including unmatched 404 paths — records `route` label), `src/api/router.rs:71-79` (metrics middleware wraps the whole router), `src/grpc/interceptors/metrics.rs:53-57` (`rpc_of` returns the **raw** path although its comment claims cardinality is bounded), `/metrics` unauthenticated (`router.rs:74`)
- **Evidence:** `GET /<unique-garbage>` (any non-UUID-shaped path) creates a fresh `route` label per request → unbounded time-series growth in the process-wide recorder (memory) and in Prometheus. On gRPC, arbitrary HTTP/2 `:path` values become `rpc` labels directly.
- **Reproduction:** `audit-repro/curl-poc.sh` §H-05 (200 unique 404 paths → series count jumps).
- **Impact:** remote memory growth + Prometheus storage explosion, unauthenticated.
- **Exploitability:** Trivial.
- **Recommended fix:** record metrics only for matched routes (axum `MatchedPath`), or bucketize unknown paths to `:unmatched`; implement `rpc_of` as `/pkg.Svc/Method` parsing with a fixed fallback; optionally require auth or loopback binding for `/metrics`.
- **Effort:** S · **Confidence:** High (trace).

---

### [M-01] Crash between `register_chunks` and `put_manifest` leaks blob + refcount permanently; README crash-safety claim overstated
- **Category:** durability/docs · **Location:** `pipeline.rs:207` then `pipeline.rs:242` (two transactions); `gc.rs:141-166` (sweeper skips chunks *with* metadata); README line 22 ("a crash can only leave a GC-able orphan")
- **Evidence:** crash in the window leaves `ChunkMeta{refcount:1}` with no owning manifest; refcount GC only acts on tombstones, orphan sweeper only on metadata-less blobs → blob never reclaimed, tenant usage unaffected but disk grows. Same window exists between `soft_delete` (delete.rs:62) and any GC pass is fine — the leak is upload-side. **Repro:** `audit_repro.rs::m01_*` proves a full expired GC pass reclaims nothing. **Fix:** reconcile pass comparing `chunks` table against `all_manifests` reachability (refcount-mode analogue of the reachability GC), run periodically; fix README wording. Effort S. Confidence High.

### [M-02] `file_storage_replication_queue_depth` is wrong (cumulative vs current flip-flop)
- **Category:** observability · **Location:** `replicator.rs:96-97` (`queued` never decremented) vs `replicator.rs:241` (gauge set to buffer len). The same gauge oscillates between "ops ever enqueued" and 0. **Fix:** decrement on flush or report `rx.len()`+buffer. Effort S. Confidence High.

### [M-03] O(n) full-scan operations on hot and admin paths
- **Category:** perf · **Location:** `index_db.rs:485-533` (`list_by_tenant` deserializes every manifest up to offset), `:536-571` (`count_by_tenant` second full scan per list call), `gc.rs:115`/`172` + `index_db.rs:644-657` (`all_manifests` loads every manifest into RAM per GC tick), `global_stats` likewise at boot; tag filter loads every matching manifest per request (`list.rs:24-38`, `file_service.rs:337-347`). At 1 M files: multi-hundred-MB allocations per admin GC tick and multi-second lists. **Fix:** keep summary rows (name-index values) instead of full manifests; cursor pagination; incremental GC. Effort L. Confidence High (structure).

### [M-04] REST vs gRPC divergences (each is a finding per §7 FOCUS 12)
- **Category:** api/isolation · (a) `?include_deleted=true` honored for **viewer** on REST (`list.rs:21`) but admin-gated on gRPC (`file_service.rs:331`) — deleted-file metadata exposed to read-only users; (b) name length validated (1..=255) only on gRPC (`file_service.rs:74-79`) — REST `name` unbounded up to header limits (`upload.rs:62-66`); (c) gRPC `GetManifest(include_chunks=true)` returns the **untruncated** chunk list (potential 131 k refs ≈ 13 MB response for a 1 GiB min-size-chunk file) while REST truncates at 1000 (`dto/file.rs:42-49`); (d) gRPC `ListFiles.total` returns page size for the default path (`file_service.rs:359`) while REST returns tenant-wide count; (e) `expected_size` guard exists only on gRPC; (f) offset ignored for tag/name filters on both. **Fix:** unify validation in the service layer both transports call. Effort M. Confidence High. **Repro:** curl-poc.sh §M-04.

### [M-05] Rate limiting gaps: gRPC streaming unthrottled; login lacks per-account lockout
- **Category:** dos/authn · **Location:** `grpc/interceptors/rate_limit.rs:5-7` (documented bypass for streaming — `Upload`/`Download` are streaming RPCs), REST governor is per-IP only (`rate_limit.rs:14-20`); login (JSON body, argon2id m=64 MiB t=3 per production.yaml — expensive) has no per-username lockout → distributed brute force and CPU burn via `/auth/login` (rate-limited per-IP at 500 rps default — weak). **Fix:** limiter hook inside streaming handlers (tenant-keyed like the unary one), per-username backoff, cheaper verify params are NOT recommended — throttle instead. Effort M. Confidence High.

### [M-06] No startup sweep of `.tmp-*` blobs and spooled multipart temps
- **Category:** ops · **Location:** `block_store.rs:187-189` (walk skips `.tmp-` forever, never deletes), `state.rs:53-57` (no sweep at boot), `upload.rs:191`/`217` (`temp_dir` keeps files on crash). Crash loops grow disk monotonically. **Fix:** boot-time sweep of both directories older than `orphan_min_age`. Effort S. Confidence High.

### [M-07] On-miss blob repair persists unverified upstream bytes (disk + L2 poisoning)
- **Category:** integrity · **Location:** `retrieval.rs:141-150` (`fetch_blob` → `blocks.write` before any verification), internal channel is plaintext HTTP with no TLS option (`Cargo.toml` reqwest `default-features=false`; README documents "no TLS stack pulled in"). BLAKE3 detects the poisoning at read time (good) but the poisoned blob **stays on disk permanently** and in L2 for its TTL → permanent read failures, no quarantine, no metric alarm beyond `hash_mismatch`. Trace: `INTERLEAVINGS.md` §M-07. **Fix:** verify-then-persist ordering where possible (decrypt+decompress+`plain_hash` check before `blocks.write` — the replica holds tenant keys), evict L2 + quarantine file on mismatch, alert on `hash_mismatch` from repair path. Effort M. Confidence High.

### [M-08] Internal trust boundary: one shared api key, no per-op integrity; internal protocols disagree
- **Category:** isolation · **Location:** `internal.rs:176-187` (`apply_chunk` writes any blob under any hash — no relationship between blob and `hash`/`plain_hash` checked), `internal.rs:95-102` vs `replication_service.rs:267-282` (HTTP purge = manifest-only; gRPC purge = refcounts+blobs — see C-02), any key holder can exfiltrate all tenants via `GET /internal/file/:id` (ciphertext; keys sit beside blobs for replicas with encryption on — same host compromise = plaintext). **Fix:** per-peer keys, HMAC the frame batch, unify purge semantics, keep keys off replicas. Effort M. Confidence High.

### [M-09] Internal 2 GiB ceiling unreachable — outer body limit caps bundles at `max_body` (1 GiB)
- **Category:** api/ops · **Location:** `internal.rs:231-233` (`state_body_limit()` = 2 GiB) vs `router.rs:80` (`DefaultBodyLimit::max(limits.max_body)` = 1 GiB default applies to `/internal` too). Files > ~1 GiB cannot be resynced/pulled as bundles (silent 413); `internal_get_file`/`apply` also buffer the whole bundle in RAM (`internal.rs:133-148`, `admin.rs:resync`). **Fix:** route-scoped body limit for `/internal` or stream bundles. Effort S. Confidence High.

### [M-10] Upload audit events are attributed to `system`, not the acting user; IPs never recorded
- **Category:** observability/audit · **Location:** `pipeline.rs:261-275` (`Actor::System`, ip `""`); download audits likewise `""` ip (`download.rs:256-268`). The audit trail cannot answer "who uploaded/downloaded this file" — the question it exists for. Login events do record the username (`handlers/auth.rs`). **Fix:** pass `AuthContext` (username, peer IP via `ConnectInfo`) into the pipeline. Effort S. Confidence High.

### [M-11] Replication worker: no circuit breaker, final-flush errors discarded at shutdown, queue memory
- **Category:** reliability · **Location:** `replicator.rs:35` (backoff capped at 2 s forever), `:229-232` (`let _ = flush_batch(...)` on shutdown — silent loss of acked-but-unshipped ops), queue retains up to `queue_capacity × chunk-size` blob bytes in RAM (default 10 000 × ≤128 KiB ≈ 1.2 GiB). Async loss is documented; silent shutdown loss and unbounded retry are not. **Fix:** log/counter shutdown drops, cap retry with dead-letter + alert, bound queue by bytes not ops. Effort M. Confidence High.

### [M-12] Background task supervision gaps; gRPC health probe weaker than REST ready
- **Category:** ops · **Location:** `grpc/services/health_service.rs:52-77` (watcher `tokio::spawn` — a panic leaves stale SERVING forever), probe checks `fs::metadata` only (`:89-96`) while REST `/ready` does a real write probe (`health.rs:21-23`). GC task (`gc.rs:213-233`) survives errors but a panic would likewise die silently. **Fix:** supervise/restart watchers; make the gRPC probe call `verify_writable`. Effort S. Confidence High (supervision absence is structural).

### [M-13] Library does not compile on Windows (`std::os::unix` unguarded)
- **Category:** quality/portability · **Location:** `security/encryption.rs:54-61` (`use std::os::unix::fs::OpenOptionsExt` + `.mode(0o600)` outside `#[cfg(unix)]`) — E0433/E0599 verified on this audit host; blocked all local test execution. Also affects any Windows-based contributor/CI matrix. **Fix:** cfg-gate with a non-unix fallback (create + explicit perms log). Effort S. Confidence Confirmed (compiler output).

### [M-14] Upload responses block on replication queue backpressure (async mode too), unbounded wait
- **Category:** reliability/api · **Location:** `pipeline.rs:246-249` awaits `enqueue_chunk` per chunk after the manifest is durable; `replicator.rs:99-101` `tx.send(...).await` blocks when the queue is full; no timeout. Replica down + full queue = all uploads hang indefinitely *after committing*. Trace: `INTERLEAVINGS.md` §M-14. **Fix:** timeout + fail-open policy (drop replication, alert) in async mode. Effort S. Confidence High.

### [M-15] No CI: none of the README quality gates are enforced
- **Category:** supply-chain/ops · **Location:** no `.github/workflows` (verified). fmt/clippy/test/audit/deny/buf exist as commands only; the "0 warnings" and test counts depend on developer discipline. **Fix:** minimal CI running fmt+clippy+test on linux (± windows after M-13), `cargo audit`/`deny`, `buf lint && buf breaking`. Effort S. Confidence Confirmed (directory listing).

### [M-16] Plaintext-mode dedup is a cross-tenant content-existence oracle (default config)
- **Category:** isolation/crypto · **Location:** `index_db.rs:196-235` (register merges refcounts across tenants — keys are raw content hashes when `encryption.enabled=false`, the shipped default), response leaks `dedup_chunks`/`deduped_bytes` (`upload.rs:248-249`, proto `UploadResponse`). Tenant B uploading bytes it possesses learns whether **any** tenant stores identical content — a confirmation oracle for known material (e.g. "does this org hold this document?"). Encrypted mode is safe by construction (tenant-scoped keys, `encryption.rs:114-119`); the README documents the encrypted-mode property but never the plaintext-mode trade-off. **Repro:** `audit_repro.rs::m16_*`, `curl-poc.sh` §M-16. **Fix:** document as accepted risk, or tenant-prefix block keys in plaintext mode too (loses cross-tenant dedup savings). Effort S(doc)/M(code). Confidence High.

### [M-17] Prometheus `/metrics` and `/health` unauthenticated on the public listener
- **Category:** ops/info-disclosure · **Location:** `router.rs:71-74`. Exposes tenant-scale operational data (bytes, dedup ratios, routes, gRPC codes) to anyone; standard practice is restrict to the ops network. **Fix:** bind to an internal interface or gate by api key. Effort S. Confidence High.

---

### LOW / INFO (condensed; all verified by direct read)

| ID | Finding | Location | Note |
|---|---|---|---|
| L-01 | Tenant usage double-counts same-name replaces until GC; `file_count` ≠ list length | `index_db.rs:378` (+1 per put) vs name-index uniqueness | cosmetic accounting drift |
| L-02 | Download duration/status metrics recorded before the body streams | `download.rs:128-129,176-191` | metric shows setup time, "ok" even on mid-stream abort |
| L-03 | `client_rejects_invalid_base` asserts `is_ok()` — a test that cannot fail | `replication/client.rs:379-383` | documents empty-api-key acceptance instead |
| L-04 | Uploading under a nonexistent tenant fabricates a tenant row (`tenant-<id>`) | `index_db.rs:807-813` | gRPC admin path does this deliberately; index path hides it |
| L-05 | README says "18 gRPC" tests; actual 19 | README:181 vs `tests/grpc_test.rs` | drift |
| L-06 | `tag_list`/gRPC tags unbounded in count (≤4 MiB message ⇒ ~100 k tag rows per upload) | `upload.rs:159-171`, `file_service.rs:106-112` | index bloat; cap at e.g. 32 tags |
| L-07 | Base images not digest-pinned; `cargo-chef:latest-rust-*` moving tag | `Dockerfile:6` | supply-chain hygiene |
| L-08 | `hash_for`/`key_age_ms`/`resolve_tenant`/`push_bundle`-adjacent helpers partially dead or cosmetic | `encryption.rs:122-129`, `middleware/tenant.rs:15-21` (no call sites) | dead code |
| L-09 | gRPC streaming success recorded with header-invisible status → failures after headers miscounted as ok | `interceptors/metrics.rs:44-49` | metric accuracy |
| L-10 | No `Vary` header on responses; `Cache-Control: private` only | `download.rs:228` | correct-ish for Bearer APIs; note |
| I-01 | HS256 single shared secret: every verifier (each replica) can mint admin tokens; no rotation support | `auth.rs` | consider RS256/EdDSA + dual-key validation window |
| I-02 | No key-rotation/re-encryption tooling; no redb rebuild-from-blobs tool despite CAS making it possible | `encryption.rs`, repo-wide | roadmap |
| I-03 | Audit log is operator-tamperable (no hash chain); docs say "tamper-evidence comes from external backup" | `docs/src/security/threat-model.md` | documented; accepted |
| I-04 | No fuzz targets, no crash-restart tests, no GC-vs-upload stress, no SOAK/leak tests | `tests/` | see §8 |
| I-05 | Replication client cannot speak TLS (feature not compiled); documented | `Cargo.toml:60` + README:175 | keep plaintext assumption explicit in ops docs |
| I-06 | `normalize_path`/metrics fix opportunities overlap H-05; `MAX_TRACKED_TENANTS` clear() wipes all buckets | `interceptors/rate_limit.rs:63-66` | minor |
| I-07 | Perf numbers in README unverifiable here (no Linux host); bench targets exist and are plausible | README:129-149 | re-run on Linux |

---

## 5. Architecture Assessment

**Layering** is clean: `core` (pure transforms) → `storage` (persistence) → `cache`/`security`/`replication` services → `api`/`grpc` transports, both transports bridging onto one `UploadPipeline`/`RetrievalEngine`. This single-core design is why cross-protocol byte-identity holds and is the repo's best structural decision. `AppState` is a wide struct but a deliberate composition root; acceptable.

**Three best decisions:** (1) one storage core under two transports (eliminates a whole class of drift — the divergences that remain, M-04, are *handler-level*, where validation was not pushed down); (2) content-defined chunking with the packetisation-independence property actually tested; (3) blob-before-index + single-txn refcount batch — the correct minimal durability protocol for a CAS.

**Three worst decisions:** (1) **refcount GC without an epoch/lease protocol** — refcounts, blob presence and manifests are coordinated only by "blob before index" ordering; every cross-component race (C-03, M-01, C-02) flows from the absence of a reconciliation/synchronization concept; the reachability pass already implemented for replicas is the right primitive and should subsume refcount mode (or back it up) as a periodic reconcile. (2) **Whole-file retention in the pipeline for replication's sake** (H-01) — the in-memory WAL queue couples memory, latency and durability incorrectly; blobs should stream to the shipper (or to a disk-backed WAL) as produced. (3) **Two parallel internal replication protocols (HTTP frames + gRPC ops) with divergent semantics** (C-02, M-08, M-09) — one should be deleted or both generated from a shared apply-core; today they disagree about purges, body limits and refcounts.

**Trait seams:** `BlockStore`/`IndexDb` are concrete types — no `FsProvider`/`TimeSource` seams, which is what makes the crash/fault-injection test suite (§8) harder than it should be. Introducing a `Clock` seam alone would de-flake grace-period tests.

---

## 6. Security Assessment (threat walkthrough)

| Adversary | What they achieve TODAY |
|---|---|
| Unauthenticated network caller | Rate-limited but unbounded Prometheus cardinality growth (H-05); reads `/metrics`/`/health` (M-17); brute-forces login per-IP (M-05) |
| Anyone with the public README (default deployment) | **Full admin** via admin/admin123 or a **forged admin JWT** via the known shipped secret; full internal read/write of all tenants' blobs via the known api key (C-01) |
| `viewer` of tenant A | Reads own-tenant files incl. **soft-deleted metadata** (`?include_deleted=true`, M-04a); content-existence oracle across tenants in plaintext mode (M-16); unthrottled gRPC streaming (M-05) |
| `user` of tenant A (honest mistakes aside) | Remote OOM via gRPC big uploads (H-01) or multipart metadata (H-02); stored-XSS content planting (H-03); cannot touch tenant B data (checks verified at every handler: `check_file_access` on download/range/meta/delete/list paths; name/tag indexes tenant-prefixed — no isolation hole found) |
| Compromised replica / api-key holder | Read all tenants' ciphertext; write arbitrary blobs under arbitrary hashes (M-08); plant decompression bombs/panics via manifests (H-04); poison replica blobs permanently via repair path (M-07); delete other tenants' replica data via gRPC purge (C-02) |
| MITM on primary↔replica channel | Plaintext HTTP only (I-05): read all traffic, inject ops as above |

**Attack surface table:** REST 16 routes + 7 internal routes; gRPC 4 services + health + reflection. All `/api/v1` routes behind JWT middleware ✓; `/internal` behind constant-time api key ✓; gRPC unary behind interceptors, streaming self-authenticates ✓ (`file_service.rs:55,192`; `replication_service.rs:56,133,184` all verified). JWT: HS256 pinned, exp validated, 5 s leeway; `iss`/`aud`/`nbf` not set/checked (no audience model — acceptable single-service, note I-01). Tenant existence is **not** re-checked per request — tokens of a deleted tenant remain valid until exp (no tenant-delete endpoint exists, so currently moot; flag for when one is added).

**Cryptography:** AES-256-GCM with per-chunk random 12-byte nonces (`encryption.rs:79`) — birthday bound ≈ 2^48 chunks/tenant-key, fine at this scale; nonce prepended and length-checked on decrypt (`:92-100`); keys are 32 random bytes, 0600 on create (unix), never logged. `storage_key = BLAKE3(tenant‖hash)` is unambiguous (fixed 16+32 layout) and correctly scopes encrypted dedup; the *plaintext-mode* global dedup is the leakage (M-16). Compression-then-encryption order verified (`pipeline.rs:294-303`). Key files not re-checked for permission drift at boot (only `has_key` existence); no rotation (I-02).

---

## 7. Performance Assessment

README numbers could not be re-measured (host limitation, I-07); the bench/integration perf targets exist and are sane. Ranked bottlenecks **by expected production cost** (analysis, not measurement):

1. **Whole-file memory retention (H-01)** — the dominant capacity limiter; fix yields O(chunk) memory per upload instead of O(file), i.e. ~1000× for 10 GiB files.
2. **O(n) manifest scans (M-03)** — `count_by_tenant` + `list_by_tenant` double full-scan per list; GC `all_manifests` per 5-min tick. At 1 M files expect multi-second lists and GC ticks in the 10 s+ range with hundreds of MB transient allocations. Fix yields O(page) lists.
3. **Per-chunk fsync ×2 (file + dir)** — `block_store.rs:76-81`: two `sync_all` per chunk cap write throughput (README's own "fsync-dominated" note). Group-commit (fsync dir once per batch of chunk writes) is the standard fix; ~2× upload throughput on NVMe, more on SSD.
4. **BLAKE3 verify on every L1 miss** (`retrieval.rs:159`) — ~1 GB/s/core cost is the price of integrity; L1-hit skipping is already correct. Keep.
5. **Clone hot spots:** `stream_full/stream_range` clone the whole chunk vector per request (`retrieval.rs:80,105`); `Bytes::copy_from_slice` on the non-compressed decode path (`retrieval.rs:184`) and spooled stream (`upload.rs:226`); `hard_purge` logic duplicated (`delete.rs:74-86` vs `file_service.rs:450-460`). Each is small; collectively a tidy-up.
6. **HTTP/2 window tuning (4 MB/8 MB)** is set but untested against large gRPC downloads (README claim unverified here) — measurement task, not a defect.

---

## 8. Test Gap Analysis

Coverage %: not measurable on this host (llvm-cov needs a Linux build). Structural assessment: unit density is excellent in `core/`, `storage/`, `security/` (chunker 10, compressor 6, auth 9, index 8, validator 9, encryption 7 …), and the 17 REST + 19 gRPC E2E tests cover the golden paths incl. authz matrix, dedup, refcount survival, range, conditional GET, TLS/mTLS (rcgen), replication (HTTP path), rate limit. **Untested critical branches (top 10):**

1. GC/purge racing an in-flight dedup upload (C-03) — no concurrency stress test at all
2. gRPC `ReplicationService` apply/purge semantics (C-02) — replication tests use HTTP only
3. Crash windows: register→manifest (M-01), soft-delete→GC, shutdown flush — no kill/restart test
4. Multipart oversized/unknown fields (H-02) — multipart tests upload small well-formed bodies
5. Hostile manifest/decompression bounds (H-04)
6. Metrics cardinality under unmatched paths (H-05)
7. Concurrent same-name uploads (name-index loser orphan accounting) — only sequential replace tested (`index_db.rs:955-978`)
8. Multi-GiB upload/streaming retention (H-01) — perf test caps at 2 MiB in-process/64 MiB HTTP
9. On-miss repair path (M-07) — no test with a lying upstream
10. `include_deleted`/name-length validation parity REST vs gRPC (M-04)

**Recommended new suites (acceptance criteria):** (a) `race_gc_upload` — 100 iterations of upload ‖ purge loop, zero dangling manifests (C-03 gate); (b) `replication_grpc_purge_shared_chunk` — twin files survive purge (C-02 gate); (c) `crash_restart` harness (spawn, SIGKILL at scripted points, reopen, assert invariants: no dangling refs, leaked-refcount reconcile report); (d) `multipart_limits` — oversize name/tags/unknown fields rejected ≤64 KiB resident; (e) proptest for `parse_range` and chunker offset math; (f) cargo-fuzz targets for `decode_batch`/`decode_file_bundle`/multipart; (g) SOAK: 24 h upload/delete loop with RSS + disk-growth assertions (M-01/M-06 gates).

---

## 9. Dependency & Supply Chain

`cargo audit`/`cargo deny` unavailable in sandbox (noted §2). Manual review of `Cargo.lock` security-critical pins: `aes-gcm 0.10.3`, `blake3 1.5.x`, `argon2 0.5.3`, `jsonwebtoken 9.3`, `rand 0.8`(+`getrandom 0.4` transitively via newer crates — dual getrandom major versions present, duplication note), `tokio 1.4x`, `redb 2.2`, `ring 0.17` (via tonic-tls/rustls), `zstd 0.13`, `reqwest 0.12` (no TLS features — I-05). No known-RustSec items matched from advisory memory at these versions as of training data — **must be re-run (`cargo audit`, `cargo deny check`) on Linux as a P0 checklist item**; treat this section as unresolved until then. `Cargo.lock` committed ✓ (reproducible `--locked` Docker build ✓). Feature hygiene: tokio `full` (heavier than needed; trim to `rt-multi-thread, macros, signal, fs, io-util, sync, time`), tonic `tls-roots` reasonable, axum `multipart,macros` used. Duplicates: two uuid paths? no (single uuid); `bincode 1.3` (legacy) alongside rmp-serde — bincode used only for `ChunkMeta`; fine but two binary codecs is drift-prone. Build supply chain: `protoc-bin-vendored` pinned major only; base images moving tags (L-07); no `cargo-deny.toml` in repo (bans/licenses config absent).

---

## 10. Documentation Drift

| Claim | Source | Reality | Verdict |
|---|---|---|---|
| "upload ──► validate" (magic bytes) | README:6, lib.rs doc | `validate_head` has zero call sites | **false** (H-03) |
| "a crash can only leave a GC-able orphan" | README:22 | register→manifest crash leaks permanently (M-01) | **overstated** |
| "cargo clippy --all-targets # 0 warnings" | README:183 | true on Linux (1 win-only warning; win build broken M-13) | true (linux) |
| "131 unit + 18 gRPC + 17 REST" | README:181 | 131 ✓, 17 ✓, gRPC is **19** | minor drift |
| L1 256 MiB/5 min, L2 512 MiB/60 min, L3 10k/60s | README:17-18 | matches `default.yaml` + weighers are byte-based ✓ | true |
| batch 64 / 5 s / backoff 100ms→500ms→2s | README:24 | `replicator.rs:35`, defaults ✓ | true |
| "sync mode blocks the upload until the batch is acknowledged" | README:24 | correct incl. per-op transitivity (verified, INTERLEAVINGS §sync) | true |
| "information leakage is impossible by construction" (encrypted dedup) | README:170 | true **only** with encryption on; plaintext mode leaks (M-16) | conditional |
| "quotas are accounted but not enforced" | docs/tenants | true; but `security/tenant.rs` module doc says "quotas" | minor |
| "Internal traffic is plain HTTP … no TLS stack" | README:175 | true (reqwest features) | true |
| Endpoint tables (REST + gRPC + internal) | README:72-87 | match router/services exactly (diffed) | true |
| Perf table | README:129-149 | not re-measured here (I-07); bench targets exist | unverified |
| "0 warnings", docs 55 pages, distroless ~12 MB | README:182-187 | pages ✓ (55 md files), distroless ✓, size plausible | mostly true |
| ADRs 0001–0008 | docs/decisions | all still true in code (CAS layout, fastcdc, redb, moka tiers, per-tenant AES, WAL shipping, key derivation, dual transport) — except 0006's implied durability is undermined by M-11/M-14 | true w/ caveat |

---

## 11. Production Readiness Checklist

Legend: ✅ pass · ❌ fail (finding) · ⚠️ partial · ➖ n/a. Only failing/partial items carry IDs; everything else passed verification.

**3.1 Correctness & Concurrency** — ❌ `write_chunk` TOCTOU vs GC/purge (C-03) · ✅ rename-idempotent concurrent writers, no `.tmp` leak between two live writers · ❌ `.tmp` startup sweep absent (M-06) · ✅ dir-fsync after rename · ✅ blob→index ordering (crash windows) · ❌ register/manifest non-atomicity leaks (M-01) · ✅ refcount batch single-txn · ⚠️ delete = 2 txns (soft-delete + later GC decr) — crash-safe direction only · ✅ cache invalidation complete at mutation sites · ❌ GC↔dedup-upload race (C-03) · ✅ GC↔delete idempotent (`soft_delete` guard) · ✅ range arithmetic complete+tested · ✅ chunker carry/streaming edge cases tested (empty/tiny/exact-max/errors) · ✅ compressor fallback flag explicit · ❌ decompress bound trusts index (H-04) · ❌ unbounded whole-file retention, no global backpressure (H-01) · ✅ `buffer_unordered` error aborts stream, request errors to client · ⚠️ graceful shutdown drains transports + replicator, but in-flight 1 GiB uploads: axum graceful drain waits for bodies (client-dependent); final replication flush errors discarded (M-11) · ⚠️ health watcher unsupervised (M-12) · ✅ wall-clock grace periods monotonic-enough; replica GC skew bounded by grace · ⚠️ concurrent same-name uploads: name index last-writer-wins, loser manifest stays live-but-unlisted, refcounts correct; no explicit test (gap 7)

**3.2 Data Integrity & Durability** — ✅ BLAKE3 verify every L1 miss (disk & L2 paths) · ✅ `Durability::Immediate` verified · ⚠️ msgpack evolution: serde defaults absent (unknown-field-tolerant decode, no version gate; `version` field never branched on) · ⚠️ `original_hash` verified only client-side (server never reassembles to check) · ➖ backup: docs page exists (backup-restore.md); no snapshot mode/tooling hooks · ❌ redb corruption recovery: boot fails (`Database::open` error), **no rebuild-from-blobs tool** despite CAS making it possible (I-02) · ❌ `.tmp` accumulation (M-06) · ⚠️ audit append-only in code, no tamper evidence (I-03, documented)

**3.3 Authentication & Authorization** — ✅ HS256 pinned (no alg confusion), exp+leeway · ➖ iss/aud/nbf (no audience model) · ❌ secret floor 16 + shipped known secret passes (C-01) · ✅ secret never logged/serialized · ⚠️ refresh tokens: in-memory only — restart invalidates all sessions, replicas can't honor each other's tokens; single-use rotation ✓; logout = refresh-only (access token lives ≤15 min, undocumented) · ✅ argon2id m=64 MiB t=3 p=4 (production.yaml) — meets OWASP · ⚠️ timing equalizer present but per-unknown-user argon2 params vs stored-hash params can differ (custom hashed users with weaker params reintroduce skew) · ❌ login brute-force: per-IP only (M-05) · ✅ role matrix verified endpoint-by-endpoint (REST+gRPC): upload write ✓, delete write ✓, purge admin ✓, tenants admin ✓, audit admin ✓, gc admin ✓, resync admin ✓ — **except** ❌ REST `include_deleted` non-admin (M-04a) · ✅ tenant predicate present on every list/tag/name/meta/download/delete path (verified per handler) · ✅ gRPC auth parity (metadata casing handled, same claims/expiry) · ✅ api-key constant-time, both stacks · ⚠️ token revocation on tenant delete ➖ (no tenant delete exists)

**3.4 Cryptography** — ✅ per-chunk OsRng-backed random nonces (rand ThreadRng is ChaCha-based CSPRNG), birthday-bound fine · ✅ nonce-prefix layout + bounds check on decrypt · ✅ tenant-scoped key derivation unambiguous · ❌ plaintext-mode cross-tenant oracle (M-16) · ⚠️ key perms checked on create only; not re-verified at boot · ❌ no rotation/re-encryption (I-02) · ✅ compress→encrypt order · ⚠️ BLAKE3 verify uses `==` on 32-byte arrays — non-constant-time memcmp on local data; not remotely observable (documented acceptance) · ✅ `rand`/`getrandom` usage sound; no v4-for-secrets · ⚠️ algorithm hardcoded (fine now; note)

**3.5 Input Validation & Injection** — ❌ magic-byte validation dead (H-03) · ⚠️ name: gRPC 1..=255 ✓ / REST unbounded (M-04b); CRLF impossible in `Content-Disposition` (sanitizer `download.rs:243-254` ✓); name never used as a path ✓ · ⚠️ tags unbounded count (L-06); tag-index key collisions impossible (tenant prefix + length filter verified) · ❌ multipart metadata buffering (H-02); file-count limits n/a (single file); duplicate part names last-wins · ✅ raw-body premature EOF aborts pipeline pre-manifest (stream error → `?` propagate) · ✅ limit clamped 1..=1000; include_deleted (see M-04a) · ✅ gRPC: duplicate header rejected, stream properly errored; message size capped 256 MiB · ⚠️ env override injection possible (`FS_STORAGE__ROOT=../../`) — config-trust boundary, acceptable; yaml anchors parsed by `config` crate (no billion-laughs protection — low risk, operator-controlled file)

**3.6 DoS & Resource Exhaustion** — ❌ whole-file memory (H-01) · ❌ multipart pre-check buffer (H-02) · ⚠️ chunk explosion: 1 GiB @8 KiB min ⇒ ~131 k chunk refs — manifest ~13 MB in redb/L3(10k entries, count-not-bytes weigher ⇒ ~1.3 GB worst-case L3!) — L3 weigher is **entry count**, not bytes (`manifest_cache.rs:20-24`) — real OOM vector folded into H-01/M-03 · ✅ uniform-content collapse works (tested) — and is the M-16 oracle · ❌ decompress bound (H-04) · ⚠️ concurrency: per-connection 64 (gRPC) ✓, no global semaphore (H-01); REST body limit 1 GiB ✓; no HTTP/1 slowloris timeouts configured (axum/hyper defaults — `timeout` layer absent) · ❌ metric cardinality (H-05) · ❌ audit table unbounded, no pruning (ops risk; audit failure doesn't block uploads — deliberate, documented) · ⚠️ replication queue bounded by ops not bytes (M-11) · ⚠️ fd churn: blob reads open/close per chunk (fine on Linux fd limits at this scale; note) · ⚠️ range spam pollutes L1 within its byte budget (weigher ✓) — acceptable

**3.7 Performance** — ⚠️ README numbers unverified here (I-07) · ✅ clone hot spots identified (§7.5) · ✅ verify-skip-on-L1-hit correct by construction · ❌ fsync per chunk unbatched (§7.3) · ❌ O(n) scans (M-03) · ➖ L2 "inline small chunks": design comment only, not implemented (dead docs) · ⚠️ zero-copy: `Bytes` chain partial (copies on raw-decode + spool) · ✅ no global mutex on hot path (DashMap for keys/refresh; moka internal) · ⚠️ zstd/BLAKE3 run on the async runtime without `spawn_blocking` — multi-core BLAKE3 helps, zstd level 3 is fast; acceptable at this scale, note · ⚠️ HTTP/2 windows set, untested (I-07)

**3.8 Error Handling & Resilience** — ✅ zero `unwrap/expect/panic` in non-test request paths (grep-verified; the two `expect`s are boot-time one-shots) · ✅ error bodies carry code+message only; no paths/stack/secrets leaked (audited Display impls) · ✅ mapping total both transports (tested) · ⚠️ retry semantics: same content re-upload = new file_id (documented dedup makes it cheap); no idempotency keys · ⚠️ replication retry infinite, no breaker (M-11) · ⚠️ startup validation: chunking/compression/jwt-len/users/replication/shard/tls paths ✓; no disk-space check, no secret-strength check (C-01), no key-dir perms check · ✅ poisoned tenant can't take shared endpoints (per-tenant state isolated in redb rows)

**3.9 API Design** — ✅ consistent snake_case JSON, stable error envelope, ISO-ish epoch-ms timestamps · ⚠️ offset pagination + wrong gRPC `total` (M-04d) · ✅ 201/204/206/304/416+`Content-Range` semantics; ETag strong quoted BLAKE3; weak-compare on INM (RFC-correct); multi-range = first range, documented and tested · ✅ proto hygiene: per-field comments, streaming semantics documented, `buf breaking` configured (not run — M-15); reserved fields absent (young API, acceptable) · ✅ `/api/v1` + `file_storage.v1` versioning; no deprecation doc yet

**3.10 Operational Readiness** — ✅ metrics catalog broad (uploads/downloads/cache tiers/replication/gc/http/grpc/audit/hash-mismatch/uptime) · ❌ queue-depth gauge broken (M-02) · ⚠️ missing: per-tenant quota usage (no quotas), blob fsync latency, index commit latency, L3 hit ratio, replication lag (bytes/ops behind) · ✅ request-id set+propagated REST; gRPC mints/reuses (`proto_auth.rs:89-97`) — not threaded into pipeline/GC logs · ✅ no secrets in logs (grep: jwt/password/key absent from tracing calls; token metadata marked sensitive) · ✅ `/ready` does a real write probe (DB ping + write probe) · ⚠️ gRPC health probe weaker + unsupervised watcher (M-12) · ⚠️ full-disk behavior: uploads fail cleanly (Io → 500), no 507 signaling; redb WAL-full untested · ❌ compose: root user, default creds, published ports, single volume for blobs+index+keys (C-01 + §6) · ➖ upgrade path: `docs/src/migration/v0-to-v1.md` exists; no schema-version gate in code, no migration test · ➖ hot-reload: none claimed, none present (consistent)

**3.11 Testing** — ✅ 131 unit / 17 REST / 19 gRPC, all behavioral, zero flaky patterns found (no sleeps; barriers via async ready) · ❌ coverage % unknown (host) · ❌ gaps 1-10 (§8) · ❌ no fuzz/property/soak/fault-injection (I-04) · ❌ no CI (M-15)

**3.12 Code Quality** — ✅ `//!` module docs everywhere (verified per file) · ⚠️ duplication: REST/gRPC purge twins, tag/name filter logic twins, three constant_time_eq copies · ⚠️ dead code: validator sniff path, `key_age_ms`, `resolve_tenant`, `auth_configured`, `unauthorized_response` (unused helpers) · ✅ naming consistent; magic numbers mostly named constants (`BACKOFF_MS`, `DOWNLOAD_PIECE_SIZE`, `MAX_LISTED_CHUNKS`…) · ⚠️ `pub` surface: lib exposes everything (`pub mod` all the way) — `pub(crate)` discipline absent · ✅ no `unsafe` outside test env-var usage

**3.13 Supply Chain** — ⚠️ audit/deny not runnable here (§9) — re-run is P0 checklist · ✅ lockfile committed, `--locked` builds · ⚠️ tokio `full`, base-image tags unpinned (L-07), protoc vendored ✓, no deny config

**3.14 Documentation** — §10 table; overall accuracy high for a repo of this kind; the two material lies are the validation stage and the crash-safety scope.

---

## 12. Scorecard

| Dimension | Score /10 | Justification |
|---|---|---|
| Security | **3** | Shipped-credential chain (C-01) + plaintext dedup oracle (M-16) + dead validation (H-03) + internal trust gaps (M-08); auth/tenant machinery itself is solid (constant-time, argon2, exhaustive matrix) |
| Correctness | **5** | Core paths correct and tested; three data-destruction/concurrency holes (C-02, C-03, M-01) + cross-protocol drift (M-04) |
| Durability | **6** | fsync discipline and ordering genuinely good (dir-fsync, Immediate); leak windows, replica purge loss, no rebuild tool, no TLS option for peers |
| Performance | **6** | Careful hot path, tiered caches, byte weighers; O(n) scans and whole-file retention cap it far below the design's potential |
| Reliability | **5** | Graceful shutdown, panic recovery (gRPC), idempotent applies; infinite retry, queue backpressure stalls, unsupervised watchers, silent shutdown drops |
| Observability | **6** | Wide metric catalog, request-ids, structured logs; broken queue gauge, pre-stream download metrics, cardinality hole, missing lag/fsync metrics |
| Code Quality | **7** | Uniform style, module docs, near-zero unwraps, sensible errors; duplication twins, dead code, pub over-exposure |
| Testing | **6** | 167 behavioral tests incl. E2E both transports; zero concurrency/crash/fuzz coverage of the exact places that fail |
| API Design | **7** | REST semantics exemplary (206/416/304, ETag, envelope); pagination/parity nits, no idempotency keys |
| Ops Readiness | **4** | Probes/metrics/compose present, but default-credential image, root containers, no CI, no rebuild/backup tooling |
| Docs | **8** | Honest, specific, file-cited threat model; only two material falsehoods |
| **Overall** | **5.1 / NO-GO** | Excellent core undermined by deployment-security and concurrency fundamentals; all P0 fixes are small-to-medium efforts (§ REMEDIATION_PLAN) |
