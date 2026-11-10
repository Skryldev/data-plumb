<!-- title: Environment Variables category: configuration audience: operator last_updated: 2026-11-10 -->

# Environment Variables

Every configuration key can be overridden from the environment without editing the YAML file, using the `FS_` prefix and `__` (double underscore) as the nesting separator. This is the idiomatic way to run the same image in different environments (Compose, Kubernetes, systemd) while keeping one config file. **Environment values always win over YAML values.**

## Overview

The loader (`config` crate, `Environment::with_prefix("FS").separator("__").try_parsing(true).ignore_empty(true)`) maps variable names to config paths:

```
FS_SERVER__GRPC_PORT   →  server.grpc_port
FS_RATE_LIMIT__BURST   →  rate_limit.burst
```

Rules of the mapping:

- Prefix `FS_` is stripped; the remainder is split on `__`; each segment is lowercased to build the path; a single `_` inside a segment is just part of the key name (`FS_STORAGE__ROOT` → `storage.root`).
- `try_parsing(true)` means **numbers and booleans are parsed into their real types**: `FS_RATE_LIMIT__PER_SECOND=1000` becomes an integer, `FS_ENCRYPTION__ENABLED=true` becomes a boolean, `FS_STORAGE__SHARD_DEPTH=3` becomes a number. Everything else stays a string.
- `ignore_empty(true)` means **empty variables are ignored** (`FS_REPLICATION__TARGET_URL=` leaves the YAML value in place) — use variable deletion to "unset".
- Env overrides apply on top of the YAML; a value defined only in the environment is added at that path.

## How it works

Precedence: **environment beats YAML beats serde defaults**. Because the config struct is `#[serde(default)]`, neither the YAML nor the environment needs to mention every key. The merged config is validated once at boot (see [Configuration reference](./reference.md)); an invalid env value (e.g. `FS_COMPRESSION__LEVEL=99`) aborts startup with the same validation error as the equivalent YAML mistake.

**List-values caveat:** sequence fields — `tenants` and `auth.users` — **cannot be overridden via environment variables**. The config crate maps environment variables to scalar leaves; there is no syntax to express a list of structs. Define lists in the YAML file only, and vary everything else with env vars. Attempting `FS_TENANTS=...` has no effect on the parsed config.

## Configuration

Commonly used overrides (full key list in the [Configuration reference](./reference.md)):

| Variable | Path | Example | Effect |
|---|---|---|---|
| `FS_SERVER__PORT` | `server.port` | `8081` | Move the REST listener |
| `FS_SERVER__GRPC_PORT` | `server.grpc_port` | `9091` | Move the gRPC listener |
| `FS_SERVER__GRPC_TLS__ENABLED` | `server.grpc_tls.enabled` | `true` | Serve gRPC over TLS |
| `FS_SERVER__GRPC_TLS__CERT_PATH` | `server.grpc_tls.cert_path` | `/tls/server.crt` | Server certificate |
| `FS_STORAGE__ROOT` | `storage.root` | `/var/lib/fs/blocks` | Block-store location |
| `FS_STORAGE__SHARD_DEPTH` | `storage.shard_depth` | `2` | Directory fan-out (≤ 4) |
| `FS_DATABASE__PATH` | `database.path` | `/var/lib/fs/index.redb` | redb index file |
| `FS_CHUNKING__MAX` | `chunking.max` | `131072` | Max chunk bytes (≤ 16 MiB) |
| `FS_COMPRESSION__LEVEL` | `compression.level` | `3` | zstd level 1..=22 |
| `FS_AUTH__JWT_SECRET` | `auth.jwt_secret` | `<64 hex>` | HS256 secret (≥ 16 chars) |
| `FS_ENCRYPTION__ENABLED` | `encryption.enabled` | `true` | AES-256-GCM at rest |
| `FS_RATE_LIMIT__PER_SECOND` | `rate_limit.per_second` | `1000` | Rate-limit refill |
| `FS_REPLICATION__ENABLED` | `replication.enabled` | `true` | WAL shipping on |
| `FS_REPLICATION__TARGET_URL` | `replication.target_url` | `http://replica:8080` | Replica base URL |
| `FS_REPLICATION__MODE` | `replication.mode` | `async` | `async` or `sync` |
| `FS_RETENTION__GC_MODE` | `retention.gc_mode` | `refcount` | GC strategy |
| `FS_LIMITS__MAX_FILE_SIZE` | `limits.max_file_size` | `10737418240` | Per-upload cap |
| `FS_LOGGING__FORMAT` | `logging.format` | `json` | `pretty` or `json` |
| `FS_LOGGING__LEVEL` | `logging.level` | `info` | Log verbosity |

## Examples

Docker Compose — override just what differs per environment; the image's YAML stays untouched:

```yaml
services:
  file-storage:
    image: file-storage:latest
    command: ["/app/file-storage", "/etc/file-storage/config.yaml"]
    environment:
      FS_SERVER__PORT: "8080"
      FS_SERVER__GRPC_PORT: "9090"
      FS_AUTH__JWT_SECRET: "${JWT_SECRET}"          # from .env / secret store
      FS_RATE_LIMIT__PER_SECOND: "1000"
      FS_REPLICATION__ENABLED: "true"
      FS_REPLICATION__TARGET_URL: "http://replica:8080"
      FS_LIMITS__MAX_FILE_SIZE: "10737418240"
      FS_LOGGING__FORMAT: "json"
    ports: ["8080:8080", "9090:9090"]
```

Equivalent shell invocation for quick experiments:

```bash
FS_SERVER__GRPC_TLS__ENABLED=true \
FS_SERVER__GRPC_TLS__CERT_PATH=/tls/server.crt \
FS_SERVER__GRPC_TLS__KEY_PATH=/tls/server.key \
FS_LOGGING__LEVEL=debug ./file-storage config/default.yaml
```

Sanity-check which values actually landed:

```bash
curl -s localhost:8080/health   # version/uptime — then check logs for the loaded config
```

Remember that all env-based changes still require a restart to take effect (no hot reload), and that secrets in environment variables are readable from `/proc/<pid>/environ` — see [Secrets](./secrets.md) for the trade-offs.

## See also

- [Configuration reference](./reference.md) — every key, default and constraint
- [Production](./production.md) — recommended production values
- [Secrets](./secrets.md) — env vs. mounted secret files
- [REST](../api/rest.md) — probes to verify a restarted instance
- [Monitoring](../operations/monitoring.md) — observable effects of tuning keys
