<!--
title: ADR 0008 — gRPC alongside REST
category: architecture
audience: all
last_updated: 2026-11-10
-->

# ADR 0008: gRPC alongside REST

This decision record explains why the service runs a second, tonic-based gRPC listener next to the REST API rather than replacing REST or extending it. It covers what the two listeners share, where their semantics intentionally differ, and the operational costs of maintaining two API surfaces.

## Overview

file-storage exposes one storage core through two protocols in one process: REST (axum 0.7) on `server.port` (8080) and gRPC (tonic 0.12) on `server.grpc_port` (9090), both backed by the same `AppState`. gRPC exists for streaming uploads/downloads with typed contracts, the internal replication protocol, and lower per-RPC overhead; REST remains the universal surface for humans, scripts, and browsers. Neither is a shim over the other — both call the same pipeline, retrieval engine, index, and caches.

## Status

**Accepted** — 2026-11-10.

## Context

REST served the first users well (curl-able, browser-friendly, ubiquitous tooling), but four pressures motivated a second protocol:

- **Streaming semantics** — large uploads and downloads want first-class client- and server-streaming with HTTP/2 flow control; hand-rolled chunked JSON is awkward, and multipart framing is a poor fit for programmatic clients.
- **Typed contracts** — SDKs (Rust today, Go/Python documented) want generated types instead of hand-maintained JSON mappings; a schema also freezes the internal replication protocol between nodes of different versions.
- **Internal replication** — node-to-node traffic (`ReplicateBatch`, `PullChunk`, `PullManifest`) benefits from binary framing, bidi streaming, and health/reflection for orchestration probes.
- **Per-RPC overhead** — JSON encoding plus header-heavy requests cost measurable latency on the hot path compared with protobuf over HTTP/2.

At the same time, dropping REST would break the simplest integrations: curl one-liners, browser downloads with `Range` and `ETag`, and every HTTP-native client.

## Decision

Run a **second listener** in the same process:

- tonic 0.12 on `server.grpc_port` (9090), sharing `AppState` with the axum stack; graceful shutdown covers both.
- Four project services in `file_storage.v1` (protos under `proto/file_storage/v1/`): `FileService` (client-streaming `Upload`, server-streaming `Download` with `start`/`end`, `GetManifest`, `ListFiles`, `DeleteFile`), `AuthService`, `AdminService` (including `TriggerGC`, `Resync`, `QueryAudit`), and `ReplicationService` — plus `grpc.health.v1.Health` (background probe every 10 s flips `NOT_SERVING` within ≤15 s) and `grpc.reflection.v1.ServerReflection` for `grpcurl` discovery without proto files.
- **Unary interceptors**: JWT → `AuthContext`, admin gate, per-tenant rate limiting, request-id propagation. **Streaming RPCs verify auth in-handler**, because tonic interceptors only execute for unary calls. Router-level layers add gRPC metrics and panic recovery (grpc-status 13).
- Server tuning mirrors the REST limits: 256 MiB max message, HTTP/2 keepalive 30 s/timeout 20 s, 64 concurrent requests per connection, 4 MiB stream / 8 MiB connection windows, TCP keepalive 30 s, gzip compression both directions.
- **Proto field discipline**: hashes are raw 32-byte fields; `ReplicationOp` uses a `oneof` envelope; field numbers are frozen once released, so proto evolution is additive only.

## How it works

The two surfaces are intentionally equivalent at the storage layer and intentionally different at the wire layer:

