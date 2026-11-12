<!-- title: Threat Model category: security audience: security-engineer|architect last_updated: 2026-11-10 -->

# Threat Model

This page identifies what file-storage must protect, who the adversaries are, which surfaces they can reach, and how each concrete threat is mitigated in the implemented code. It is deliberately concrete: every mitigation row cites the source file that implements it, so claims can be verified rather than trusted. The actionable summary of this page is the [Hardening Checklist](./hardening.md).

## Overview

file-storage stores customer content, so the model treats data confidentiality and integrity as primary goals and availability as a strong secondary goal. The service ships **without a KMS and without network-level TLS on REST**, so several mitigations are explicitly the operator's responsibility — the model below states exactly which ones are built in and which must be provided by the surrounding deployment.

## How it works

### Assets

| Asset | Description | Notes |
|---|---|---|
| File content | Chunk blobs under `storage.root` (`ab/cd/<hash>.bin`) | Encrypted at rest only when `encryption.enabled` |
| Metadata / manifests | redb tables `files`, `name_index`, `tag_index` | Names, tags, sizes, chunk hashes — **never encrypted** |
| Tenant identity | redb `tenants` table, UUIDs in configs | Tenant UUID is part of key derivation — treat as immutable |
| JWT secret | `auth.jwt_secret` | Compromise = arbitrary token forgery |
| Per-tenant AES keys | `<keys_dir>/<tenant>.key` files, 0600 | No KMS integration — files must be protected by the OS |
| Audit log | redb `audit` table | Append-only; tamper-evidence comes from external backup |
| Replication `api_key` | `replication.api_key` | Guard for `/internal/*` and `ReplicationService` |

### Adversaries

| Adversary | Capability | Primary defence |
|---|---|---|
| External attacker | Network access to published ports | Auth middleware, rate limits, TLS at ingress |
| Malicious insider with valid credentials | Legitimate JWT for their tenant | Tenant scoping in every handler; audit trail |
| Compromised replica | Runs replication code, holds `api_key` | Least privilege: replica stores only what was shipped; mTLS limits forged ops |
| Curious operator with disk access | Reads disks, backups, key files | Per-tenant encryption (blobs), key file 0600; metadata remains readable — documented residual risk |
| Compromised client | Steals a user's token | Short access TTL (900 s), refresh rotation, logout |

### Attack surface

```text
 external ──► REST /api/v1/*            (JWT; rate-limited per client IP)
 external ──► gRPC :9090                (JWT interceptors; per-tenant token bucket)
 cluster  ──► REST /internal/*          (X-Api-Key; plain HTTP — firewall!)
 cluster  ──► gRPC ReplicationService   (x-api-key metadata; mTLS-capable)
 local    ──► data/blocks/, index.redb, keys/*.key, env vars / config file
```

## Configuration

### Mitigation matrix

