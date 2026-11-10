<!-- title: Rust Client category: integration audience: developer last_updated: 2026-11-10 -->

# Rust Client

The official SDK ships with the service crate: `file_storage::grpc::client::FileStorageClient` wraps the generated tonic clients with streaming upload from any `AsyncRead`, streaming download into any `AsyncWrite`, token management and a retry policy for unary calls. It is the same API exercised by the project's 18 gRPC end-to-end tests, so every snippet below is tested behaviour. Protocol details are in [gRPC API](../api/grpc.md) and the design rationale in [SDK Design](./sdk-design.md).

## Overview

One struct covers files, auth and admin; a separate `ReplicationClient` exposes the internal replication protocol with `x-api-key` authentication. The SDK negotiates gzip in both directions, raises message limits to the server's 256 MiB ceiling, and attaches `authorization: Bearer <jwt>` metadata to every call — including streaming RPCs, where interceptors do not run.

| Concern | SDK answer |
|---|---|
| Transport | `connect` (plaintext) / `connect_tls` (TLS with optional CA pinning) |
| Auth | `login` / `refresh` / `logout`, token stored client-side |
| Upload | `upload(header, impl AsyncRead)` — 1 MiB pieces, `expected_size` guard |
| Download | `download` / `download_range` into `impl AsyncWrite`, returns `DownloadHeader` |
| Retry | `RetryPolicy` — 3 attempts, 100 ms → 2 s capped backoff, transport codes only |

## How it works

`upload` builds a 32-slot bounded channel, sends the `UploadHeader` first, then spawns a reader task that copies 1 MiB pieces from your `AsyncRead`. The sender is moved **into** that task, so when the reader hits EOF the stream closes and the server observes end-of-stream — holding the sender open anywhere else would hang the server. `download` iterates the server stream, writes `data` frames to your writer and returns the header that arrived first; `DownloadHeader.original_hash` is the raw 32-byte BLAKE3 of the full plaintext, which you should verify locally.

Retries wrap unary calls only (manifests, listings, deletes, GC, tenants, audit). The default policy retries `Unavailable`, `DeadlineExceeded` and `Aborted` — business rejections such as `NotFound` or `PermissionDenied` surface immediately because retrying them cannot succeed.

## Configuration

Add the dependency (the crate is published from the service repository; adjust to your registry):

```toml
[dependencies]
file-storage = { path = "../file-storage" }
# or: file-storage = { git = "https://github.com/your-org/file-storage", branch = "main" }
tokio  = { version = "1", features = ["macros", "rt-multi-thread", "io-util"] }
blake3 = "1"
```

Generated protocol types live at `file_storage::grpc::generated::pb` (aliased `pb` below); the raw generated tonic clients (`FileServiceClient`, `AuthServiceClient`, `AdminServiceClient`) are exported from the same module when you need streaming control the facade does not expose. `RetryPolicy::new(max_attempts, initial_backoff, max_backoff)` replaces the default via `with_retry_policy`.

## Examples

Complete working program — connect, login, upload, download, verify the hash, delete:

```rust
use file_storage::grpc::client::FileStorageClient;
use file_storage::grpc::generated::pb;

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    let mut client = FileStorageClient::connect("http://127.0.0.1:9090").await?;
    client.login("alice", "alice123").await?;

    let payload = b"hello file-storage".to_vec();
    let header = pb::UploadHeader {
        name: "hello.txt".into(),
        mime_type: "text/plain".into(),
        expected_size: payload.len() as u64, // oversize streams fail fast
        tenant_id: String::new(),            // own tenant; admin may override
        tags: vec!["demo".into()],
    };
    let resp = client
        .upload(header, std::io::Cursor::new(payload.clone()))
        .await?;
    let file_id = resp.manifest.as_ref().unwrap().file_id.clone();
    println!("uploaded {file_id} dedup_chunks={} stored={}", resp.dedup_chunks, resp.stored_bytes);

    let mut downloaded = Vec::new();
    let hdr = client.download(&file_id, &mut downloaded).await?;
    let mut hasher = blake3::Hasher::new();
    hasher.update(&downloaded);
    assert_eq!(hdr.original_hash, hasher.finalize().as_bytes().to_vec());
    assert_eq!(downloaded, payload);

    client.delete_file(&file_id, false).await?; // soft delete
    Ok(())
}
```

Range download and verification of a slice:

```rust
let mut sink = tokio::fs::File::create("part.bin").await?;
// half-open range [1024, 2048): matches gRPC Download{start,end}
let hdr = client.download_range(&file_id, Some(1024), Some(2048), &mut sink).await?;
assert_eq!(hdr.size, 1024);
```

TLS with a pinned private CA (endpoint must use a SAN listed in the certificate):

```rust
let ca = tokio::fs::read("certs/ca.pem").await?;
let mut client = FileStorageClient::connect_tls("https://fs.example.com:9090", Some(&ca)).await?;
```

Error handling: every method returns `tonic::Status`; map `status.code()` against the catalog in [Errors](../api/errors.md) (`NotFound` → 404 `not_found`, `ResourceExhausted` → 413 `payload_too_large`, and so on). `Status` implements `Display` and carries `details()`.

## Usage notes

- `login()` stores the access token for all subsequent calls; `refresh(refresh_token)` rotates the pair (the old refresh token is single-use). Tokens are in-memory — persist the refresh token if your process restarts.
- `trigger_gc(mode, dry_run)` mirrors the admin API: `dry_run=true` returns `UNIMPLEMENTED`; use `false`.
- `list_files(limit, offset, tag, name, include_deleted)` — server clamps `limit` to 1..1000; `include_deleted` is admin-only.
- Streaming uploads are not retried by the policy; wrap them yourself if your storage layer tolerates re-sends (same-name upload replaces atomically).

## See also

- [gRPC API](../api/grpc.md) — messages, streaming semantics, metadata.
- [SDK Design](./sdk-design.md) — why the streaming protocol looks like this.
- [Errors](../api/errors.md) — full gRPC/REST error mapping.
- [TLS](../deployment/tls.md) — server-side certificates and mTLS.
- [Authentication](../api/authentication.md) — token lifetimes and roles.
- [CLI Recipes](./cli.md) — shell equivalents for quick checks.