| Aspect | REST (`/api/v1`, axum) | gRPC (`file_storage.v1`, tonic) |
|---|---|---|
| Upload | `POST /files` — multipart or raw body, streamed | `Upload` client-streaming: `UploadChunk{header first \| data}` |
| Download | `GET /files/:id` — full body, `ETag`/304 | `Download` server-streaming: header, then data frames |
| Ranges | `Range: bytes=start-end` (inclusive end; suffix `-N`; multi-range → first range) → 206/416 | `Download{start, end}` half-open exclusive end, clamped to size; `OUT_OF_RANGE` when `start ≥ size` |
| Browser support | Native (fetch, `<a download>`, service workers) | None directly; gRPC-Web needs a translating proxy |
| Tooling | curl, httpie, every HTTP client | grpcurl (via reflection), codegen'd SDKs |
| Internal replication | `/internal/*` with `X-Api-Key` (the built-in WAL shipper's transport) | `ReplicationService` with `x-api-key` metadata |
| Rate limiting | tower_governor per client IP | Per-tenant token bucket; streaming RPCs not rate limited (documented) |
| Errors | JSON `{"error":{"code","message"}}` | gRPC status codes mapped from `StorageError` |

Both listeners emit the same `file_storage_*` metrics with their own dimensions (`http_requests_total{method,route,status}` versus `grpc_requests_total{rpc,code}`), and a file uploaded over either is byte-identical when downloaded over the other.

## Consequences

**Positive**

- **Typed, versioned contracts**: generated clients, reflection-driven discovery, and a frozen replication wire format that tolerates version skew between nodes.
- **First-class streaming**: client-streaming uploads and server-streaming ranged downloads with real HTTP/2 flow control (4 MiB / 8 MiB windows) and gzip framing.
- **Cheaper internal traffic**: protobuf framing keeps node-to-node replication compact; health + reflection make Kubernetes probes and `grpcurl`-based debugging trivial.
- **One storage core**: transport parity is structural, not aspirational — the same pipeline, caches, and index serve both listeners, so behaviour drift is unlikely.
- **Incremental adoption**: existing REST integrations keep working while performance-sensitive clients move to gRPC.

**Negative**

- **Two API surfaces to document and test**: the suite carries separate REST (17) and gRPC (18) E2E tests plus duplicate reference pages; every feature must land on both sides or be explicitly documented as asymmetric.
- **tonic's interceptor limitation**: interceptors run only for unary RPCs, so streaming handlers duplicate auth logic in-handler; a new streaming RPC that forgets the check is a plausible bug class — mitigated by review checklists, not by the type system.
- **Proto evolution discipline**: field numbers are permanent; renames and type changes are breaking. API changes require additive fields and a version bump of `file_storage.v1`.
- **Two listeners to operate**: two ports in firewalls and containers, TLS configured per listener (`server.grpc_tls`), and per-listener dashboards.

## Alternatives considered

- **Replace REST with gRPC** — one protocol, one test suite; but it breaks curl workflows, browser downloads, and every HTTP-native integration on day one, and `Range`/`ETag` semantics have no natural gRPC equivalent. Rejected as needlessly destructive.
- **GraphQL** — good for shaped reads, poor for binary streaming: file bytes do not fit resolver semantics well, and the upload path would fight the query layer. Rejected.
- **Pure gRPC-Web** — browsers could talk gRPC directly, but it requires a translating proxy (Envoy or equivalent), still lacks true bidirectional streaming from browsers, and adds a moving part to every deployment. Rejected for now.
- **Keep REST only, add chunked-JSON streaming** — avoids a second listener but reimplements framing, flow control, and contracts poorly; the SDK would hand-map JSON fragments. Rejected after prototyping costs were compared with tonic codegen.

## Usage

| Key | Default | Effect |
|---|---|---|
| `server.port` | `8080` | REST listener |
| `server.grpc_port` | `9090` | gRPC listener |
| `server.grpc_tls.enabled` | `false` | TLS on the gRPC listener (`cert_path`, `key_path`) |
| `server.grpc_tls.client_ca_path` | `null` | Set to require client certificates (mTLS) |

The Rust client SDK (`file_storage::grpc::FileStorageClient`) speaks to the gRPC listener with a built-in `RetryPolicy` (3 attempts, 100 ms → 2 s backoff, retrying `Unavailable`/`DeadlineExceeded`/`Aborted`); see [Rust Client](../../integration/rust-client.md).

## Examples

Discover and call the API without a proto file on disk:

```bash
grpcurl -plaintext localhost:9090 list
grpcurl -plaintext -d '{"username":"admin","password":"admin123"}' \
    localhost:9090 file_storage.v1.AuthService/Login
grpcurl -plaintext -d '{"service": "file_storage.v1.FileService"}' \
    localhost:9090 grpc.health.v1.Health/Check
```

The same file from both transports:

```rust
let mut client = file_storage::grpc::FileStorageClient::connect("http://127.0.0.1:9090").await?;
client.login("admin", "admin123").await?;
let resp = client.upload(
    pb::UploadHeader { name: "demo.bin".into(), ..Default::default() },
    std::io::Cursor::new(payload),
).await?;
```

```bash
curl -s -X POST "localhost:8080/api/v1/files?name=demo.bin" \
     -H "Authorization: Bearer $TOKEN" --data-binary @demo.bin
```

## See also

- [gRPC API](../../api/grpc.md) — the complete service reference
- [REST API](../../api/rest.md) — the HTTP surface, including Range/ETag semantics
- [Errors](../../api/errors.md) — the shared `StorageError` mapping table
- [Replication](../replication.md) — the internal protocol's use of both transports
- [TLS](../../deployment/tls.md) — terminating TLS for each listener
- [Rust Client](../../integration/rust-client.md) — SDK design and retry policy