| Threat | Mitigation | Implementation reference |
|---|---|---|
| Plaintext HTTP internal traffic sniffed inside the cluster | gRPC replication channel runs with `server.grpc_tls` + `client_ca_path` (mTLS); firewall the REST `/internal/*` routes to replication peers only | `src/grpc/server.rs`, `src/api/middleware/auth.rs` |
| JWT replay until expiry | Short access TTL (900 s default), single-use refresh rotation, `jti` claim, 5 s clock leeway | `src/security/auth.rs` |
| Tenant enumeration via dedup (cross-tenant existence oracle) | With encryption on, storage keys are `BLAKE3(tenant_id ‖ plain_hash)` — the same plaintext in two tenants lands under different keys and different ciphertexts, so nothing leaks | `src/security/encryption.rs`, `src/core/pipeline.rs` |
| Decompression bombs | Decompression is bounded by the recorded `size_original`; payloads that do not shrink are stored verbatim with an explicit `compressed=false` flag | `src/core/compressor.rs` |
| Timing-based username enumeration | Unknown usernames burn the same argon2 cost via a dummy hash; plaintext comparison is constant-time | `src/security/auth.rs` |
| Audit tampering by a compromised node/operator | Audit lives in the single redb file and is append-only from the API surface — but a disk-level attacker can edit it. Mitigate operationally: back up the database off-host and ship audit events externally | `src/security/audit.rs`, `src/storage/index_db.rs` |
| DoS via large uploads | `limits.max_file_size` (default 10 GiB), `limits.max_body` HTTP body cap, per-IP REST rate limit, per-tenant gRPC token bucket, 256 MiB gRPC message ceiling | `src/api/router.rs`, `src/core/validator.rs`, `src/grpc/server.rs` |
| Credential brute force | argon2id hashing (configurable via `password_hash`), rate limiting on both transports, login failures audited | `src/security/auth.rs`, `src/api/middleware/rate_limit.rs` |
| Cross-tenant data access | Every handler calls tenant access checks; admins alone may cross tenants; name/tag indexes are tenant-prefixed | `src/security/tenant.rs`, `src/api/middleware/tenant.rs` |
| Key file theft | Key files created `0600` at first use; runtime image is distroless/nonroot | `src/security/encryption.rs`, `Dockerfile` |
| Compromised primary serves poisoned chunk bytes (replica repair) | **M-07 verify-then-persist**: pulled bytes are decrypted + decompressed and BLAKE3-verified against the chunk reference's `plain_hash` BEFORE anything is persisted. Mismatches go to `quarantine/` (size-bounded, boot-swept), the L2 entry is invalidated, `file_storage_repair_hash_mismatch_total{source="repair"}` ticks, and the caller gets `Corruption`. Nothing lands in the CAS or cache unverified | `src/storage/retrieval.rs::repair_chunk`, `src/storage/quarantine.rs` |
| Forged replication operations | Shared `api_key` required (`x-api-key` metadata / `X-Api-Key` header), constant-time comparison, mTLS peer certificates | `src/grpc/interceptors/api_key.rs`, `src/api/middleware/auth.rs` |
| Filenames/metadata exposure on stolen disks | **Residual risk by design**: manifests, names and tags live unencrypted in redb. Mitigate with disk/host-level encryption | `src/storage/index_db.rs` |
| Panic-based crashes | Panic recovery on both stacks converts panics to error responses | `src/grpc/interceptors/recovery.rs`, `src/api/router.rs` |

## Examples

### Verifying two mitigations locally

```bash
# 1. username enumeration: same latency for unknown users
time curl -s -X POST localhost:8080/api/v1/auth/login \
  -H 'Content-Type: application/json' \
  -d '{"username":"no-such-user","password":"x"}' >/dev/null
time curl -s -X POST localhost:8080/api/v1/auth/login \
  -H 'Content-Type: application/json' \
  -d '{"username":"admin","password":"wrong"}' >/dev/null

# 2. tenant isolation: bob cannot see alice's files
BOB=$(curl -s -X POST localhost:8080/api/v1/auth/login -H 'Content-Type: application/json' \
  -d '{"username":"bob","password":"bob123"}' | jq -r .access_token)
curl -s localhost:8080/api/v1/files -H "Authorization: Bearer $BOB" | jq .total   # 0
```

### Residual risks (documented honestly)

- **Ciphertext-only replicas skip repair verification**: a replica WITHOUT the tenant key (`encryption.enabled=false`) cannot decrypt what it pulls, so it cannot verify before persisting. Those repairs keep the legacy persist-then-serve order with a loud `warn!` per event and the `file_storage_repair_ciphertext_only_total` metric — run the documented compose mode (tenants replicated WITH their keys) to get full M-07 protection. Unverifiable ≠ hostile: these bytes are NOT quarantined, they are classified as untrusted-until-read-verified.
- Refresh tokens are held in an **in-memory store**: a restart invalidates all sessions (a crude but effective logout-all), and un-flushed sessions are lost.
- The audit log is not cryptographically chained; tamper evidence requires external shipping/backup.
- Quota accounting is informational only (`file_count`, `bytes_used`) — there is no enforcement.
- The login lockout map and RPC-governor map are bounded at 10 000 entries with oldest-first eviction; a sustained flood of >10 000 distinct usernames/tenants churns entries (bounded memory, no unbounded growth).

## See also

- [Hardening Checklist](./hardening.md) — the operational countermeasures in checklist form.
- [Authentication](./authentication.md) — token lifecycle and role matrix.
- [Encryption at Rest](./encryption.md) — per-tenant AES-256-GCM design and limits.
- [Tenants](./tenants.md) — isolation guarantees and accounting.
- [Audit Log](./audit.md) — event catalog and retention.
- [Configuration reference](../configuration/reference.md) — every security-relevant knob.
