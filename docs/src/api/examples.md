<!-- title: Examples category: api audience: developer last_updated: 2026-11-10 -->

# Examples

Copy-paste recipes for the full file lifecycle — login, upload, download (full and ranged), metadata, listing, delete and GC — in curl, Rust (REST and gRPC SDK), Go and Python. All snippets target a local dev server on `localhost:8080` (REST) and `localhost:9090` (gRPC) with the default test users (`alice/alice123`), which exist only in the shipped dev config.

## Overview

The same operations exist on both transports: REST is simplest for scripts and browsers, gRPC streams better for large payloads. Each snippet below is self-contained and stays within 20 lines; exact request/response schemas live in [REST](./rest.md) and [gRPC](./grpc.md). Errors follow the shared catalog in [Errors](./errors.md), so a 404 on REST is `NOT_FOUND` on gRPC, and tokens obtained via one transport work on the other.

## How it works

Auth once, then carry the token: REST wants `Authorization: Bearer <jwt>`; gRPC wants the `authorization: Bearer <jwt>` metadata (or `x-api-key` for the internal replication service). Uploads accept either `multipart/form-data` with a `file` field or a raw body with metadata in the query string; large payloads favor the raw-body form. Downloads honor `ETag`/`If-None-Match` (304) and `Range` on the `/range` route (inclusive end; multi-range takes the first range).

## Usage

curl recipes (base URL `http://localhost:8080`, `$FID` = an uploaded file id):

```bash
# Login
TOKEN=$(curl -s localhost:8080/api/v1/auth/login \
  -d '{"username":"alice","password":"alice123"}' | jq -r .access_token)

# Upload, small (multipart)
curl -s -H "Authorization: Bearer $TOKEN" \
  -F file=@notes.txt -F name=notes.txt -F tags=docs,draft \
  localhost:8080/api/v1/files

# Upload, large (raw body; stream-friendly)
curl -s -H "Authorization: Bearer $TOKEN" -H 'Content-Type: application/octet-stream' \
  --data-binary @big.iso 'localhost:8080/api/v1/files?name=big.iso&tags=iso'
```

```bash
# Download full / range
curl -s -H "Authorization: Bearer $TOKEN" localhost:8080/api/v1/files/$FID -o out.bin
curl -s -H "Authorization: Bearer $TOKEN" -H 'Range: bytes=0-1023' \
  localhost:8080/api/v1/files/$FID/range -o head.bin

# Metadata (with chunk table) and listing
curl -s -H "Authorization: Bearer $TOKEN" 'localhost:8080/api/v1/files/$FID/meta?chunks=true'
curl -s -H "Authorization: Bearer $TOKEN" 'localhost:8080/api/v1/files?tag=docs&limit=10'
```

```bash
# Soft delete (user), hard purge (admin), GC trigger (admin)
curl -s -X DELETE -H "Authorization: Bearer $TOKEN" localhost:8080/api/v1/files/$FID
curl -s -X DELETE -H "Authorization: Bearer $ADMIN" 'localhost:8080/api/v1/files/$FID?purge=true'
curl -s -X POST -H "Authorization: Bearer $ADMIN" localhost:8080/api/v1/admin/gc
```

## Examples

### Rust (REST via reqwest)

```rust
use reqwest::multipart;

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    let c = reqwest::Client::new();
    let pair: serde_json::Value = c.post("http://localhost:8080/api/v1/auth/login")
        .json(&serde_json::json!({"username":"alice","password":"alice123"}))
        .send().await?.json().await?;
    let token = pair["access_token"].as_str().unwrap().to_string();

    let form = multipart::Form::new()
        .text("name", "notes.txt")
        .part("file", multipart::Part::reader(std::fs::File::open("notes.txt")?));
    let up: serde_json::Value = c.post("http://localhost:8080/api/v1/files")
        .bearer_auth(&token).multipart(form)
        .send().await?.json().await?;
    println!("uploaded {}", up["file_id"]);
    Ok(())
}
```

### Rust (gRPC via the bundled SDK)

