<!-- title: Secrets category: configuration audience: operator last_updated: 2026-11-10 -->

# Secrets

file-storage keeps exactly four classes of secret: the JWT signing secret, the cluster (peer) API key, per-tenant AES-256-GCM key files, and user password hashes. There is no external KMS integration — secrets live in config, environment variables or key files under `encryption.keys_dir`, so this page focuses on handling and rotating them with the mechanisms that actually exist.

## Overview

| Secret | Where it lives | Used for |
|---|---|---|
| `auth.jwt_secret` | config / `FS_AUTH__JWT_SECRET` | Signing & verifying HS256 access tokens |
| `replication.api_key` | config / `FS_REPLICATION__API_KEY` | `/internal/*` `X-Api-Key` and gRPC `x-api-key` peer auth |
| Per-tenant AES keys | `<encryption.keys_dir>/<tenant>.key` (hex, 0600) | Encrypting/decrypting blobs at rest |
| `password_hash` | `auth.users[].password_hash` | argon2id login verification |

## How it works

The JWT secret and peer API key are read once at startup and held in memory; the API key is compared with a constant-time equality check on every internal request. Tenant key files are created **on first use**: when a tenant's first blob is encrypted, 32 random bytes are written hex-encoded to `<keys_dir>/<tenant-simple>.key` with `0600` permissions (mode is enforced at creation). Key files are cached in memory per tenant after first load.

## Configuration

Relevant keys (full constraints in the [Configuration reference](./reference.md)):

- `auth.jwt_secret` — ≥ 16 chars at boot; treat as root-equivalent.
- `replication.api_key` — must be **identical on primary and replica**.
- `encryption.enabled` + `encryption.keys_dir` — when enabled, the keys directory is mandatory and must be on a persistent volume; losing it makes every encrypted blob unreadable.
- `auth.users[].password_hash` — argon2id PHC from `file-storage hash-password --password '…'`.

## Usage

### Rotating `auth.jwt_secret`

1. Generate `openssl rand -hex 32`, set it as the new `auth.jwt_secret` (or `FS_AUTH__JWT_SECRET`).
2. Restart. **All previously issued access tokens become invalid immediately** — verification uses the new secret.
3. Clients simply log in again. Refresh tokens are **not derived from the JWT secret**; those held in memory remain valid across the rotation, but they are cleared by the restart itself (the store is in-memory), so in practice a rotation-with-restart logs everyone out.

### Rotating the peer `api_key`

Both sides must agree; the key is used for `X-Api-Key` (REST `/internal/*`) and `x-api-key` (gRPC `ReplicationService`) metadata:

1. Update `replication.api_key` on the replica, restart it.
2. Update the primary and restart it. Between the two restarts replication fails (async: queued with backoff; sync: uploads get 502 `replication_error`) and resumes automatically once both sides match.

### Per-tenant AES keys

- **Naming/creation:** `<keys_dir>/<tenant-simple>.key` (UUID without dashes), hex-encoded 32 bytes, written `0600` on the tenant's first encrypted upload. Verify permissions with `key_file_permissions_ok` semantics — owner-only (`0600`) is expected.
- **Diagnostics:** the key file's modification time is exposed as `key_age_ms` (epoch ms) via the encryptor for tooling/audits.
- **Rotation is a manual procedure by design** (no KMS): the key file name is the tenant id, so a rotation means creating a *new* key file for the tenant. Old blobs were encrypted under the old key and become **unreadable unless re-encrypted**. The supported procedure:
  1. Pause writes for the tenant.
  2. Move/rename the current key file (e.g. `<tenant>.key.old`) so a fresh key is generated on next use.
  3. **Re-encrypt data**: download each file through the API with the old key still loadable (e.g. run the old key path via a temporary `keys_dir`), then re-upload it under the new key. There is no automated re-encryption command.
  4. Trigger GC (`POST /api/v1/admin/gc`) to reclaim blobs that are no longer referenced by any manifest.
  5. Restore `0600` on the new key file and keep backups of both keys until step 3 completes.

### File permissions and env caveats

- `chmod 600` on the config file and key files; run the service as a dedicated user with read access only where needed.
- Secrets in **environment variables are visible in `/proc/<pid>/environ`** to the process owner and to anyone who can inspect container runtime state, and `FS_*` values in **compose files** tend to leak via `.env` files committed to git. Prefer **mounted secret files** (K8s Secrets as volumes, docker secrets) where the deployment platform supports it; the config file itself is currently the primary secret carrier, so protect its mount.
- Never log the secret values; the service does not print config secrets, but crash logs with debug verbosity may include surrounding context.

## Examples

Locking down a production keys directory:

```bash
sudo install -d -m 700 -o file-storage -g file-storage /var/lib/file-storage/data/keys
sudo -u file-storage ./file-storage hash-password --password 'S3cret-hunter2'
sudo chmod 600 /etc/file-storage/config.yaml /etc/file-storage/tls/server.key
```

Checking key-file exposure and age:

```bash
stat -c '%a %U %n' /var/lib/file-storage/data/keys/*.key   # expect 600
ls -l --time-style=full-iso /var/lib/file-storage/data/keys/ # mtime = key_age_ms source
```

## See also

- [Configuration reference](./reference.md) — key constraints (`jwt_secret` ≥ 16 chars)
- [Production](./production.md) — the three placeholders to replace
- [Environment variables](./environment.md) — env-visibility trade-offs
- [Encryption at Rest](../security/encryption.md) — how tenant keys are applied
- [Threat model](../security/threat-model.md), [Hardening checklist](../security/hardening.md)
- [Backup & Restore](../operations/backup-restore.md) — including keys in backups
