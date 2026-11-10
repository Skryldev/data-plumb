<!-- title: Docker category: getting-started audience: developer|operator last_updated: 2026-11-10 -->

# Docker

This page explains the project Dockerfile, how to build and run a single container, and the runtime characteristics of the image (size, user, volumes, health probes, logging). For a multi-container primary/replica stack, continue to [Docker Compose](../deployment/docker-compose.md).

## Overview

The image is a two-stage build: compilation happens in `rust:1.83-slim`, and the runtime stage is `gcr.io/distroless/cc-debian12:nonroot`. The final image contains only the ~12 MB binary and the bundled configuration — no shell, no package manager, no debug tools — and it runs as a non-root user. Total image size lands in the **~30 MB** range. Because `build.rs` vendors the protobuf compiler through `protoc-bin-vendored`, neither the builder nor the runtime image needs a system `protoc` or OpenSSL.

## How it works

The Dockerfile in the repository root:

```dockerfile
# syntax=docker/dockerfile:1

# ---------- build stage ----------
# protoc is unnecessary on the build host: build.rs uses the vendored
# compiler from the `protoc-bin-vendored` crate.
FROM rust:1.83-slim AS build
WORKDIR /src
RUN apt-get update && apt-get install -y --no-install-recommends pkg-config && rm -rf /var/lib/apt/lists/*
COPY Cargo.toml Cargo.lock* build.rs ./
COPY proto ./proto
COPY src ./src
RUN cargo build --release --locked 2>/dev/null || cargo build --release

# ---------- runtime stage ----------
FROM gcr.io/distroless/cc-debian12:nonroot
COPY --from=build /src/target/release/file-storage /usr/local/bin/file-storage
COPY --from=build /src/config /etc/file-storage
ENV RUST_LOG=info
EXPOSE 8080 9090
WORKDIR /var/lib/file-storage
VOLUME ["/var/lib/file-storage/data"]
ENTRYPOINT ["/usr/local/bin/file-storage", "--config", "/etc/file-storage/default.yaml"]
```

Key properties:

| Property | Value |
|---|---|
| Builder image | `rust:1.83-slim` (plus `pkg-config` only) |
| Runtime image | `gcr.io/distroless/cc-debian12:nonroot` |
| Listener ports | `EXPOSE 8080` (REST) and `9090` (gRPC) |
| Configuration | baked into `/etc/file-storage/` (default, production templates) |
| Entry point | `/usr/local/bin/file-storage --config /etc/file-storage/default.yaml` |
| Data volume | `/var/lib/file-storage/data` (blocks, temp dir, redb index, keys) |
| Image size | ~30 MB range (12 MB binary + distroless cc base) |
| Process user | `nonroot` (uid `65532`) |

## Configuration

Override any configuration key with `FS_`-prefixed environment variables (double underscore for nesting), which merge on top of the baked-in YAML:

| Environment variable | Purpose |
|---|---|
| `FS_SERVER__PORT` | REST port (default 8080) |
| `FS_SERVER__GRPC_PORT` | gRPC port (default 9090) |
| `FS_STORAGE__ROOT` | Block store root inside the container |
| `FS_DATABASE__PATH` | redb index path |
| `FS_ENCRYPTION__KEYS_DIR` | Per-tenant key directory |
| `FS_LOGGING__FORMAT` | `pretty` or `json` logs |

The full override syntax is documented in the [Configuration reference](../configuration/reference.md).

## Examples

### Build the image

```bash
docker build -t file-storage:1.0.0 .
```

### Run a single node with a persistent volume

```bash
docker volume create fs-data
docker run -d --name file-storage \
  -p 8080:8080 -p 9090:9090 \
  -v fs-data:/var/lib/file-storage/data \
  -e FS_LOGGING__FORMAT=json \
  file-storage:1.0.0
```

**Volume permissions.** The process runs as uid `65532` (`nonroot`). A freshly created named volume starts root-owned, so either initialise ownership once with a helper container, or prepare a bind mount with the right owner:

```bash
# one-time fix for an existing volume
docker run --rm -v fs-data:/data alpine chown -R 65532:65532 /data
```

For bind mounts, create the host directory and `chown 65532:65532` it before starting the container. Fresh named volumes used from the first boot are simplest.

### Health probes

The runtime exposes two unauthenticated endpoints on the REST port:

| Endpoint | Success response | Failure behaviour |
|---|---|---|
| `/health` | `{"status":"ok","version":"...","uptime_secs":N}` | — (liveness) |
| `/ready` | `{"status":"ok","database":"ok","storage":"ok"}` | HTTP 503 while the database or storage root is unavailable |

Use `/health` as the container liveness probe and `/ready` as the readiness probe:

```bash
curl -fsS localhost:8080/ready && echo ready
```

### Logging

The service logs to stdout in `pretty` or `json` format (`logging.format`). Use the standard json-file driver with rotation, or any log shipper:

```bash
docker run -d --name file-storage \
  --log-driver json-file \
  --log-opt max-size=10m --log-opt max-file=5 \
  -p 8080:8080 -p 9090:9090 \
  -v fs-data:/var/lib/file-storage/data \
  file-storage:1.0.0
```

## See also

- [Docker Compose](../deployment/docker-compose.md) — primary + replica + Prometheus stack.
- [Quickstart](./quickstart.md) — first commands against a running container.
- [Configuration reference](../configuration/reference.md) — all `FS_` environment overrides.
- [TLS](../deployment/tls.md) — TLS for the gRPC listener and mTLS for replication peers.
- [Monitoring](../operations/monitoring.md) — scraping `/metrics` from the container.
