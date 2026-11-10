<!-- title: Configuration Reference category: configuration audience: operator last_updated: 2026-11-10 -->

# Configuration Reference

file-storage is configured from one YAML file (passed as the CLI argument, e.g. `config/default.yaml`) plus `FS_`-prefixed environment variables that override individual keys. This page lists every key with its type, default, constraints and the environment variable that overrides it. **There is no hot reload: every change requires a process restart.**

## Overview

Configuration is a single `AppConfig` tree mirroring `config/default.yaml`. The full file is deserialized with `serde(default)`, so **unknown/new keys fall back to defaults and existing minimal YAML files keep working** across upgrades. After loading (and after env overrides are merged) the config is validated; invalid values fail boot with a clear message rather than corrupting data.

## How it works

Load order: YAML file first, then environment overrides (env wins — see [Environment variables](./environment.md)). Validation then enforces the cross-field rules below; anything invalid aborts startup before any listener binds. Paths in the config are relative to the process working directory unless absolute. Runtime behavior of several keys is observable via `/metrics`; see [Monitoring](../operations/monitoring.md).

## Configuration

### server

| Key | Type | Default | Constraints | Description | Env |
|---|---|---|---|---|---|
| `server.host` | string | `"0.0.0.0"` | valid bind address | Bind address for both listeners | `FS_SERVER__HOST` |
| `server.port` | u16 | 8080 | 1..65535 | REST (axum) listener port | `FS_SERVER__PORT` |
| `server.grpc_port` | u16 | 9090 | 1..65535 (`0` = ephemeral, tests) | gRPC (tonic) listener port, same host | `FS_SERVER__GRPC_PORT` |
| `server.grpc_tls.enabled` | bool | false | — | Serve gRPC over TLS | `FS_SERVER__GRPC_TLS__ENABLED` |
| `server.grpc_tls.cert_path` | path | `/etc/file-storage/tls/server.crt` | required when enabled | Server certificate (PEM) | `FS_SERVER__GRPC_TLS__CERT_PATH` |
| `server.grpc_tls.key_path` | path | `/etc/file-storage/tls/server.key` | required when enabled | Server private key (PEM) | `FS_SERVER__GRPC_TLS__KEY_PATH` |
| `server.grpc_tls.client_ca_path` | path/null | null | — | Set to require client certs chained to this CA (mTLS) | `FS_SERVER__GRPC_TLS__CLIENT_CA_PATH` |

### storage & database

| Key | Type | Default | Constraints | Description | Env |
|---|---|---|---|---|---|
| `storage.root` | path | `data/blocks` | — | Block-store root; blobs live at `ab/cd/<64-hex>.bin` | `FS_STORAGE__ROOT` |
| `storage.temp_dir` | path | `data/tmp` | — | Spool directory for multipart uploads | `FS_STORAGE__TEMP_DIR` |
| `storage.shard_depth` | u32 | 2 | ≤ 4 | Directory fan-out levels for the block store | `FS_STORAGE__SHARD_DEPTH` |
| `database.path` | path | `data/index.redb` | — | redb index file (chunks, files, name/tag index, audit, tenants) | `FS_DATABASE__PATH` |

### chunking & compression

| Key | Type | Default | Constraints | Description | Env |
|---|---|---|---|---|---|
| `chunking.min` | usize | 8192 | > 0, ≤ avg | FastCDC minimum chunk (bytes) | `FS_CHUNKING__MIN` |
| `chunking.avg` | usize | 32768 | min ≤ avg ≤ max | Target chunk size (bytes) | `FS_CHUNKING__AVG` |
| `chunking.max` | usize | 131072 | ≤ 16 MiB (16777216) | Hard chunk ceiling (bytes) | `FS_CHUNKING__MAX` |
| `compression.level` | i32 | 3 | 1..=22 | zstd compression level | `FS_COMPRESSION__LEVEL` |
| `compression.min_size` | usize | 1024 | — | Do not store compressed when smaller than this | `FS_COMPRESSION__MIN_SIZE` |

