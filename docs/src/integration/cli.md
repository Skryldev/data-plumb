<!-- title: CLI Recipes category: integration audience: all last_updated: 2026-11-10 -->

# CLI Recipes

Copy-paste recipes for driving both transports from the shell: `curl` against the REST API on port 8080 and `grpcurl` against gRPC on 9090. These are the fastest tools for smoke tests, CI probes and the first minute of an incident; each recipe maps one-to-one onto an endpoint documented in [REST API](../api/rest.md) or [gRPC API](../api/grpc.md). Authentication details are in [Authentication](../api/authentication.md).

## Overview

Two conventions run through this page: capture the JWT once into `$TOKEN` (or `$ADMIN` for admin calls), and pipe everything through `jq` for readable output. REST errors are JSON `{"error":{"code","message"}}`; gRPC errors surface as `Error: ... code = NotFound desc = ...` in grpcurl output.

## How it works

### Login and basic REST calls

```bash
BASE=http://localhost:8080/api/v1
TOKEN=$(curl -s -X POST $BASE/auth/login -H 'Content-Type: application/json' \
  -d '{"username":"alice","password":"alice123"}' | jq -r .access_token)
```

Upload — raw streamed body and multipart:

```bash
head -c 64M /dev/urandom > /tmp/f.bin

# raw body; name/mime/tags as query parameters
curl -s -X POST "$BASE/files?name=f.bin&mime=application/octet-stream&tags=demo,raw" \
  -H "Authorization: Bearer $TOKEN" -H 'Content-Type: application/octet-stream' \
  --data-binary @/tmp/f.bin | jq

# multipart; the file field is named `file`
curl -s -X POST $BASE/files -H "Authorization: Bearer $TOKEN" \
  -F name=report.pdf -F tags=q1,finance -F file=@report.pdf | jq
```

Both return 201 with `{file_id, name, size, size_stored, chunk_count, content_hash, dedup_chunks, dedup_bytes, ...}` — capture the id:

```bash
FILE_ID=$(curl -s -X POST ... -F file=@f.bin | jq -r .file_id)
```

Download, range and conditional requests:

```bash
curl -s -H "Authorization: Bearer $TOKEN" $BASE/files/$FILE_ID -o out.bin        # full
curl -s -H "Authorization: Bearer $TOKEN" -H 'Range: bytes=0-1023' \
  $BASE/files/$FILE_ID/range -o part.bin                                          # 206
curl -s -D- -o /dev/null -H "Authorization: Bearer $TOKEN" \
  -H "If-None-Match: \"<etag-from-headers>\"" $BASE/files/$FILE_ID                # 304 (quotes required)
```

Metadata, list, delete, purge, GC and audit:

```bash
curl -s -H "Authorization: Bearer $TOKEN" "$BASE/files/$FILE_ID/meta?chunks=true" | jq '.chunks[0]'
curl -s -H "Authorization: Bearer $TOKEN" "$BASE/files?tag=q1&limit=20" | jq '.total, .items[].name'
curl -s -X DELETE -H "Authorization: Bearer $TOKEN" $BASE/files/$FILE_ID                     # soft, 204
curl -s -X DELETE -H "Authorization: Bearer $ADMIN" "$BASE/files/$FILE_ID?purge=true"        # hard, 204
curl -s -X POST -H "Authorization: Bearer $ADMIN" $BASE/admin/gc | jq
curl -s -H "Authorization: Bearer $ADMIN" "$BASE/audit?limit=20" | jq '.[] | {action, actor, object, status}'
```

### grpcurl recipes

Discovery, health and auth (no proto files needed — reflection is registered on the server):

```bash
grpcurl -plaintext localhost:9090 list                        # 4 services + health
grpcurl -plaintext localhost:9090 describe file_storage.v1.FileService
grpcurl -plaintext -d '{"service": ""}' localhost:9090 grpc.health.v1.Health/Check
grpcurl -plaintext -d '{"username":"admin","password":"admin123"}' \
  localhost:9090 file_storage.v1.AuthService/Login
```

Authenticated unary calls — pass the JWT as `authorization` metadata:

```bash
TOKEN=$(grpcurl -plaintext -d '{"username":"alice","password":"alice123"}' \
  localhost:9090 file_storage.v1.AuthService/Login | jq -r .access_token)

grpcurl -plaintext -H "authorization: Bearer $TOKEN" \
  -d '{"limit": 10, "offset": 0}' localhost:9090 file_storage.v1.FileService/ListFiles

grpcurl -plaintext -H "authorization: Bearer $TOKEN" \
  -d '{"mode": 1}' localhost:9090 file_storage.v1.AdminService/TriggerGC    # 1 = REFCOUNT, 2 = REACHABILITY
```