`FileStorageClient` handles token storage, 1 MiB upload pieces, gzip and unary retries; the server does the content-defined chunking.

```rust
use file_storage::grpc::FileStorageClient;

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    let mut c = FileStorageClient::connect("http://localhost:9090").await?;
    c.login("alice", "alice123").await?;

    let header = file_storage::grpc::UploadHeader {
        name: "notes.txt".into(), mime_type: "text/plain".into(), ..Default::default()
    };
    let up = c.upload(header, tokio::fs::File::open("notes.txt").await?).await?;
    let id = up.manifest.as_ref().unwrap().file_id.clone();

    let mut out = tokio::fs::File::create("copy.txt").await?;
    let hdr = c.download(&id, &mut out).await?; // verifies nothing lost: hdr.original_hash
    let mut sink = tokio::fs::File::create("head.bin").await?;
    c.download_range(&id, Some(0), Some(1024), &mut sink).await?;
    Ok(())
}
```

### Go (grpc-go, generated types)

Generate types from `proto/file_storage/v1` (go_package `github.com/your-org/file-storage/api/v1;v1`), then attach the bearer metadata manually — grpc-go has no built-in token layer here.

```go
package main

import (
    "context"

    "google.golang.org/grpc"
    "google.golang.org/grpc/credentials/insecure"
    "google.golang.org/grpc/metadata"
    v1 "github.com/your-org/file-storage/api/v1"
)

func main() {
    conn, _ := grpc.NewClient("localhost:9090", // gRPC listener port
        grpc.WithTransportCredentials(insecure.NewCredentials()))
    defer conn.Close()

    login, _ := v1.NewAuthServiceClient(conn).Login(context.Background(),
        &v1.LoginRequest{Username: "alice", Password: "alice123"})
    ctx := metadata.AppendToOutgoingContext(context.Background(),
        "authorization", "Bearer "+login.AccessToken) // manual bearer metadata

    files := v1.NewFileServiceClient(conn)
    man, _ := files.GetManifest(ctx, &v1.GetManifestRequest{FileId: "<file-id>"})
    _, _ = files.DeleteFile(ctx, &v1.DeleteFileRequest{FileId: man.FileId})
}
```

### Python (grpcio)

```python
import grpc
import file_storage.v1.file_pb2 as fp
import file_storage.v1.file_pb2_grpc as fp_grpc
import file_storage.v1.auth_pb2 as fa
import file_storage.v1.auth_pb2_grpc as fa_grpc
import file_storage.v1.admin_pb2 as adm
import file_storage.v1.admin_pb2_grpc as adm_grpc

channel = grpc.insecure_channel("localhost:9090")
pair = fa_grpc.AuthServiceStub(channel).Login(
    fa.LoginRequest(username="alice", password="alice123"))
md = (("authorization", f"Bearer {pair.access_token}"),)

stub = fp_grpc.FileServiceStub(channel)
manifest = stub.GetManifest(
    fp.GetManifestRequest(file_id="<file-id>", include_chunks=True), metadata=md)
print(manifest.name, manifest.size)
```

Streaming upload from Python: build a generator that yields `UploadChunk(payload=header)` once, then `UploadChunk(payload=data=…)` pieces (~1 MiB) from the file, and call `stub.Upload(gen, metadata=md)` — the server chunks content itself. Admin operations follow the same pattern, e.g. GC with an admin login:

```python
adm_grpc.AdminServiceStub(channel).TriggerGC(
    adm.TriggerGCRequest(mode=adm.TriggerGCRequest.REFCOUNT),
    metadata=admin_md)  # dry_run=True raises UNIMPLEMENTED
```

## See also

- [REST](./rest.md) and [gRPC](./grpc.md) — exact request/response schemas
- [Authentication](./authentication.md) — token lifecycle details
- [Rust client](../integration/rust-client.md), [Go client](../integration/go-client.md), [Python client](../integration/python-client.md) — deeper SDK guides
- [CLI recipes](../integration/cli.md) — shell-first workflows
- [Quickstart](../getting-started/quickstart.md) — boot the server first
