<!-- title: Your First File category: getting-started audience: developer last_updated: 2026-11-10 -->

# Your First File

This page walks a single file through its entire lifecycle — upload, inspect, read back, cache-verify, partially read, soft-delete, purge — and then demonstrates the same operations over gRPC. All commands assume the service from [Quickstart](./quickstart.md) is running with the default development configuration.

## Overview

Every file becomes a manifest in the index plus one or more content-addressed chunks in the block store. The upload response already tells you how the content was cut into chunks and whether any of them were duplicates. Reads are served through a three-level cache and verified against the BLAKE3 plaintext hash after every disk load, so the bytes you get back are provably the bytes you put in.

## How it works

The lifecycle below maps one-to-one onto REST endpoints; each step shows the exact command and response. The gRPC equivalents operate on the same storage core — a file uploaded over REST is visible over gRPC and vice versa.

```text
 login ─► upload ─► 201 manifest ─► meta ─► download ─► 304/206 ─► delete ─► 404 ─► purge
                                                              │
              dedup: same bytes re-uploaded ──► dedup_chunks = chunk_count
```

## Usage

### 1. Log in

```bash
TOKEN=$(curl -s -X POST localhost:8080/api/v1/auth/login \
  -H 'Content-Type: application/json' \
  -d '{"username":"admin","password":"admin123"}' | jq -r .access_token)
```

### 2. Upload — multipart variant

```bash
echo "quarterly report" > /tmp/report.txt
curl -s -X POST localhost:8080/api/v1/files \
  -H "Authorization: Bearer $TOKEN" \
  -F name=report.txt \
  -F tags=q1,finance \
  -F file=@/tmp/report.txt | jq
```

### 3. Upload — raw body variant

The raw variant streams the body directly (no multipart parsing) and takes metadata as query parameters:

```bash
curl -s -X POST "localhost:8080/api/v1/files?name=report-raw.txt&mime=text/plain&tags=q1" \
  -H "Authorization: Bearer $TOKEN" \
  -H 'Content-Type: application/octet-stream' \
  --data-binary @/tmp/report.txt | jq
```

### 4. The upload response, field by field

```json
{
  "file_id": "01930000-0000-7000-8000-0000000000ab",
  "name": "report.txt",
  "mime": "text/plain",
  "size": 17,
  "size_stored": 17,
  "chunk_count": 1,
  "content_hash": "5f2c9c4b8d3e1a7f6b0c4d8e2f9a1b3c5d7e9f0a2b4c6d8e0f2a4b6c8d0e2f4a",
  "dedup_chunks": 0,
  "dedup_bytes": 0,
  "created_at_ms": 1762700000000
}
```

| Field | Meaning |
|---|---|
| `file_id` | UUID of the manifest; use it in every subsequent request |
| `name` | Logical name you supplied; uploading the same name again replaces the old file (soft delete of the previous manifest) |
| `mime` | MIME type (from the field, or inferred/default) |
| `size` | Original byte count |
| `size_stored` | Bytes actually on disk after compression; equal to `size` here because zstd did not shrink this tiny payload (it is stored verbatim). With encryption enabled each blob additionally carries 28 bytes of nonce + tag |
| `chunk_count` | Number of content-defined chunks (FastCDC, avg 32 KiB) |
| `content_hash` | 64-hex BLAKE3 digest of the original bytes |
| `dedup_chunks` | Chunks that were already stored and re-used this upload |
| `dedup_bytes` | Bytes not written because they were duplicates |
| `created_at_ms` | Epoch milliseconds |

### 5. Inspect metadata with chunks

```bash
curl -s -H "Authorization: Bearer $TOKEN" \
  "localhost:8080/api/v1/files/$ID/meta?chunks=true" | jq
```

With `?chunks=true` the response includes a `chunks` array where each entry reports `offset`, `size_original`, `size_stored`, `hash`, `compressed` and `encrypted`. The `hash` values are the BLAKE3 plaintext digests of each chunk; `compressed=false` appears when a chunk was stored verbatim because zstd did not shrink it.

### 6. Download and verify

```bash
curl -s -D headers.txt -H "Authorization: Bearer $TOKEN" \
  localhost:8080/api/v1/files/$ID -o /tmp/report.out
grep -iE 'etag|x-content-hash' headers.txt
b3sum /tmp/report.out
```

The response carries `ETag: "<content_hash>"`, `Cache-Control: private, max-age=3600`, `Content-Disposition: inline; filename="report.txt"` and `X-Content-Hash: <content_hash>`. The hash is **BLAKE3** — `sha256sum` will not match; use `b3sum` (installable with `cargo install b3sum`) or simply compare against the `content_hash` returned at upload.

### 7. Conditional download (304)