### cache (moka)

| Key | Type | Default | Description | Env |
|---|---|---|---|---|
| `cache.l1_max_bytes` | u64 | 268435456 (256 MiB) | Plaintext chunk cache capacity | `FS_CACHE__L1_MAX_BYTES` |
| `cache.l1_ttl_secs` | u64 | 300 | L1 entry TTL | `FS_CACHE__L1_TTL_SECS` |
| `cache.l2_max_bytes` | u64 | 536870912 (512 MiB) | Compressed blob cache capacity | `FS_CACHE__L2_MAX_BYTES` |
| `cache.l2_ttl_secs` | u64 | 3600 | L2 entry TTL | `FS_CACHE__L2_TTL_SECS` |
| `cache.manifest_capacity` | u64 | 10000 | Manifest (L3) entry count | `FS_CACHE__MANIFEST_CAPACITY` |
| `cache.manifest_ttl_secs` | u64 | 60 | Manifest TTL | `FS_CACHE__MANIFEST_TTL_SECS` |

### auth

| Key | Type | Default | Constraints | Description | Env |
|---|---|---|---|---|---|
| `auth.jwt_secret` | string | — | **≥ 16 chars** | HS256 signing secret — treat as a root secret | `FS_AUTH__JWT_SECRET` |
| `auth.access_ttl_secs` | u64 | 900 | — | Access-token lifetime (15 min) | `FS_AUTH__ACCESS_TTL_SECS` |
| `auth.refresh_ttl_secs` | u64 | 604800 | — | Refresh-token lifetime (7 d) | `FS_AUTH__REFRESH_TTL_SECS` |
| `auth.users[].username` | string | — | unique in list | Login name | — |
| `auth.users[].password` | string/null | null | plaintext (dev only) | Cleartext credential | — |
| `auth.users[].password_hash` | string/null | null | argon2id PHC | Preferred production credential | — |
| `auth.users[].tenant_id` | uuid | — | — | Tenant the user belongs to | — |
| `auth.users[].role` | enum | — | `admin\|user\|viewer` | RBAC role | — |

`auth.users` is a list — it cannot be overridden via environment variables (see the list-values caveat in [Environment variables](./environment.md)).

### encryption

| Key | Type | Default | Description | Env |
|---|---|---|---|---|
| `encryption.enabled` | bool | false | AES-256-GCM encrypt blobs at rest, per-tenant keys | `FS_ENCRYPTION__ENABLED` |
| `encryption.keys_dir` | path | `data/keys` | Directory of per-tenant key files (`<tenant>.key`, 0600) | `FS_ENCRYPTION__KEYS_DIR` |

### rate_limit

| Key | Type | Default | Description | Env |
|---|---|---|---|---|
| `rate_limit.enabled` | bool | true | REST IP-keyed governor; gRPC per-tenant bucket (unary only) | `FS_RATE_LIMIT__ENABLED` |
| `rate_limit.per_second` | u32 | 500 | Sustained refill rate | `FS_RATE_LIMIT__PER_SECOND` |
| `rate_limit.burst` | u32 | 1000 | Bucket capacity | `FS_RATE_LIMIT__BURST` |

### replication

| Key | Type | Default | Constraints | Description | Env |
|---|---|---|---|---|---|
| `replication.enabled` | bool | false | requires `target_url` | Ship WAL ops to a replica | `FS_REPLICATION__ENABLED` |
| `replication.mode` | enum | `async` | `async\|sync` | sync blocks uploads until flush | `FS_REPLICATION__MODE` |
| `replication.target_url` | url/null | null | required when enabled | Replica base URL (REST `/internal/*`) | `FS_REPLICATION__TARGET_URL` |
| `replication.upstream_url` | url/null | null | — | Where this replica pulls missing blobs from | `FS_REPLICATION__UPSTREAM_URL` |
| `replication.api_key` | string | dev value | shared cluster secret | Sent as `X-Api-Key` / `x-api-key` | `FS_REPLICATION__API_KEY` |
| `replication.queue_capacity` | usize | 10000 | — | WAL queue depth (backpressure beyond) | `FS_REPLICATION__QUEUE_CAPACITY` |
| `replication.batch_size` | usize | 64 | > 0 when enabled | Ops per flush batch | `FS_REPLICATION__BATCH_SIZE` |
| `replication.flush_interval_ms` | u64 | 5000 | — | Max time between flushes | `FS_REPLICATION__FLUSH_INTERVAL_MS` |
| `replication.sync_timeout_ms` | u64 | 5000 | — | Sync-mode flush timeout | `FS_REPLICATION__SYNC_TIMEOUT_MS` |

