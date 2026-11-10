<!-- title: Python Client category: integration audience: developer last_updated: 2026-11-10 -->

# Python Client

Python talks to the service through `grpcio` and stubs generated from `proto/file_storage/v1/`. This page walks through generating the modules, a complete synchronous client (login, streaming upload, streaming download, error handling), and an `asyncio` sketch with `grpc.aio`. The wire protocol is identical to the one documented in [gRPC API](../api/grpc.md), so Rust, Go and Python clients interoperate freely.

## Overview

Three commands get you started:

```bash
python -m pip install grpcio grpcio-tools grpcio-health-checking grpcio-reflection
mkdir -p gen
python -m grpc_tools.protoc \
  -Iproto \
  -I"$(python -c 'import grpc_tools, os; print(os.path.join(os.path.dirname(grpc_tools.__file__), "_proto"))')" \
  --python_out=gen --pyi_out=gen --grpc_python_out=gen \
  proto/file_storage/v1/common.proto \
  proto/file_storage/v1/auth.proto \
  proto/file_storage/v1/file.proto \
  proto/file_storage/v1/admin.proto
```

The extra `-I` path supplies the well-known `google/protobuf/timestamp.proto` from the `grpc_tools` bundle. The result is importable as `gen/file_storage/v1/{common,auth,file,admin}_pb2*.py` (set `sys.path` to `gen` or package it).

## How it works

Streaming follows the header-first rule. **Upload** is a request iterator whose first yielded message sets `UploadChunk.header` and whose remaining messages set `UploadChunk.data` — send 1 MiB pieces to keep memory flat. **Download** iterates the response stream: the first message carries `DownloadChunk.header` (including the 32-byte `original_hash` for verification) and the rest carry ordered plaintext bytes. The bearer token from `Login` must be attached as `authorization: Bearer <jwt>` metadata on every call, streams included.

## Examples

### Synchronous client

```python
#!/usr/bin/env python3
import sys
sys.path.insert(0, "gen")

import hashlib
import grpc
from file_storage.v1 import auth_pb2, auth_pb2_grpc, file_pb2, file_pb2_grpc

TARGET = "localhost:9090"

with grpc.insecure_channel(TARGET) as channel:
    auth = auth_pb2_grpc.AuthServiceStub(channel)
    login = auth.Login(auth_pb2.LoginRequest(username="alice", password="alice123"))
    metadata = (("authorization", f"Bearer {login.access_token}"),)

    files = file_pb2_grpc.FileServiceStub(channel)

    # --- upload: yield the header, then byte chunks ---
    payload = b"hello from python" * 1000

    def requests():
        yield file_pb2.UploadChunk(
            header=file_pb2.UploadHeader(
                name="hello.txt",
                mime_type="text/plain",
                expected_size=len(payload),  # oversize streams fail fast
                tags=["demo"],
            )
        )
        for i in range(0, len(payload), 1 << 20):
            yield file_pb2.UploadChunk(data=payload[i : i + (1 << 20)])

    resp = files.Upload(requests(), metadata=metadata)
    file_id = resp.manifest.file_id
    print(f"uploaded {file_id} dedup_chunks={resp.dedup_chunks} stored={resp.stored_bytes}")

    # --- download: header message first, then data ---
    hasher = hashlib.blake2b(digest_size=32)  # or a BLAKE3 package
    with open("hello-copy.txt", "wb") as out:
        for msg in files.Download(file_pb2.DownloadRequest(file_id=file_id), metadata=metadata):
            kind = msg.WhichOneof("payload")
            if kind == "header":
                print(f"total_size={msg.header.total_size}")
            elif kind == "data":
                out.write(msg.data)
                hasher.update(msg.data)
    # Compare hasher.digest() with msg.header.original_hash for full verification.

    # --- soft delete ---
    files.DeleteFile(file_pb2.DeleteFileRequest(file_id=file_id), metadata=metadata)
```

### Error handling

Every stub method raises `grpc.RpcError`; inspect `e.code()` against the catalog in [Errors](../api/errors.md):

```python
try:
    files.GetManifest(file_pb2.GetManifestRequest(file_id="0193-unknown"), metadata=metadata)
except grpc.RpcError as e:
    if e.code() == grpc.StatusCode.NOT_FOUND:
        print("missing:", e.details())
    elif e.code() == grpc.StatusCode.PERMISSION_DENIED:
        print("wrong tenant — needs an admin account")
    else:
        raise
```

### asyncio sketch

`grpc.aio` mirrors the same API with coroutines; the request iterator becomes an async generator:

```python
import asyncio
import grpc.aio

async def main():
    async with grpc.aio.insecure_channel(TARGET) as channel:
        auth = auth_pb2_grpc.AuthServiceStub(channel)
        login = await auth.Login(auth_pb2.LoginRequest(username="alice", password="alice123"))
        metadata = (("authorization", f"Bearer {login.access_token}"),)
        files = file_pb2_grpc.FileServiceStub(channel)

        async def requests():
            yield file_pb2.UploadChunk(header=file_pb2.UploadHeader(name="async.bin"))
            yield file_pb2.UploadChunk(data=b"chunk-1")
            yield file_pb2.UploadChunk(data=b"chunk-2")

        resp = await files.Upload(requests(), metadata=metadata)
        stream = files.Download(
            file_pb2.DownloadRequest(file_id=resp.manifest.file_id), metadata=metadata
        )
        async for msg in stream:
            if msg.WhichOneof("payload") == "data":
                print(len(msg.data), "bytes")

asyncio.run(main())
```

## Configuration

Operational pointers for the Python side:

- **Health checks** — use `grpcio-health-checking` against `grpc.health.v1.Health`; the server serves checks for `file_storage.v1.FileService`, `AuthService`, `AdminService`, `ReplicationService` and the overall service name `""`, probed internally every 10 s:
  ```python
  from grpc_health.v1 import health_pb2, health_pb2_grpc
  health = health_pb2_grpc.HealthStub(channel)
  resp = health.Check(health_pb2.HealthCheckRequest(service="file_storage.v1.FileService"))
  print(resp.status == health_pb2.HealthCheckResponse.SERVING)
  ```
- **Reflection** — `grpcio-reflection` can list services and resolve descriptors without generated stubs, mirroring `grpcurl list`; reflection is registered at boot together with the health service.
- **TLS** — pass `grpc.ssl_channel_credentials(root_certificates=ca_pem)` to `secure_channel`; the dial name must match a SAN in the certificate (see [TLS](../deployment/tls.md)).
- Keep pieces at 1 MiB; the server's maximum message size is 256 MiB but large single messages defeat HTTP/2 flow control.

## See also

- [gRPC API](../api/grpc.md) — full message and streaming reference.
- [Authentication](../api/authentication.md) — roles, refresh rotation, expiry.
- [Errors](../api/errors.md) — status codes and REST equivalents.
- [SDK Design](./sdk-design.md) — why the header-first protocol exists.
- [CLI Recipes](./cli.md) — grpcurl equivalents for smoke tests.
- [Quickstart](../getting-started/quickstart.md) — boot the server first.
