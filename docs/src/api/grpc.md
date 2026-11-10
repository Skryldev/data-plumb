<!-- title: gRPC API category: api audience: developer last_updated: 2026-11-10 -->

# gRPC API

The gRPC API is served by tonic on a second listener, `server.grpc_port` (default `9090`), sharing the same state as the REST API. All services live in the protobuf package `file_storage.v1` (protos under `proto/file_storage/v1/`), so you can generate clients in any language or use the bundled Rust SDK. This page catalogs every service and RPC, streaming semantics, and how errors map onto gRPC status codes.

## Overview

| Service | Purpose | Auth |
|---|---|---|
| `file_storage.v1.FileService` | Upload, Download, GetManifest, ListFiles, DeleteFile | Bearer JWT |
| `file_storage.v1.AuthService` | Login, Refresh, Logout | public |
| `file_storage.v1.AdminService` | Tenants, TriggerGC, Resync, QueryAudit | admin JWT |
| `file_storage.v1.ReplicationService` | Internal peer protocol (`ReplicateBatch`, `PullChunk`, `PullManifest`) | `x-api-key` metadata |
| `grpc.health.v1.Health` | Check/Watch for the four services + overall `""` | public |

`grpc.reflection.v1.ServerReflection` is also enabled (project protos + health descriptors), which is why plain `grpcurl` works against the server. The full RPC surface is summarized below; the messages are defined in the repository's `proto/file_storage/v1/*.proto` — hashes (`hash`, `plain_hash`, `original_hash`) are raw 32-byte BLAKE3.

## How it works

Metadata conventions: send `authorization: Bearer <jwt>` for user RPCs, `x-api-key` for `ReplicationService`, and optionally `x-request-id` (propagated when present, otherwise a UUIDv7 is minted). Two honest caveats about the implementation:

- **tonic interceptors run for unary RPCs only.** Unary calls get JWT verification, the admin gate, the per-tenant rate limit and request-id propagation via interceptors; streaming handlers (Upload, Download, ReplicateBatch, PullChunk) verify the token / api-key **in-handler**. Semantics are identical.
- **Rate limiting:** unary RPCs pass a per-tenant token bucket (`rate_limit.per_second`/`burst`; disabled → passthrough) and fail with `RESOURCE_EXHAUSTED`. **Streaming RPCs are not rate limited** — their protection is `limits.max_file_size` and the `expected_size` guard.

Server tuning: max message 256 MiB, HTTP/2 keepalive 30 s / timeout 20 s, 64 concurrent streams per connection, stream window 4 MiB / connection window 8 MiB, TCP keepalive 30 s, gzip compression supported both directions. TLS/mTLS for the listener is configured under `server.grpc_tls` (see [TLS](../deployment/tls.md)). Panics in handlers are recovered and surface as gRPC status 13 (`INTERNAL`).

## Usage

Every RPC maps internal `StorageError` variants onto gRPC codes per the [Errors](./errors.md) table — e.g. a missing file is `NOT_FOUND`, a viewer attempting a write is `PERMISSION_DENIED`, an oversized stream is `RESOURCE_EXHAUSTED`.

### FileService

| RPC | Type | Request | Response | Notable statuses |
|---|---|---|---|---|
| `Upload` | client-streaming | `UploadChunk` stream (first: `header{name,mime_type,expected_size,tenant_id,tags}`, then `data`) | `UploadResponse{manifest,deduped_bytes,stored_bytes,dedup_chunks,compression_ratio}` | `INVALID_ARGUMENT` (empty/bad name, header not first), `RESOURCE_EXHAUSTED` (size cap), `UNAUTHENTICATED`, `PERMISSION_DENIED` |
| `Download` | server-streaming | `DownloadRequest{file_id,start?,end?}` — **half-open exclusive end**, clamped to size | `DownloadChunk` stream (first: `header{file_id,size,total_size,mime_type,original_hash,start,end}`, then `data`) | `NOT_FOUND`, `OUT_OF_RANGE` (start ≥ size), `DATA_LOSS` (hash mismatch) |
| `GetManifest` | unary | `GetManifestRequest{file_id,include_chunks}` | `Manifest` | `NOT_FOUND`, `INVALID_ARGUMENT` (bad UUID) |
| `ListFiles` | unary | `ListFilesRequest{limit,offset,tag,name,include_deleted}` — limit clamped 1..1000 default 50; tag exact / name substring; **`include_deleted` silently ignored for non-admins** | `ListFilesResponse{files, total}` | `UNAUTHENTICATED` |
| `DeleteFile` | unary | `DeleteFileRequest{file_id,purge}` | `DeleteFileResponse{deleted_at,purged}` | `NOT_FOUND`, `PERMISSION_DENIED` (purge or write without role) |

### AuthService

| RPC | Type | Request | Response | Statuses |
|---|---|---|---|---|
| `Login` | unary | `LoginRequest{username,password}` | `LoginResponse{access_token,refresh_token,token_type,expires_in,expires_at}` | `UNAUTHENTICATED` (bad credentials), `INVALID_ARGUMENT` |
| `Refresh` | unary | `RefreshRequest{refresh_token}` | `LoginResponse` (old token revoked) | `UNAUTHENTICATED` (unknown/expired) |
| `Logout` | unary | `LogoutRequest{token}` | `Empty` — idempotent | — |

### AdminService (all admin-only)

