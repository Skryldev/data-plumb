<!-- title: Quickstart category: getting-started audience: developer last_updated: 2026-11-10 -->

# Quickstart

This page takes you from zero to a working file-storage service in about five minutes: start the server, log in, upload a file, download it back, and confirm the gRPC listener is alive. Everything runs locally with the development configuration — no TLS, no encryption, and the seeded demo users.

## Overview

file-storage is a single Rust binary that exposes two listeners from one process: a REST API (axum) on port **8080** and a gRPC API (tonic) on port **9090**. Both transports share the same storage core, so a file uploaded over REST is byte-identical when downloaded over gRPC. The development configuration ships with three tenants and four users, including the administrator `admin` / `admin123`.

```text
  you ──► REST :8080 ─┐
                      ├─► chunk ─► hash ─► compress ─► block store + redb index
  you ──► gRPC :9090 ─┘
```

## How it works

You can start the service from source or from Docker Compose. Either way the server boots, seeds the tenants from configuration, opens the redb index at `data/index.redb`, and begins serving both listeners. Data lands under `data/blocks/` (sharded content-addressed blobs) unless overridden.

## Usage

### Option A — run from source

Requires Rust 1.80+ (see [Installation](./installation.md)):

```bash
git clone <your-repo-url> file-storage && cd file-storage
cargo run --release -- --config config/default.yaml
```

You should see the REST listener on `0.0.0.0:8080` and the gRPC listener on `0.0.0.0:9090`.

### Option B — run with Docker Compose

Starts a primary (8080/9090), a replica (host ports 8081/9091) and Prometheus (host port 9099, because 9090 on the host already belongs to the primary's gRPC listener):

```bash
docker compose up --build
```

For the full walkthrough see [Docker Compose](../deployment/docker-compose.md). The rest of this page assumes Option A (or the primary container).

### 1. Log in

```bash
TOKEN=$(curl -s -X POST localhost:8080/api/v1/auth/login \
  -H 'Content-Type: application/json' \
  -d '{"username":"admin","password":"admin123"}' | jq -r .access_token)
```

### 2. Upload a file

```bash
head -c 4M /dev/urandom > /tmp/demo.bin
curl -s -X POST "localhost:8080/api/v1/files?name=demo.bin&tags=demo,quickstart" \
  -H "Authorization: Bearer $TOKEN" \
  -H 'Content-Type: application/octet-stream' \
  --data-binary @/tmp/demo.bin | jq
```

Note the `content_hash` in the response — a 64-character hex BLAKE3 digest of your file.

### 3. Download it back and verify the hash

```bash
FILE_ID=<file_id from the upload response>

curl -s -D headers.txt -H "Authorization: Bearer $TOKEN" \
  localhost:8080/api/v1/files/$FILE_ID -o /tmp/demo.out

grep -i x-content-hash headers.txt
b3sum /tmp/demo.out   # must equal the content_hash above
```

The download carries an `X-Content-Hash` header with the same 64-hex digest that `content_hash` returned at upload. The digest is **BLAKE3**, not SHA-256: `sha256sum /tmp/demo.out` will *not* match. If you have the `b3sum` utility installed (`cargo install b3sum`), its output matches byte-for-byte.

### 4. List files

```bash
curl -s -H "Authorization: Bearer $TOKEN" \
  'localhost:8080/api/v1/files?limit=10' | jq
```

### 5. Verify the gRPC listener

gRPC serves reflection, so `grpcurl` can discover the API without any `.proto` files:

```bash
# list services
grpcurl -plaintext localhost:9090 list

# health check
grpcurl -plaintext \
  -d '{"service": "file_storage.v1.FileService"}' \
  localhost:9090 grpc.health.v1.Health/Check
```

Both should succeed; the health check returns `{"status": "SERVING"}`.

## Examples

A minimal end-to-end script combining the steps above:

```bash
set -e
TOKEN=$(curl -s -X POST localhost:8080/api/v1/auth/login \
  -H 'Content-Type: application/json' \
  -d '{"username":"admin","password":"admin123"}' | jq -r .access_token)
echo hello-quickstart > /tmp/hello.txt
RESP=$(curl -s -X POST "localhost:8080/api/v1/files?name=hello.txt" \
  -H "Authorization: Bearer $TOKEN" --data-binary @/tmp/hello.txt)
ID=$(echo "$RESP" | jq -r .file_id)
curl -s -H "Authorization: Bearer $TOKEN" localhost:8080/api/v1/files/$ID
```

## See also

- [Installation](./installation.md) — building from source, systemd, release profile.
- [Docker](./docker.md) — image layout, running a single container.
- [Your First File](./first-file.md) — the full upload/download/delete walkthrough, including multipart uploads, ranges, ETags and dedup.
- [REST API](../api/rest.md) and [gRPC API](../api/grpc.md) — complete endpoint reference.
- [Configuration reference](../configuration/reference.md) — every key and default.
- [Authentication](../security/authentication.md) — tokens, roles and refresh rotation.
