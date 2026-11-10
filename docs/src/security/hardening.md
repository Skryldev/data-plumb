<!-- title: Hardening Checklist category: security audience: security-engineer|operator|architect last_updated: 2026-11-10 -->

# Hardening Checklist

A production-hardening checklist for file-storage, grouped by theme and written to be walked top-to-bottom before go-live and re-verified each audit cycle. Every row names the concrete key, file or procedure involved, so the list doubles as a configuration review sheet. The threats each item addresses are catalogued in the [Threat Model](./threat-model.md).

## Overview

The checklist covers 55 items across seven themes: Secrets, Network, Auth, Storage, Limits, Ops and Process. Items marked **☑ built-in** are already enforced by the shipped defaults or by code. Items marked **☐ operator action** leave the surface open by default and require a change on your side.

## How it works

Work through the themes in order: secrets first, then network, auth, storage, limits, and finally the operational and process controls. After each pass, record the date and owner in your change log so the next audit knows what was verified and when.

## Usage

### Secrets

| Item | Detail | Status |
|---|---|---|---|
| Replace `auth.jwt_secret` | `openssl rand -hex 32`; the default is a placeholder | ☐ operator action |
| Replace `replication.api_key` | `openssl rand -hex 24`; identical on primary and replica | ☐ operator action |
| Remove demo passwords | `admin/admin123`, `alice/alice123`, `bob/bob123`, `viewer/viewer123` must not survive | ☐ operator action |
| Argon2id hashes only | Use `password_hash` (`file-storage hash-password --password '...'`), never `password` | ☐ operator action |
| Key file permissions | Per-tenant keys created `0600` by the service; verify nothing loosened them | ☑ built-in |
| Key files off git | `data/keys/` and TLS private keys never enter version control | ☐ operator action |
| Rotate secrets quarterly | Rotating `jwt_secret` also invalidates all tokens and the in-memory refresh store | ☐ operator action |
| Secret length validation | `jwt_secret` under 16 chars is rejected at boot | ☑ built-in |
| Constant-time key comparison | Internal API keys compared in constant time | ☑ built-in |

### Network

| Item | Detail | Status |
|---|---|---|---|
| TLS termination for REST | Terminate at ingress/LB; the REST listener is plain HTTP by design | ☐ operator action |
| gRPC listener TLS | `server.grpc_tls.enabled=true` with cert/key in untrusted segments | ☐ operator action |
| mTLS for replication | `grpc_tls.client_ca_path` forces CA-signed client certificates | ☐ operator action |
| Firewall `/internal/*` | Restrict the plain-HTTP internal routes to replication peers | ☐ operator action |
| Never expose 8080/9090 directly | Publish only the ingress; both ports are unauthenticated or JWT-only | ☐ operator action |
| Bind addresses | `0.0.0.0` default; restrict with firewall/network policy, not just config | ☐ operator action |
| REST rate limiting | tower_governor per client IP, on by default | ☑ built-in |
| gRPC per-tenant token bucket | On by default; streaming RPCs are not limited — size the perimeter for that | ☑ built-in |

### Auth

| Item | Detail | Status |
|---|---|---|---|
| Least-privilege roles | Prefer `viewer` for read-only consumers; reserve `admin` for operators | ☐ operator action |
| Short access TTL | 900 s default; verification is local with no revocation list | ☑ built-in |
| One user, one tenant | Config structure maps each user to exactly one `tenant_id` | ☑ built-in |
| Tenant mapping reviewed | Ensure service accounts are not accidentally admins | ☐ operator action |
| No plaintext passwords in prod config | Enforced in review; validation cannot detect it | ☐ operator action |
| Login failures audited | `auth.login.failure` events with IP on every failed attempt | ☑ built-in |
| Timing equalisation | Unknown usernames burn argon2 cost via a dummy hash | ☑ built-in |
| Session invalidation procedure | Restart or rotate `jwt_secret` to log everyone out | ☐ operator action |

### Storage

| Item | Detail | Status |
|---|---|---|---|
| Dedicated volumes | Block store, redb index and keys on a dedicated, monitored volume | ☐ operator action |
| Single-writer discipline | One process per `database.path`; never share the data dir over NFS | ☑ built-in |
| redb fsync durability | Commits use `Durability::Immediate` (WAL + fsync) | ☑ built-in |
| Atomic blob writes | Temp + rename + dir fsync before the index transaction | ☑ built-in |
| GC mode-correct | `refcount` on primaries, `reachability` on replicas | ☑ built-in |
| Orphan sweep tuned | `orphan_min_age_secs` 3600 default; raise for slow replication | ☐ operator action |
| Grace period matches policy | `grace_period_secs` 86400 default; set to your real undo window | ☐ operator action |
| Encryption at rest | Enable `encryption.enabled` for confidential data | ☐ operator action |
| Backups + restore drills | Back up `data/` and rehearse restores; audit history shares the file | ☐ operator action |

