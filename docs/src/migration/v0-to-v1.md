<!-- title: Migration v0 → v1 category: migration audience: all last_updated: 2026-11-10 -->

# Migration v0 → v1

Version 1 is the first release with the gRPC listener running alongside the REST API, and the upgrade is deliberately boring: the on-disk formats are unchanged, the REST API is unchanged, and every new configuration key is additive with a default. If you were running a pre-release v0 build, the migration is "back up, stop the old binary, start the new one on the same data directory."

## Overview

What v1 adds on top of v0:

- **gRPC listener** on `server.grpc_port` (default `9090`) — same process, same state; see [gRPC](../api/grpc.md) and [ADR 0008 — gRPC alongside REST](../architecture/decisions/0008-grpc-alongside-rest.md).
- **`server.grpc_tls`** config block (server certificate, optional client CA for mTLS).
- **`ReplicationService` over gRPC** — additive; the built-in WAL shipper still uses the REST `/internal/*` endpoints by default, and both are guarded by the same `replication.api_key`.
- **gRPC health + reflection** services for probes and grpcurl.
- **Behavioral change:** uploading under an existing name now **replaces the previous file** — the old manifest is soft-deleted and remains visible in listings only with `include_deleted=true` (admin).

Metadata and audit semantics are unchanged: audit actions, event fields, tenant scoping and the error catalog mean the same thing on both transports.

## How it works

**Config compatibility.** The config struct deserializes with serde defaults, so an existing v0 YAML file keeps working untouched: missing keys (`server.grpc_port`, `server.grpc_tls`, …) are filled with defaults (`9090`, TLS off). All new keys are additive; nothing was renamed or removed. See the [Configuration reference](../configuration/reference.md).

**Wire-format compatibility.** v0 adopters were pre-release, and the redb schema has not changed since: tables (`chunks`, `files`, `name_index`, `tag_index`, `audit`, `tenants`), the block-store layout (`storage.root` shards of 64-hex blobs) and manifests are identical. There is no data-conversion step.

**REST compatibility.** Paths, request/response shapes, error codes and status semantics of `/api/v1` and the probes are byte-for-byte what they were. The only observable REST-level difference is the same-name-replacement behavior above.

## Configuration

New keys you may want to set after upgrading:

| Key | Default | Notes |
|---|---|---|
| `server.grpc_port` | 9090 | Exposing the port is **opt-in**: inside the container image it is `EXPOSE`d, but nothing listens on it externally until you publish/firewall it |
| `server.grpc_tls.enabled` | false | Turn on with `cert_path`/`key_path` (mTLS adds `client_ca_path`) |
| `replication.*` | unchanged | The gRPC `ReplicationService` reuses `replication.api_key` — no new secret |

## Usage

Upgrade procedure (assumes a single binary deployment; containers do the same dance with image + restart):

1. **Back up first** — stop nothing yet; take a filesystem-level backup of `storage.root`, `database.path`, and `encryption.keys_dir` (the keys are mandatory backups when encryption is on). See [Backup & Restore](../operations/backup-restore.md).
2. **Stop the old v0 binary.** In-flight uploads abort; queued replication ops live in memory and are lost — re-push anything important with resync after the upgrade.
3. **Start the new binary on the same data directory**, pointing at your existing YAML:

```bash
cp -a data data.bak.$(date +%F)          # 1. backup (or snapshot the volume)
systemctl stop file-storage              # 2. stop v0
./file-storage-v1 config/default.yaml    # 3. start v1 on the same data dir
curl -s localhost:8080/ready             # 4. verify probes
grpcurl -plaintext localhost:9090 grpc.health.v1.Health/Check
```

4. **Verify**: `/health`, `/ready` both healthy; upload a test file, list it, download it, delete it; confirm the gRPC listener answers (health check above); confirm replication is shipping (`file_storage_replication_ops_applied_total` in [Monitoring](../operations/monitoring.md)).
5. Publish or firewall `grpc_port` deliberately — do not expose it accidentally.

### Rollback

Because no data format changed, rollback is symmetric: stop the v1 binary and start the v0 binary on the same data directory. Two caveats: (a) files created under v1 are ordinary manifests and blobs, readable by v0; (b) soft-deleted "replaced" files are hidden from v0 listings exactly like other soft deletes; (c) if you enabled encryption or changed retention under v1, revert those config keys to the v0 values *before* starting the old binary, and remember refresh tokens from the v1 process are gone (they were in memory). If anything looks wrong, restore the backup from step 1 instead.

## Examples

v0 YAML (still valid under v1, unchanged):

```yaml
server:
  host: "0.0.0.0"
  port: 8080
storage:
  root: "data/blocks"
auth:
  jwt_secret: "change-me-in-production-0123456789abcdef"
  users:
    - username: admin
      password: "admin123"
      tenant_id: "01930000-0000-7000-8000-000000000001"
      role: admin
```

Same file after opting into gRPC with TLS:

```yaml
server:
  host: "0.0.0.0"
  port: 8080
  grpc_port: 9090
  grpc_tls:
    enabled: true
    cert_path: "/etc/file-storage/tls/server.crt"
    key_path: "/etc/file-storage/tls/server.key"
```

Checking that replaced files still exist (admin only):

```bash
curl -s -H "Authorization: Bearer $ADMIN" \
  'localhost:8080/api/v1/files?name=report.pdf&include_deleted=true'
```

## See also

- [ADR 0008 — gRPC alongside REST](../architecture/decisions/0008-grpc-alongside-rest.md)
- [gRPC](../api/grpc.md), [REST](../api/rest.md) — what v1 adds to each surface
- [Configuration reference](../configuration/reference.md) — new keys and defaults
- [Secrets](../configuration/secrets.md) — key files that must be backed up
- [Backup & Restore](../operations/backup-restore.md)
