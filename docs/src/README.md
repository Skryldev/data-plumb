<!--
title: file-storage documentation
category: all
audience: all
last_updated: 2026-11-10
-->

# file-storage

`file-storage` is a production-grade **hybrid chunked content-addressed file storage microservice** written in Rust. Files are split with FastCDC content-defined chunking, hashed with BLAKE3, compressed with zstd, optionally encrypted with AES-256-GCM, and stored as content-addressed blobs with a redb ACID metadata index and a three-tier moka cache. Two transports expose the same storage core: a REST API (axum) and a gRPC API (tonic), served by one process on two ports.

## What this book covers

| Section | Audience | Contents |
|---|---|---|
| [Architecture](./architecture/overview.md) | architects, developers | components, data flow, storage layout, chunking, caching, replication, GC, security, and 8 architecture decision records |
| [Getting Started](./getting-started/quickstart.md) | everyone | 5-minute quickstart, installation, Docker, first file walkthrough |
| [API Reference](./api/rest.md) | developers | full REST reference, full gRPC reference, error catalog, authentication, copy-paste examples in curl/Rust/Go/Python |
| [Configuration](./configuration/reference.md) | operators | every config key, env overrides, production walkthrough, secrets management |
| [Deployment](./deployment/docker-compose.md) | operators | Docker Compose, Kubernetes StatefulSet, scaling, TLS |
| [Security](./security/threat-model.md) | security engineers | threat model, auth internals, encryption at rest, tenant isolation, audit log, 50+ item hardening checklist |
| [Operations](./operations/monitoring.md) | operators | Prometheus metrics catalog, backup & restore, GC operations, replication ops, troubleshooting FAQ, incident runbook |
| [Integration](./integration/rust-client.md) | developers | Rust client SDK, Go and Python clients, curl/grpcurl recipes, SDK design patterns |
| [Performance](./performance/benchmarks.md) | engineers | benchmarks, tuning knobs, profiling guide |
| [Migration](./migration/v0-to-v1.md) | early adopters | v0 → v1 migration notes |

## The 60-second version

```bash
cargo run --release -- --config config/default.yaml
```

Two listeners come up: **REST on :8080**, **gRPC on :9090**.

```bash
# REST
TOKEN=$(curl -s -X POST localhost:8080/api/v1/auth/login \
  -H 'Content-Type: application/json' \
  -d '{"username":"admin","password":"admin123"}' | jq -r .access_token)

echo "hello" | curl -s -X POST "localhost:8080/api/v1/files?name=hello.txt" \
  -H "Authorization: Bearer $TOKEN" --data-binary @-
```

```bash
# gRPC — discovery works without any proto files thanks to reflection
grpcurl -plaintext localhost:9090 list
```

```rust
// Rust client SDK
let mut c = file_storage::grpc::FileStorageClient::connect("http://127.0.0.1:9090").await?;
c.login("admin", "admin123").await?;
let resp = c.upload(
    file_storage::grpc::generated::pb::UploadHeader {
        name: "hello.txt".into(),
        ..Default::default()
    },
    std::io::Cursor::new(b"hello".to_vec()),
).await?;
```

A file uploaded over gRPC and downloaded over REST (or the reverse) is byte-identical — both transports share one `AppState`, one block store, one index.

## Where to go next

- Building it: [Installation](./getting-started/installation.md)
- The complete REST surface: [REST API](./api/rest.md)
- The complete gRPC surface: [gRPC API](./api/grpc.md)
- Tuning chunk sizes and caches: [Tuning](./performance/tuning.md)
