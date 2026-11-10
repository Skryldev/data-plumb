<!--
title: Security
category: architecture
audience: security-engineer
last_updated: 2026-11-10
-->

# Security

This page is the architecture-level summary of the security model: authentication (JWT + refresh tokens), password hashing, per-tenant encryption at rest, tenant isolation, the audit trail, and transport TLS. Security engineers should read it first and then continue into the dedicated pages of the [Security](../security/threat-model.md) section for the threat model and hardening checklist.

## Overview

Security is layered at four boundaries. **Authentication** verifies humans: users are defined in configuration, passwords are hashed with argon2id, and login issues a short-lived HS256 access token plus a rotating refresh token. **Authorization** scopes every request to a tenant, enforced by middleware on REST and interceptors/in-handler checks on gRPC, with a role ladder `admin > user (write) > viewer (read)`. **Data protection** encrypts blobs at rest with per-tenant AES-256-GCM keys stored as `0600` files, and derives tenant-scoped storage keys so dedup never leaks cross-tenant equality. **Accountability** records audit events into an append-only redb table.

## How it works

### Authentication: JWT HS256 + rotating refresh tokens

`POST /api/v1/auth/login` (or `AuthService/Login`) verifies the password and returns:

```json
{
  "access_token": "eyJhbGciOiJIUzI1NiIs…",
  "refresh_token": "opaque-random-128-bit",
  "token_type": "Bearer",
  "expires_in": 900
}
```

- **Access tokens** are JWTs signed HS256 with `auth.jwt_secret` (must be ≥16 chars) and carry claims `sub` (username), `tenant` (tenant UUID), `role`, `iat`, `exp`, `jti`. Default lifetime: `auth.access_ttl_secs` = 900 (15 minutes).
- **Refresh tokens** are opaque random values held in memory (never persisted), single-use, with TTL `auth.refresh_ttl_secs` = 604800 (7 days). `POST /auth/refresh` rotates the pair and revokes the old token; `POST /auth/logout` is idempotent.
- **Password hashing** uses argon2id via PHC strings in config (`password_hash`), generated with `file-storage hash-password`. Unknown usernames still burn a full argon2 verification against a dummy hash, so login timing does not enumerate users.
- **gRPC auth**: unary interceptors parse `authorization: Bearer <jwt>` into an `AuthContext`; streaming RPCs (`Upload`, `Download`) verify the token in-handler, because tonic interceptors only run for unary calls. Metadata is the same shape as REST headers: `authorization`, `x-api-key`, `x-request-id` (propagated or minted UUIDv7).

```mermaid
sequenceDiagram
    autonumber
    participant C as Client
    participant A as AuthService
    participant M as Middleware / interceptor
    participant S as Storage services

    C->>A: Login {username, password}
    A->>A: argon2id verify (timing-equalized for unknown users)
    A-->>C: access_token (JWT, 900 s) + refresh_token (7 d, single-use)
    C->>M: request with authorization: Bearer &lt;jwt&gt;
    M->>M: verify HS256, check exp, extract tenant + role
    M->>S: call with AuthContext {username, tenant_id, role}
    S->>S: role check (admin > user > viewer) + tenant scope
    C->>A: Refresh {refresh_token}
    A-->>C: new pair; old refresh token revoked
```
*Figure 1: Token issuance and per-request verification. The same `AuthContext` reaches both the REST middleware chain and the gRPC service handlers.*

### Tenant isolation

Isolation is enforced at three depths, so no single bug exposes another tenant's data:

1. **Request scope** — every JWT binds one tenant; `user` and `viewer` roles can only see their tenant's files. Cross-tenant access requires `admin`.
2. **Index scoping** — the `name_index` and `tag_index` redb tables are tenant-prefixed (`tenant(16B) || 0x00 || …`), so a listing query physically scans only the tenant's byte range.
3. **Encryption scoping** — with encryption enabled, blob storage keys are `BLAKE3(tenant_id || plain_hash)` (see [ADR 0007](./decisions/0007-encrypted-dedup-key-derivation.md)), so even dedup equality is invisible across tenants.

### Encryption at rest

Per-tenant AES-256-GCM with 32-byte keys (see [ADR 0005](./decisions/0005-aes-gcm-per-tenant-key.md)):

- Blob layout is self-describing: `nonce(12) || ciphertext || tag(16)`, a fresh random 96-bit nonce per encryption.
- Keys live as hex-encoded files at `<encryption.keys_dir>/<tenant>.key` (default `data/keys`), created with `0600` permissions and cached in memory; a permissions check (`key_file_permissions_ok`) verifies owner-only access on Unix.
- Chunks carry explicit `encrypted` and `compressed` flags in their refs — decoding never relies on magic-byte sniffing.
- Key files are never rewritten by the service; a `key_age_ms` diagnostic exposes the key file's modification time so operators can monitor key age. Rotation is currently an out-of-band procedure (re-encrypt blobs externally); KMS/envelope encryption is documented future work.

