<!-- title: Production Configuration category: configuration audience: operator last_updated: 2026-11-10 -->

# Production Configuration

`config/production.yaml` is the batteries-included template for a real deployment: TLS on the gRPC listener, encryption at rest, argon2id password hashes, replication enabled and JSON logging. This page walks through the file section by section and lists the three secrets you must replace before rollout. **There is no hot reload — plan a rolling restart for any change.**

## Overview

The template differs from `config/default.yaml` in these ways: `server.grpc_tls.enabled=true` with mTLS (`client_ca_path` set), paths under `/var/lib/file-storage/data/`, `auth.users` reduced to a single admin with `password_hash` (no plaintext), `encryption.enabled=true`, higher rate limits (`per_second: 1000`, `burst: 2000`), replication enabled against `http://replica:8080`, and `logging.format: json`. Everything else (chunking, caches, retention, limits) matches the defaults.

## How it works

Secrets to replace **before first boot** — the file's header comment names them too:

| Placeholder | Replace with | How |
|---|---|---|
| `auth.jwt_secret: REPLACE_WITH_OPENSSL_RAND_HEX_32` | 64 hex chars | `openssl rand -hex 32` |
| `auth.users[].password_hash` | argon2id PHC string | `file-storage hash-password --password '…'` |
| `replication.api_key: REPLACE_WITH_CLUSTER_API_KEY` | shared peer secret | `openssl rand -hex 24` |

Boot the server with the template and rely on the validation rules (secret length, cert paths present, user credentials) to catch mistakes early; see the [Configuration reference](./reference.md) for every key.

## Configuration

Section-by-section walkthrough of the actual file:

```yaml
server:
  host: "0.0.0.0"
  port: 8080
  grpc_port: 9090
  grpc_tls:
    enabled: true
    cert_path: "/etc/file-storage/tls/server.crt"
    key_path: "/etc/file-storage/tls/server.key"
    client_ca_path: "/etc/file-storage/tls/client-ca.crt"   # mTLS for peers
```

TLS terminates at the gRPC listener; clients connecting without TLS fail immediately. With `client_ca_path` set, peer certificates must chain to that CA — this is how replicas authenticate each other. Certificate provisioning is covered in [TLS](../deployment/tls.md).

```yaml
storage:
  root: "/var/lib/file-storage/data/blocks"
  temp_dir: "/var/lib/file-storage/data/tmp"
database:
  path: "/var/lib/file-storage/data/index.redb"
encryption:
  enabled: true
  keys_dir: "/var/lib/file-storage/data/keys"
```

Put `storage.root`, `database.path` and `encryption.keys_dir` on a **persistent volume** — the block store, the redb index and the per-tenant AES keys must survive restarts together; losing the keys directory with `encryption.enabled=true` makes all blobs unreadable (see [Secrets](./secrets.md)).

```yaml
auth:
  jwt_secret: "REPLACE_WITH_OPENSSL_RAND_HEX_32"
  access_ttl_secs: 900
  refresh_ttl_secs: 604800
  users:
    - username: admin
      password_hash: "$argon2id$v=19$m=65536,t=3,p=4$…"
      tenant_id: "01930000-0000-7000-8000-000000000001"
      role: admin
```

Exactly one admin user with an argon2id hash — no plaintext passwords in production files. The example hash in the template is for `admin123` and exists only so the file parses; generate your own.

```yaml
rate_limit:
  per_second: 1000
  burst: 2000
replication:
  enabled: true
  mode: async
  target_url: "http://replica:8080"
  api_key: "REPLACE_WITH_CLUSTER_API_KEY"
  queue_capacity: 50000
retention:
  gc_mode: refcount
  grace_period_secs: 86400
limits:
  max_file_size: 10737418240
  max_body: 1073741824
logging:
  format: json
  level: info
```

Retention tuning: lower `grace_period_secs` (default 86400 = 24 h) if you reclaim space faster, or raise it to widen the undelete window; `gc_interval_secs` 300 keeps background sweeps frequent. `queue_capacity: 50000` widens the replication WAL buffer for bursty write loads. JSON logs with `level: info` are grep- and ingest-friendly; the [Monitoring](../operations/monitoring.md) page maps them to metrics.

## Examples

Generate the three secrets and inject them:

```bash
openssl rand -hex 32            # -> auth.jwt_secret
./file-storage hash-password --password 'S3cret-hunter2'   # -> password_hash
openssl rand -hex 24            # -> replication.api_key (same value on both peers)
```

Deployment targets: the repository's `docker-compose.yml` runs a primary (8080/9090) and a replica (8081/9091) with Prometheus on host port 9099, plus a `grpc-test` profile with grpcurl — see [Docker Compose](../deployment/docker-compose.md). For cluster deployments, [Kubernetes](../deployment/kubernetes.md) mounts the YAML and the TLS/key material as secrets/volumes and sets overrides via `FS_*` environment variables ([Environment variables](./environment.md)).

Finish with the **hardening checklist** — it has 50+ items (firewalling `/internal/*`, secret permissions, mTLS, backups, audit review cadence) in [Hardening checklist](../security/hardening.md), and the [Threat model](../security/threat-model.md) explains what each control buys you.

## See also

- [Configuration reference](./reference.md) — every key with constraints
- [Environment variables](./environment.md) — `FS_*` overrides
- [Secrets](./secrets.md) — jwt_secret / api_key / key-file handling
- [TLS](../deployment/tls.md) — certificate generation and rotation
- [Docker Compose](../deployment/docker-compose.md), [Kubernetes](../deployment/kubernetes.md)
- [Monitoring](../operations/monitoring.md), [Hardening checklist](../security/hardening.md)