```bash
ETAG=$(grep -i '^etag:' headers.txt | cut -d' ' -f2 | tr -d '\r')
curl -s -D- -o /dev/null -H "Authorization: Bearer $TOKEN" \
  -H "If-None-Match: $ETAG" \
  localhost:8080/api/v1/files/$ID
```

Because the ETag still matches, the server returns **304 Not Modified** with `ETag` and `Cache-Control: private, max-age=3600` and no body.

### 8. Partial download (206)

```bash
curl -s -D- -o /dev/null -H "Authorization: Bearer $TOKEN" \
  -H 'Range: bytes=0-7' \
  localhost:8080/api/v1/files/$ID/range
```

This returns **206 Partial Content** with `Content-Range: bytes 0-7/17` and the first eight bytes. The end offset is inclusive; a suffix form (`Range: bytes=-4`) and multi-range requests (first range wins) are also supported. An unsatisfiable range yields **416** with `Content-Range: bytes */17`.

### 9. Soft delete

```bash
curl -s -X DELETE -H "Authorization: Bearer $TOKEN" localhost:8080/api/v1/files/$ID -o /dev/null -w '%{http_code}\n'
# 204
curl -s -o /dev/null -w '%{http_code}\n' -H "Authorization: Bearer $TOKEN" \
  localhost:8080/api/v1/files/$ID
# 404
```

A soft delete only sets a tombstone (`deleted_at_ms`); the chunks remain on disk until garbage collection removes them after the grace period (86400 s by default).

### 10. Purge (admin)

```bash
curl -s -X DELETE -H "Authorization: Bearer $TOKEN" \
  "localhost:8080/api/v1/files/$ID?purge=true" -o /dev/null -w '%{http_code}\n'
# 204 (admin only)
```

Purge hard-deletes the manifest immediately; blob storage is reclaimed by the next GC pass.

### 11. Observe deduplication

Upload the same file twice and compare responses:

```bash
curl -s -X POST "localhost:8080/api/v1/files?name=dedup-a.txt" \
  -H "Authorization: Bearer $TOKEN" --data-binary @/tmp/report.txt | jq '.chunk_count, .dedup_chunks'
curl -s -X POST "localhost:8080/api/v1/files?name=dedup-b.txt" \
  -H "Authorization: Bearer $TOKEN" --data-binary @/tmp/report.txt | jq '.chunk_count, .dedup_chunks, .dedup_bytes'
```

The second upload reports `dedup_chunks == chunk_count` and `dedup_bytes == size`: every chunk was already stored, so no new bytes were written — both files now reference the same physical blobs.

## Examples

### The same lifecycle over gRPC

Discover and log in with `grpcurl` (reflection means no local proto files are needed):

```bash
grpcurl -plaintext localhost:9090 list
grpcurl -plaintext -d '{"username":"admin","password":"admin123"}' \
  localhost:9090 file_storage.v1.AuthService/Login

grpcurl -plaintext -H "authorization: Bearer $JWT" \
  -d '{"limit": 10}' localhost:9090 file_storage.v1.FileService/ListFiles

grpcurl -plaintext -H "authorization: Bearer $JWT" \
  -d '{"file_id": "'$ID'", "include_chunks": true}' \
  localhost:9090 file_storage.v1.FileService/GetManifest

grpcurl -plaintext -H "authorization: Bearer $JWT" \
  -d '{"file_id": "'$ID'", "purge": false}' \
  localhost:9090 file_storage.v1.FileService/DeleteFile
```

Streaming upload/download is easiest through the Rust SDK:

```rust
use file_storage::grpc::FileStorageClient;
use file_storage::grpc::generated::pb;

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    let mut client = FileStorageClient::connect("http://127.0.0.1:9090").await?;
    client.login("admin", "admin123").await?;          // token stored on the client

    let payload = std::fs::read("/tmp/report.txt")?;
    let header = pb::UploadHeader {
        name: "report.txt".into(),
        mime_type: "text/plain".into(),
        tags: vec!["q1".into()],
        ..Default::default()
    };
    let resp = client.upload(header, std::io::Cursor::new(payload)).await?;
    let file_id = resp.manifest.unwrap().file_id;

    let mut sink = Vec::new();
    let header = client.download(&file_id, &mut sink).await?;   // full read-back
    println!("{} bytes, hash {:?}", sink.len(), header.original_hash);
    Ok(())
}
```

The client streams 1 MiB pieces, retries transient failures (3 attempts, exponential backoff), and attaches the JWT as `authorization` metadata automatically after `login`.

## See also

- [REST API](../api/rest.md) — every endpoint, parameter and status code.
- [gRPC API](../api/grpc.md) — services, streaming shapes and metadata.
- [Authentication](../security/authentication.md) — token lifecycle behind the login step.
- [Encryption at Rest](../security/encryption.md) — what the `encrypted` chunk flag means.
- [Configuration reference](../configuration/reference.md) — chunking, caching and limits used above.