| RPC | Type | Request → Response | Statuses |
|---|---|---|---|
| `CreateTenant` | unary | `{name}` (1..=128 chars) → `Tenant` | `INVALID_ARGUMENT`, `PERMISSION_DENIED` |
| `GetTenant` | unary | `{id}` → `Tenant` (`quota_bytes` reserved, always 0) | `NOT_FOUND`, `INVALID_ARGUMENT` |
| `ListTenants` | unary | `{limit}` → `{tenants,total}` | `PERMISSION_DENIED` |
| `TriggerGC` | unary | `{mode: MODE_UNSPECIFIED\|REFCOUNT\|REACHABILITY, dry_run}` → `GCResult` | `dry_run=true` → **`UNIMPLEMENTED`** ("gc dry_run is not supported yet; run without dry_run"); `PERMISSION_DENIED` |
| `Resync` | unary | `{file_id}` → `{file_id,replicated}` | `NOT_FOUND`, `ALREADY_EXISTS` (replication disabled), `UNAVAILABLE` |
| `QueryAudit` | unary | `{limit (1..=1000, default 100), tenant_id}` → `{entries,total}` | `PERMISSION_DENIED`, `INVALID_ARGUMENT` |

### ReplicationService (`x-api-key`)

| RPC | Type | Request → Response | Statuses |
|---|---|---|---|
| `ReplicateBatch` | bidi | stream of `ReplicationOp{put_chunk\|put_manifest\|delete_file\|flush}` → stream of `ReplicationAck{ops_acked,success,error}` | `UNAUTHENTICATED` (bad key), `INTERNAL` |
| `PullChunk` | server-streaming | `{hash (32 B)}` → `PullChunkResponse{header\|data}` — ≤ 64 KiB pieces, raw stored blob | `NOT_FOUND`, `UNAUTHENTICATED` |
| `PullManifest` | unary | `{file_id}` → `ManifestBundle{manifest}` | `NOT_FOUND`, `UNAUTHENTICATED` |

All ops are idempotent, so replaying a batch after a crash is safe. Health probes run in the background every 10 s (redb ping + storage-root writability) and flip `NOT_SERVING` within ≤ 15 s of failure.

### Streaming semantics

- **Header-first framing.** Upload starts with exactly one `UploadHeader`; Download starts with exactly one `DownloadHeader`. Everything after is `data`.
- **The client does NOT chunk content.** Send raw bytes in ~1 MiB pieces (the Rust SDK does this); the server performs FastCDC chunking, hashing, compression and encryption internally.
- **`expected_size` guard.** When set and the stream exceeds it, the upload is rejected with `RESOURCE_EXHAUSTED` and the server stops buffering.
- **Backpressure** is HTTP/2 flow control end-to-end. The download path drains retrieval through a bounded mpsc channel (depth 32 × 256 KiB pieces); the upload SDK bridges a bounded 32-message channel. Honor gRPC READY signals; never buffer unbounded.
- **Abort semantics:** canceling the stream (dropping the request/response future) tears down the pump; server-side tasks observe a send failure and stop, discarding partial state. Aborted uploads store nothing.
- REST vs gRPC ranges differ: REST `Range` is inclusive-end (`bytes=10-19`), gRPC `end` is exclusive (slice semantics).

### REST or gRPC?

| Operation | Prefer REST when | Prefer gRPC when |
|---|---|---|
| Login/refresh | scripts, curl | SDK-managed token lifecycle |
| Upload | one-shot tools, browsers (multipart) | large files, backpressure-sensitive pipelines |
| Download | HTTP caching (ETag/304), CDN-style clients | streaming into disk/network with flow control, ranged reads without header parsing |
| Metadata/list/delete | admin scripts | low-latency typed access from Go/Rust/Python services |
| GC/resync/audit | humans + curl | automation in services |

## Examples

Rust SDK (`file_storage::grpc::FileStorageClient` — 1 MiB upload pieces, retry policy 3 attempts 100 ms→2 s on Unavailable/DeadlineExceeded/Aborted):

```rust
use file_storage::grpc::FileStorageClient;

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    let mut c = FileStorageClient::connect("http://127.0.0.1:9090").await?;
    c.login("alice", "alice123").await?; // token stored on the client

    let header = file_storage::grpc::UploadHeader {
        name: "notes.txt".into(),
        mime_type: "text/plain".into(),
        tags: vec!["notes".into()],
        ..Default::default()
    };
    let reader = tokio::fs::File::open("notes.txt").await?;
    let resp = c.upload(header, reader).await?;
    println!("stored {} B, dedup {} B", resp.stored_bytes, resp.deduped_bytes);
    Ok(())
}
```

grpcurl (reflection is enabled):

```bash
grpcurl -plaintext -d '{"username":"alice","password":"alice123"}' \
  127.0.0.1:9090 file_storage.v1.AuthService/Login

grpcurl -plaintext -H "authorization: Bearer $TOKEN" \
  -d '{"file_id":"01930000-0000-7000-8000-0000000000aa"}' \
  127.0.0.1:9090 file_storage.v1.FileService/GetManifest
```

## See also

- [REST](./rest.md) — HTTP equivalent of every operation
- [Errors](./errors.md) — HTTP ↔ gRPC status mapping
- [Authentication](./authentication.md) — JWTs on the wire
- [Rust client](../integration/rust-client.md), [CLI recipes](../integration/cli.md)
- [TLS](../deployment/tls.md), [Monitoring](../operations/monitoring.md)
- [ADR 0008 — gRPC alongside REST](../architecture/decisions/0008-grpc-alongside-rest.md)