### retention

| Key | Type | Default | Description | Env |
|---|---|---|---|---|
| `retention.gc_mode` | enum | `refcount` | `refcount` (primary) \| `reachability` (replica) \| `disabled` | `FS_RETENTION__GC_MODE` |
| `retention.grace_period_secs` | u64 | 86400 | Soft-deleted files survive this long before purge | `FS_RETENTION__GRACE_PERIOD_SECS` |
| `retention.gc_interval_secs` | u64 | 300 | Background GC cadence | `FS_RETENTION__GC_INTERVAL_SECS` |
| `retention.orphan_min_age_secs` | u64 | 3600 | Orphan blobs younger than this are kept | `FS_RETENTION__ORPHAN_MIN_AGE_SECS` |

### limits, logging, tenants

| Key | Type | Default | Constraints | Description | Env |
|---|---|---|---|---|---|
| `limits.max_file_size` | u64 | 10737418240 (10 GiB) | — | Per-upload size cap (the only size quota) | `FS_LIMITS__MAX_FILE_SIZE` |
| `limits.max_body` | usize | 1073741824 (1 GiB) | — | HTTP body cap (`DefaultBodyLimit`) | `FS_LIMITS__MAX_BODY` |
| `logging.format` | enum | `pretty` | `pretty\|json` | Log output format (json for production) | `FS_LOGGING__FORMAT` |
| `logging.level` | string | `info` | trace..error | Log verbosity | `FS_LOGGING__LEVEL` |
| `tenants[].id` | uuid | — | — | Pre-provisioned tenant ids (list — env override not possible) | — |
| `tenants[].name` | string | — | — | Tenant names | — |

## Validation rules

Boot fails fast if any rule is violated:

- `auth.jwt_secret` length **≥ 16 chars**.
- `chunking`: `min > 0` and `min ≤ avg ≤ max`; `max ≤ 16 MiB`.
- `compression.level` in **1..=22**.
- At least one `auth.users` entry, and **every user needs `password` or `password_hash`**.
- `replication.enabled=true` requires `replication.target_url` and `batch_size > 0`.
- `storage.shard_depth` **≤ 4**.
- `server.grpc_tls.enabled=true` requires non-empty `cert_path` **and** `key_path`.

### Hot reload

None. Every key — including seemingly trivial ones like `logging.level` — is read once at startup; changing a value requires a restart. Plan production changes via rolling restarts (see [Production](./production.md)).

## Examples

Minimal working config (everything else defaults):

```yaml
auth:
  jwt_secret: "at-least-16-characters-long"
  users:
    - username: admin
      password_hash: "$argon2id$v=19$m=65536,t=3,p=4$…"
      tenant_id: "01930000-0000-7000-8000-000000000001"
      role: admin
tenants:
  - id: "01930000-0000-7000-8000-000000000001"
    name: default
```

Equivalent one-off override without touching YAML:

```bash
FS_SERVER__PORT=8081 FS_STORAGE__SHARD_DEPTH=3 ./file-storage config/default.yaml
```

## See also

- [Environment variables](./environment.md) — `FS_` override semantics and caveats
- [Production](./production.md) — walkthrough of `config/production.yaml`
- [Secrets](./secrets.md) — handling `jwt_secret`, `api_key`, key files
- [REST](../api/rest.md), [TLS](../deployment/tls.md), [Monitoring](../operations/monitoring.md)