### Limits

| Item | Detail | Status |
|---|---|---|---|
| `limits.max_file_size` sized | 10 GiB default; lower it to your real maximum | ☐ operator action |
| `limits.max_body` sized | 1 GiB cap; multipart needs headroom over the largest file | ☐ operator action |
| Rate limits tuned | `rate_limit.per_second`/`burst` per workload | ☐ operator action |
| gRPC message ceiling awareness | 256 MiB max message; streamed uploads rarely hit it | ☑ built-in |
| Decompression bounded | Honours recorded `size_original`; bombs cannot allocate unboundedly | ☑ built-in |
| Replication queue sized | `queue_capacity` 10000; overload backpressures uploads | ☐ operator action |
| Chunking bounds validated | `min ≤ avg ≤ max`, max ≤ 16 MiB, checked at boot | ☑ built-in |

### Ops

| Item | Detail | Status |
|---|---|---|---|
| Metrics scraped | Prometheus targets both nodes on the REST port | ☐ operator action |
| Alert rules | On `hash_mismatch_total`, `replication_failures_total`, audit errors, queue depth | ☐ operator action |
| Audit export off-host | Ship `GET /audit` output or database backups off-host | ☐ operator action |
| JSON logging at info | `logging.format: json`, `level: info` | ☐ operator action |
| Graceful shutdown | Both listeners drain on SIGTERM | ☑ built-in |
| Health probes wired | `/ready` as readiness, `/health` as liveness | ☐ operator action |
| Ready-probe semantics | `/ready` returns 503 while database or storage is unhealthy | ☑ built-in |

### Process

| Item | Detail | Status |
|---|---|---|---|
| Distroless nonroot runtime | Official image runs uid 65532, no shell or package manager | ☑ built-in |
| Read-only rootfs compatible | All state lives under the `/var/lib/file-storage/data` volume | ☑ built-in |
| Volume ownership | Prepare data volumes for uid 65532 (`fsGroup: 65532` on Kubernetes) | ☐ operator action |
| Image signing | Sign and verify the deployed image | ☐ operator action |
| Dependency audit | `cargo audit` against `Cargo.lock` in CI | ☐ operator action |
| Reproducible builds | `cargo build --release --locked`; Dockerfile builds from the lockfile | ☑ built-in |
| Quality gates | `cargo fmt --check` and zero-warning clippy gate changes | ☑ built-in |

## Configuration

Every key cited above is documented with its default in the [Configuration reference](../configuration/reference.md); secret-handling patterns (env injection, mounted files, rotation) are in [Secrets](../configuration/secrets.md). TLS certificate generation belongs to the TLS page in the Deployment section.

## Examples

### The first-hour pass

If you have one hour before go-live, do these five in order:

1. Generate and deploy a fresh `jwt_secret` and replication `api_key` (Secrets theme).
2. Replace the demo users with argon2id-hashed, least-privilege accounts (Secrets + Auth).
3. TLS for REST at the ingress; gRPC TLS with mTLS for replication (Network).
4. Right-size `max_file_size`, `max_body` and rate limits (Limits).
5. Wire Prometheus, alerts and an off-host audit export (Ops).

### Re-verification snippet

```bash
# secrets are not defaults
grep -c "change-me\|admin123" /etc/file-storage/default.yaml   # expect 0
# key files are owner-only
stat -c '%a' /var/lib/file-storage/data/keys/*.key             # expect 600
# gRPC TLS refuses plain connections
grpcurl -plaintext localhost:9090 list                          # expect failure
# ready probe is honest
curl -fsS localhost:8080/ready | jq .status                     # expect "ok"
```

## See also

- [Threat Model](./threat-model.md) — the adversary model behind these rows.
- [Authentication](./authentication.md) — token lifecycle and roles.
- [Encryption at Rest](./encryption.md) — at-rest protections and residual risks.
- [Tenants](./tenants.md) — isolation guarantees.
- [Audit Log](./audit.md) — the event trail you are asked to export.
- [Configuration reference](../configuration/reference.md) — every key cited above.
- [Secrets](../configuration/secrets.md) — storing and rotating secrets.
