# file-storage

A production-grade **hybrid chunked content-addressed file storage microservice** in Rust.

```
upload ──► validate (magic bytes, enforce|off) ──► FastCDC chunk ──► BLAKE3 hash ──► zstd ──► AES-256-GCM (opt.)
                                                                     │
        ┌────────────────────────────────────────────────────────────┘
        ▼
   block store  ab/cd/<hash>.bin   (atomic temp+rename+fsync, idempotent)
        │
        ▼
   redb index   chunks (hash→refcount) · files (id→manifest) · name/tag index
                · audit (append-only) · tenants
        │
        ▼
   moka cache   L1 plaintext (256 MiB / 5 min) · L2 blobs (512 MiB / 60 min)
                · L3 manifests (10k / 60 s)
```

* **Content validation (H-03)** — the declared content-type is checked against sniffed magic bytes of the first chunk (`validation.mode: enforce` default; `off` is a loud operator opt-out). Downloads always carry `X-Content-Type-Options: nosniff`, active-content types are forced to `Content-Disposition: attachment`, and responses carry `Vary: Authorization`.
* **Content addressing** — chunks are keyed by BLAKE3; identical content is stored once and reference-counted (dedup ≥2× on duplicate-heavy workloads, uniform content collapses to a single chunk).
* **Crash safety** — blobs are fsynced *before* the verified index transaction that references them (C-03: registration re-checks blob presence inside the same transaction, and the uploader self-heals against racing GC/purge). A crash can leave a GC-able orphan or — in the transaction-sized register→manifest window — a refcount row reclaimed by the periodic reconcile pass (`retention.reconcile_interval_secs`, default hourly — M-01, now implemented). redb commits are `Durability::Immediate` (WAL + fsync).
* **Soft delete + GC** — deletes are tombstones; a background collector purges after a grace period, drops zero-refcount blobs and sweeps orphans (refcount mode on primaries, reachability mode on replicas).
* **Replication** — async WAL-style shipper (batch 64 ops / 5 s flush, exponential backoff 100 ms → 500 ms → 2 s) to a replica, or sync mode that blocks the upload until the batch is acknowledged. Replicas can pull missing blobs on demand from the upstream primary.
* **Read path** — L1 → L2 → disk with BLAKE3 verification after every disk load, N+1..N+4 read-ahead prefetch, HTTP `Range` (206/416) and `ETag`/`If-None-Match` (304).

## Quick start

```bash
cargo run --release -- --config config/default.yaml   # REST :8080 + gRPC :9090
# or
docker compose up --build          # primary :8080/:9090, replica :8081/:9091, prometheus :9099
```

> 📚 **Full documentation suite** — `mdbook build docs/` renders `docs/book/index.html`
> (architecture, API reference for both transports, configuration, deployment, security,
> operations, integration, performance — 55 pages).

```bash
# login
TOKEN=$(curl -s -X POST localhost:8080/api/v1/auth/login \
  -H 'Content-Type: application/json' \
  -d '{"username":"admin","password":"admin123"}' | jq -r .access_token)

# raw upload (streamed)
head -c 64M /dev/urandom > /tmp/f.bin
curl -s -X POST "localhost:8080/api/v1/files?name=f.bin&tags=demo" \
  -H "Authorization: Bearer $TOKEN" -H 'Content-Type: application/octet-stream' \
  --data-binary @/tmp/f.bin | jq

# multipart upload
curl -s -X POST localhost:8080/api/v1/files \
  -H "Authorization: Bearer $TOKEN" \
  -F name=report.pdf -F tags=q1,finance -F file=@report.pdf | jq

# download / range / conditional
curl -s -H "Authorization: Bearer $TOKEN" localhost:8080/api/v1/files/<id> -o out.bin
curl -s -H "Authorization: Bearer $TOKEN" -H 'Range: bytes=0-1023' \
     localhost:8080/api/v1/files/<id>/range -o part.bin
curl -s -D- -o /dev/null -H "Authorization: Bearer $TOKEN" \
     -H "If-None-Match: <etag>" localhost:8080/api/v1/files/<id>   # 304

# metadata / list / delete
curl -s -H "Authorization: Bearer $TOKEN" localhost:8080/api/v1/files/<id>/meta | jq
curl -s -H "Authorization: Bearer $TOKEN" 'localhost:8080/api/v1/files?tag=q1&limit=20' | jq
curl -s -X DELETE -H "Authorization: Bearer $TOKEN" localhost:8080/api/v1/files/<id>          # soft
curl -s -X DELETE -H "Authorization: Bearer $TOKEN" 'localhost:8080/api/v1/files/<id>?purge=true'  # admin hard purge
```

## API