Streaming RPCs need the proto files plus `-d @` to read newline-delimited JSON messages from stdin:

```bash
# Upload (client-streaming): first message = header, then data messages.
# Large files: feed base64 "data" lines from a generator, or use the SDKs —
# grpcurl buffers stdin, so it is a smoke tool, not a bulk uploader.
grpcurl -plaintext -H "authorization: Bearer $TOKEN" \
  -import-path proto -proto proto/file_storage/v1/file.proto \
  -d @ localhost:9090 file_storage.v1.FileService/Upload <<'EOF'
{"header": {"name": "cli.txt", "mime_type": "text/plain"}}
{"data": "aGVsbG8gZmlsZS1zdG9yYWdl"}
EOF

# Download (server-streaming) and AdminService/Resync
grpcurl -plaintext -H "authorization: Bearer $TOKEN" \
  -d '{"file_id": "<id>"}' localhost:9090 file_storage.v1.FileService/Download
grpcurl -plaintext -H "authorization: Bearer $ADMIN" \
  -d '{"file_id": "<id>"}' localhost:9090 file_storage.v1.AdminService/Resync
```

Internal replication calls authenticate with `x-api-key` metadata instead of a JWT:

```bash
grpcurl -plaintext -H "x-api-key: cluster-api-key-change-me" \
  -d '{"file_id": "<id>"}' localhost:9090 file_storage.v1.ReplicationService/PullManifest
```

### jq one-liners

```bash
curl -s "$BASE/files?limit=1000" -H "Authorization: Bearer $TOKEN" \
  | jq -r '.items[] | [.file_id, .name, (.size|tostring)] | @tsv'        # TSV inventory
curl -s $BASE/files/$FILE_ID/meta?chunks=true -H "Authorization: Bearer $TOKEN" \
  | jq '[.chunks[] | .size_stored] | add'                                  # stored bytes
curl -s localhost:8080/metrics | grep file_storage_replication_queue_depth # lag gauge
```

## Configuration

### Compose grpcurl profile

`docker-compose.yml` ships a one-shot `grpcurl` service under the `grpc-test` profile — service discovery from inside the compose network:

```bash
docker compose run --rm grpcurl          # runs: grpcurl -plaintext primary:9090 list
```

### Scripting tips

- Always propagate request identity: `curl -s -D- -o /dev/null -H "x-request-id: $(uuidgen)" ...` — the server echoes/propagates it, and the id appears in logs and audit detail, which makes support tickets tractable.
- For CI probes prefer `GET /health` (unauthenticated) over login flows; use `grpc.health.v1.Health/Check` with `{"service": "file_storage.v1.FileService"}` for the gRPC side.
- Capture `file_id` and ETag into variables immediately; UUIDs are the only stable handle across renames.
- Keep `x-request-id` + `Content-Length` in `-D-` header dumps when filing reproduction steps.
- For TLS deployments, add `--insecure` (grpcurl) or `--cacert ca.pem` (curl against a REST ingress) as appropriate; SANs must match dial names — see [TLS](../deployment/tls.md).

## Examples

A complete smoke test suitable for a post-deploy job:

```bash
#!/usr/bin/env bash
set -euo pipefail
BASE=http://localhost:8080/api/v1
TOKEN=$(curl -sf -X POST $BASE/auth/login -H 'Content-Type: application/json' \
  -d '{"username":"viewer","password":"viewer123"}' | jq -r .access_token)
test "$(curl -sf -H "Authorization: Bearer $TOKEN" $BASE/files?limit=1 | jq .total)" -ge 0
curl -sf localhost:8080/ready | jq -e '.status == "ok"' >/dev/null && echo "REST OK"
grpcurl -plaintext -d '{"service": ""}' localhost:9090 grpc.health.v1.Health/Check | grep -q SERVING && echo "gRPC OK"
```

## See also

- [REST API](../api/rest.md) — parameter and response reference for every curl above.
- [gRPC API](../api/grpc.md) — the RPCs behind the grpcurl calls.
- [Authentication](../api/authentication.md) — users, roles and token TTLs.
- [Errors](../api/errors.md) — decoding the error bodies and status codes.
- [Quickstart](../getting-started/quickstart.md) — installing and booting the stack.