Details, key-management guidance, and the file-permissions checklist live in [Encryption at Rest](../security/encryption.md).

### Audit trail

Every consequential action appends a JSON `AuditEvent` to the redb `audit` table, keyed by UUIDv7 so the log is physically time-ordered. Actions: `file.upload`, `file.download`, `file.delete`, `file.purge`, `auth.login.success`, `auth.login.failure`, `tenant.create`, `replication.apply`, `storage.gc`, `replication.resync`. Each event records `id`, `ts_ms`, `actor` (`system` or a username), `tenant_id`, `object`, `ip`, `status`, and `detail`. Admins read it via `GET /api/v1/audit?limit=` or `AdminService/QueryAudit` (which also filters by tenant). See [Audit Log](../security/audit.md).

### Transport and request hygiene

- **gRPC TLS**: `server.grpc_tls` enables TLS on the `:9090` listener (`cert_path`, `key_path`); setting `client_ca_path` turns on mTLS with client certificates — recommended for node-to-node replication links. Deployment details in [TLS](../deployment/tls.md).
- **REST TLS**: the service intentionally ships plain HTTP on `:8080` (internal traffic); terminate TLS at your ingress or load balancer for external exposure.
- **Rate limiting**: REST via tower_governor keyed by client IP (`rate_limit.per_second` 500, `burst` 1000); gRPC via a per-tenant token bucket with the same knobs, disabled per tenant if unset. Streaming RPCs are deliberately not rate limited (documented behaviour).
- **Internal endpoints**: `/internal/*` and the gRPC `ReplicationService` are guarded by the shared `api_key`, not JWTs — they authenticate nodes, not users.

### Known limits

The honest list: static users from config (no user management API), in-memory refresh tokens (restart logs everyone out), no KMS integration, no per-user quota enforcement (`limits.max_file_size` and `max_body` are the only size controls), and streaming RPCs bypass the rate limiter. These are tracked in the [Threat Model](../security/threat-model.md).

## Configuration

| Key | Default | Notes |
|---|---|---|
| `auth.jwt_secret` | — | ≥16 chars; rotating it invalidates all access tokens |
| `auth.access_ttl_secs` | `900` | Access-token lifetime |
| `auth.refresh_ttl_secs` | `604800` | Refresh-token lifetime (7 days) |
| `auth.users[]` | dev fixtures | `username` + `password` (dev) or `password_hash` (PHC argon2id), `tenant_id`, `role` |
| `encryption.enabled` | `false` | Enables the AES-GCM pipeline stage and tenant-scoped storage keys |
| `encryption.keys_dir` | `data/keys` | Per-tenant key files, created `0600` |
| `server.grpc_tls.*` | disabled | `enabled`, `cert_path`, `key_path`, `client_ca_path` (mTLS) |
| `rate_limit.*` | 500 / 1000 | Token-bucket rate and burst |

Secrets handling (including `FS_AUTH__JWT_SECRET` via environment) is covered in [Secrets](../configuration/secrets.md); the production template with an argon2id hash is `config/production.yaml`.

## Examples

Generate a password hash and configure a user:

```bash
file-storage hash-password --password 'correct-horse-battery'
# $argon2id$v=19$m=19456,t=2,p=1$…PHC string…
```

```yaml
auth:
  users:
    - username: alice
      password_hash: "$argon2id$v=19$m=19456,t=2,p=1$…"
      tenant_id: "01930000-0000-7000-8000-000000000002"
      role: user
```

Confirm tenant isolation from a client's point of view:

```bash
# bob (tenant …003) cannot read alice's (tenant …002) file:
curl -s -H "Authorization: Bearer $BOB_TOKEN" localhost:8080/api/v1/files/<alice_file_id>
# {"error":{"code":"forbidden","message":"…"}}

# an admin can cross tenants explicitly via admin-only listings
curl -s -H "Authorization: Bearer $ADMIN_TOKEN" \
     "localhost:8080/api/v1/files?include_deleted=true"
```

Inspect key-file hygiene on the host:

```bash
ls -l data/keys/
# -rw------- 1 app app 64 Nov 10 09:14 01930000-0000-7000-8000-000000000002.key
```

## See also

- [Threat Model](../security/threat-model.md) — attacker-centric analysis of these controls
- [Authentication](../security/authentication.md) — deep dive on tokens and hashing
- [Encryption at Rest](../security/encryption.md) — key management and rotation procedures
- [Tenants](../security/tenants.md) — the isolation model in detail
- [Audit Log](../security/audit.md) — event schema and querying
- [Hardening Checklist](../security/hardening.md) — pre-production checklist
- [ADR 0005 — AES-GCM per-tenant key](./decisions/0005-aes-gcm-per-tenant-key.md) and [ADR 0007 — Encrypted dedup key](./decisions/0007-encrypted-dedup-key-derivation.md) — the cryptography decisions
- [Configuration reference](../configuration/reference.md) — all `auth.*`, `encryption.*`, `server.grpc_tls.*` keys