| Method | Path | gRPC equivalent | Description |
|---|---|---|
| POST | `/api/v1/files` | `FileService/Upload` | Upload (multipart **or** raw body; `?name=&tags=a,b&mime=`) → 201 |
| GET | `/api/v1/files` | `FileService/ListFiles` | List (`?limit&offset&tag&name&include_deleted`) |
| GET | `/api/v1/files/:id` | `FileService/Download` | Download (ETag/304, `Cache-Control`; gRPC supports ranges) |
| GET | `/api/v1/files/:id/meta` | `FileService/GetManifest` | Manifest metadata (`?chunks=false` to skip chunk list) |
| GET | `/api/v1/files/:id/range` | `FileService/Download` (`start`/`end`) | Partial content, `Range: bytes=…` → 206 / 416 |
| DELETE | `/api/v1/files/:id` | `FileService/DeleteFile` | Soft delete; `?purge=true` (admin) hard-deletes |
| POST | `/api/v1/auth/login` / `refresh` / `logout` | `AuthService/Login|Refresh|Logout` | JWT (HS256) + rotating refresh tokens |
| POST/GET | `/api/v1/tenants` | `AdminService/CreateTenant|ListTenants` | Tenant admin (admin only) |
| GET | `/api/v1/audit?limit=` | `AdminService/QueryAudit` | Audit trail (admin only) |
| POST | `/api/v1/admin/gc` | `AdminService/TriggerGC` | Trigger a GC pass (admin) |
| POST | `/api/v1/admin/resync/:id` | `AdminService/Resync` | Push a full file bundle to the replica (admin) |
| GET | `/health`, `/ready`, `/metrics` | `grpc.health.v1.Health/Check` + reflection | Probes, Prometheus exposition |

Internal replication (guarded by `X-Api-Key`): REST `POST /internal/chunk`, `GET /internal/chunk/:hash`, `POST /internal/manifest`, `DELETE /internal/manifest/:id`, `POST /internal/batch`, `GET|POST /internal/file/:id` — plus the gRPC `ReplicationService` (`ReplicateBatch` bidi, `PullChunk`, `PullManifest`).

## gRPC API

The gRPC listener shares the storage core with REST: a file uploaded over gRPC is byte-identical
when downloaded over REST (and vice versa). 4 protobuf services + standard health + reflection
on `:9090` (`server.grpc_port`, `server.grpc_tls` for TLS/mTLS):

* `file_storage.v1.FileService` — `Upload` (client-streaming), `Download` (server-streaming, ranges), `GetManifest`, `ListFiles`, `DeleteFile`
* `file_storage.v1.AuthService` — `Login`, `Refresh`, `Logout`
* `file_storage.v1.AdminService` — `CreateTenant`, `GetTenant`, `ListTenants`, `TriggerGC`, `Resync`, `QueryAudit`
* `file_storage.v1.ReplicationService` — `ReplicateBatch` (bidi), `PullChunk`, `PullManifest` (internal, `x-api-key`)
* `grpc.health.v1.Health` + `grpc.reflection.v1.ServerReflection` — probe and `grpcurl` discovery

```bash
# discovery without any proto files (reflection)
grpcurl -plaintext localhost:9090 list

# health check
grpcurl -plaintext -d '{"service": "file_storage.v1.FileService"}' \
    localhost:9090 grpc.health.v1.Health/Check

# login (JWT goes into the authorization metadata)
grpcurl -plaintext -d '{"username":"admin","password":"admin123"}' \
    localhost:9090 file_storage.v1.AuthService/Login
```

Rust client SDK (see `docs/src/integration/rust-client.md`):

```rust
let mut client = file_storage::grpc::FileStorageClient::connect("http://127.0.0.1:9090").await?;
client.login("admin", "admin123").await?;
let resp = client.upload(
    pb::UploadHeader { name: "demo.bin".into(), ..Default::default() },
    std::io::Cursor::new(payload),
).await?;
let mut sink = Vec::new();
let header = client.download(&resp.manifest.unwrap().file_id, &mut sink).await?;
```

Roles: `admin` > `user` (read/write) > `viewer` (read-only). Every request is tenant-scoped; cross-tenant access requires admin. Errors are JSON: `{"error":{"code":"not_found","message":"…"}}`.

## Performance (measured)

Environment: 2-vCPU sandbox, NVMe-backed overlayfs, release build (`opt-level=3`, thin LTO off in this measurement, `strip=symbols`), 8-way chunk pipeline, zstd level 3.

| Metric | Measured | Note |
|---|---|---|
| CDC chunking (pure) | **1 134 MB/s** | 4 MiB input, 8 KiB/32 KiB/128 KiB min/avg/max |
| zstd compress (pure) | **1 824 MB/s** | 128 KiB blocks, level 3 |
| Upload pipeline (in-process, 2 MiB) | **273 MB/s** | chunk→hash→compress→blob fsync→redb txn |
| Upload via HTTP (64 MiB) | **119 MB/s** | random bytes (incompressible → stored raw); fsync-dominated |
| Download via HTTP, cold | **399 MB/s** | disk → verify → decompress |
| Download via HTTP, warm | **1 107 MB/s** | L1 plaintext hits |
| Range (1 MiB slice) | **0.9 ms** | cross-chunk slice |
| Retrieval path (L1 hit, in-process) | ~29 µs / 2 MiB | see `cargo bench` `download_2mb_*` |

Dedup: re-uploading the same 512 KiB file yields `dedup_chunks = chunk_count` and a single physical chunk; uniform content collapses to one chunk per hash. Run your own numbers:

```bash
cargo test --release --test integration_test perf_smoke -- --ignored --nocapture
cargo bench --bench throughput
```

## Configuration

`config/default.yaml` (dev) and `config/production.yaml` (template with argon2id hash + encryption + replication enabled). Every key can be overridden via env vars using `FS_` + `__` nesting:

```bash
FS_SERVER__PORT=9000 \
FS_STORAGE__ROOT=/data/blocks \
FS_ENCRYPTION__ENABLED=true \
FS_REPLICATION__ENABLED=true \
FS_REPLICATION__TARGET_URL=http://replica:8080 \
file-storage --config /etc/file-storage/default.yaml
```

Key sections: `chunking` (min/avg/max), `compression` (level, min_size), `cache` (L1/L2 sizes + TTLs, manifest capacity), `auth` (jwt_secret, TTLs, users with `password` or `password_hash`), `encryption` (enabled, keys_dir — per-tenant keys written `0600`), `rate_limit` (per_second, burst), `replication` (mode sync|async, target_url, upstream_url, api_key, batch_size, flush_interval_ms), `retention` (gc_mode refcount|reachability|disabled, grace_period_secs, gc_interval_secs, orphan_min_age_secs), `limits` (max_file_size, max_body).

Generate a password hash: `file-storage hash-password --password '…'` (argon2id PHC).

## Design notes

* **Dedup scoping.** Random nonces make ciphertext-addressed dedup impossible, so with `encryption.enabled=true` the block-store key becomes `BLAKE3(tenant_id ‖ chunk_hash)` (`ChunkRef.plain_hash` still holds the plaintext hash for read-back verification). **Plaintext-mode caveat (audit M-16, open ticket):** with encryption off, keys are raw content hashes — dedup is currently global across tenants, which is a content-existence oracle; the remediation plan (M-16) switches the default to tenant-scoped keys behind `dedup.scope`. Until that lands, treat plaintext-mode dedup as an accepted, documented risk.
* **Chunk boundaries are content-defined.** `fastcdc::v2020::AsyncStreamCDC` maintains its own rolling buffer; boundaries depend only on content, never on TCP packetisation (asserted by tests feeding the same bytes in different chunk sizes).
* **Compression fallback.** Payloads below `compression.min_size` or that do not shrink are stored verbatim with `compressed=false` (explicit flag, not magic sniffing); decompression is bounded by the recorded `size_original` so corrupt input cannot allocate unboundedly.
* **Same-name replacement** soft-deletes the previous manifest; its chunk refcounts keep the shared blobs alive until GC.
* **Multi-range requests** are served as their first range (documented, RFC-7233-legal).
* **Internal traffic is plain HTTP** inside the cluster (no TLS stack pulled in); terminate TLS at your ingress/load balancer for external traffic. This assumes a trusted VLAN or tunnel — the M-08 workstream adds per-peer keys + HMAC frame integrity and an optional TLS feature.
* **Replication queue is in-memory** with bounded capacity (backpressure on send). A crash can lose un-flushed ops on the *primary*; sync mode closes that window at the cost of upload latency. The replica-side `?purge`/tombstone handling and pull-based repair (`upstream_url`) cover reconciliation.

## Development

```bash
cargo test                 # 143 unit + 19 gRPC + 16 REST + 21 regression (race/purge/hardening/C-01 boot)
cargo test --release       # same, optimized
cargo clippy --all-targets # 0 warnings
cargo fmt --check
cargo bench --bench throughput
mdbook build docs/         # renders docs/book/index.html
docker build -t file-storage .   # distroless runtime, binary ~12 MB
```

Project layout:

```
src/
├── main.rs            thin binary (lib holds all logic)
├── lib.rs             CLI, boot, graceful shutdown, hash-password tool
├── config.rs          YAML + FS_* env overrides, validation
├── error.rs           StorageError + HTTP mapping + JSON bodies
├── state.rs           AppState assembly (db, stores, caches, services)
├── metrics.rs         metric names + recorders (Prometheus)
├── gc.rs              refcount & reachability collectors
├── api/               router, middleware (auth/api-key/rate-limit/request-id), handlers, DTOs
├── core/              validator, chunker (FastCDC), hasher (BLAKE3), compressor (zstd), pipeline
├── storage/           block_store (sharded blobs), index_db (redb), manifest, retrieval
├── cache/             chunk_cache (L1/L2 moka), manifest_cache (L3), http_cache (ETag)
├── security/          auth (JWT/argon2), encryption (AES-256-GCM), audit, tenants
├── replication/       client (frame codecs), replicator (WAL shipper)
└── grpc/              tonic server, interceptors, 4 services, health, reflection,
                        proto↔domain converters, Rust client SDK (retry + streaming)
proto/file_storage/v1/ common/file/auth/admin/replication.proto (+ buf.yaml)
tests/                 REST E2E scenarios + 18 gRPC E2E tests
benches/throughput.rs  criterion benchmarks
docs/                  mdBook documentation suite (55 pages → docs/book/)
```

## License

MIT — use at your own risk; the security placeholders in `config/production.yaml` **must** be replaced before any real deployment.
