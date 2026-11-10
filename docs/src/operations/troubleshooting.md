<!-- title: Troubleshooting category: operations audience: operator last_updated: 2026-11-10 -->

# Troubleshooting

Twenty common failure reports — symptom, diagnostic commands, fix. Deeper incident procedures (disk full, replica lag, corruption, secret rotation, key compromise) live in the [Runbook](./runbook.md); error-code semantics in [REST API](../api/rest.md) and [gRPC API](../api/grpc.md).

## Overview

Most issues fall into four buckets — auth (401/403), limits (413/416), transport quirks (gRPC metadata, compression, reflection) and storage health (redb, disk, replication) — and the error body `{"error":{"code":"...","message":"..."}}` names the bucket. The audit log (`GET /api/v1/audit`, admin) records who did what; `GET /metrics` confirms whether the server agrees with you.

## How it works

Two diagnostics work everywhere:

```bash
curl -s localhost:8080/ready     # {"status":"ok","database":"ok","storage":"ok"}
curl -s localhost:8080/metrics | grep -E 'hash_mismatch|replication_queue|gc_bytes'
```

The gRPC health service mirrors readiness and is probed every 10 s (`redb` ping + storage-root check), flipping `NOT_SERVING` within about 15 s of a real fault.

## Usage

Issue catalog — Symptom / Diagnose / Fix:

### 1. Login returns 401
**Symptom.** `POST /auth/login` → 401 `unauthorized`, or calls fail 401 after a restart.
**Diagnose.** `curl -s -X POST localhost:8080/api/v1/auth/login -d '{"username":"admin","password":"admin123"}' -H 'Content-Type: application/json'`; check audit `auth.login.failure` events; compare `auth.jwt_secret` across nodes and restarts; check the node clock (`date -u`) — JWT validation allows only 5 s of leeway for `iat`/`exp`.
**Fix.** Correct the credentials; set the same `FS_AUTH__JWT_SECRET` cluster-wide (rotation invalidates all tokens — see [Runbook](./runbook.md)); fix clock skew or move the nodes under NTP.

### 2. Upload returns 413
**Symptom.** Large upload rejected, REST code `payload_too_large`.
**Diagnose.** Compare the file size against `limits.max_body` (default 1 GiB, the body cap) and `limits.max_file_size` (default 10 GiB); both are listed in the [Configuration reference](../configuration/reference.md).
**Fix.** Raise the relevant cap via `FS_LIMITS__MAX_BODY` / `FS_LIMITS__MAX_FILE_SIZE` and restart; `max_body` also caps non-file requests.

### 3. Upload returns 403
**Symptom.** `forbidden` on `POST /files` or `Upload`.
**Diagnose.** Check the account's role: `viewer` is read-only. Also check cross-tenant behaviour: passing a `tenant_id` (or uploading into another tenant) requires the `admin` role.
**Fix.** Use a `user`-role account for writes; drop the tenant override or authenticate as admin.

### 4. Download returns 404 after delete
**Symptom.** File vanishes from default listings but disks do not shrink.
**Diagnose.** Deletes are soft: the manifest keeps `deleted_at_ms` until GC. As admin: `curl -s -H "Authorization: Bearer $A" 'localhost:8080/api/v1/files?include_deleted=true' | jq '.items[] | select(.deleted)'`.
**Fix.** Restore is not exposed — re-upload or restore from backup; wait out `retention.grace_period_secs` for space (see [GC Operations](./gc-operations.md)), or purge directly with `DELETE ?purge=true` (admin).

### 5. Range request returns 416
**Symptom.** `GET /files/:id/range` → 416 with body `bytes */size`.
**Diagnose.** The request `Range: bytes=start-end` is inclusive and end is beyond EOF only when start exceeds size; empty files (size 0) cannot satisfy any range, including suffix `-N`.
**Fix.** Clamp `start` below the file size from `/meta`; for empty files use a plain download. gRPC `Download` clamps `end` but rejects `start >= size` with `OUT_OF_RANGE`.

### 6. Conditional requests never return 304
**Symptom.** `If-None-Match` is always ignored; full body re-downloads.
**Diagnose.** `curl -sD- -o /dev/null -H "Authorization: Bearer $T" URL` and read the `ETag` header — it is `"<64-hex>"` **with quotes**. Sending the value without quotes does not match.
**Fix.** Return the ETag verbatim in `If-None-Match`, quotes included; a 304 then carries `Cache-Control: private, max-age=3600`.

### 7. gRPC calls fail UNAUTHENTICATED
**Symptom.** Every RPC after login returns `Unauthenticated`.
**Diagnose.** Intercepters read `authorization: Bearer <jwt>` metadata on unary calls; streaming RPCs verify in-handler but the metadata requirement is identical. Dump what you send: `grpcurl -v` prints outgoing headers.
**Fix.** Attach the metadata after `AuthService/Login` (SDK `login()` does this automatically; manual clients must set the header on every call, including streams).

### 8. gRPC PERMISSION_DENIED on another tenant's data
**Symptom.** Valid JWT, correct file id, `PermissionDenied`.
**Diagnose.** The token's `tenant` claim scopes every access; cross-tenant reads/writes require the `admin` role. Compare the manifest's `tenant_id` (via an admin `GetManifest`) with your claim.
**Fix.** Use an account of the owning tenant, or an admin account.

### 9. gRPC UNIMPLEMENTED mentioning gzip
**Symptom.** Client fails with `UNIMPLEMENTED` / "gzip not supported" although the server accepts gzip in both directions.
**Diagnose.** The negotiation headers disagree: a client that advertises `grpc-accept-encoding: gzip` must be able to decompress it. In Go this means importing the gzip codec (`_ "google.golang.org/grpc/encoding/gzip"`); in tonic, `accept_compressed`/`send_compressed` must be set symmetrically.
**Fix.** Register gzip client-side, or stop advertising/requesting it on one side.

### 10. Reflection lists no services
**Symptom.** `grpcurl -plaintext host list` returns nothing or errors.
**Diagnose.** Reflection is registered at boot only — check you are on the **gRPC port** (`server.grpc_port`, default 9090), not the REST port 8080; `docker compose run --rm grpcurl` proves the path works inside the network.
**Fix.** Point the client at 9090; ensure any proxy forwards HTTP/2 (h2c) rather than HTTP/1.1.

### 11. Health reports NOT_SERVING
**Symptom.** `grpc.health.v1.Health/Check` returns `NOT_SERVING` for a service or overall.
**Diagnose.** The 10 s probe pings redb and stats the storage root: check the data volume is mounted (`df -h`), the disk is writable, and only one process owns the data dir. `curl -s localhost:8080/ready` shows which component fails.
**Fix.** Remount/repair the volume, free disk, or remove the duplicate process; health flips back within ~15 s.

### 12. Replication queue depth keeps rising
**Symptom.** `replication_queue_depth` grows; `replication_failures_total` increases.
**Diagnose.** `curl -s http://replica:8081/health` for reachability; compare `FS_REPLICATION__API_KEY` on both nodes; read primary logs for `replication flush failed`.
**Fix.** Restore the target or fix the key mismatch (restart both — see [Replication Ops](./replication-ops.md)); the queue drains automatically on recovery.

### 13. Replica reads are slow
**Symptom.** First read of each file on the replica takes noticeably longer.
**Diagnose.** A fresh replica misses blobs and pulls each one from `upstream_url` during the read — per-blob latency, measured by `download_duration_seconds` with `source=disk`.
**Fix.** Preseed: re-seed via admin resync (procedure 3 in [Replication Ops](./replication-ops.md)) or rely on the L2 cache and read-ahead after the first pass.

### 14. Disk full on the primary
**Symptom.** Uploads fail; filesystem at 100%.
**Diagnose.** `df -h /var/lib/file-storage; du -sh data/blocks data/tmp`; check tombstone backlog (`include_deleted` listing) and `gc_bytes_reclaimed_total`.
**Fix.** Trigger `POST /api/v1/admin/gc`, shorten `grace_period_secs`, purge specific files with `?purge=true`, or expand the volume — full procedure in [Runbook](./runbook.md).

### 15. redb busy / locked errors
**Symptom.** Boot fails or requests 500 with `database_error` mentioning the lock.
**Diagnose.** redb is single-writer per file: two processes on the same data dir (forgotten dev server, two replicas on one volume) contend on the lock.
**Fix.** Ensure exactly one process per data directory: `pgrep -a file-storage`, `docker ps`; remove the stray process and restart.

### 16. HashMismatch / corruption errors
**Symptom.** Downloads return 500 `hash_mismatch`; `hash_mismatch_total` rises; gRPC reports `DATA_LOSS`.
**Diagnose.** BLAKE3 verification failed after a disk load — treat as corruption. Identify the affected file from audit entries and its blob path from `/meta?chunks=true` (chunk hash → `ab/cd/<hash>.bin`).
**Fix.** Follow runbook scenario 3: isolate, restore the blob from replica/backup, verify, then check hardware — fsync'd writes make disk-level tearing unlikely, so suspect RAM or device failure.

### 17. Dedup numbers are zero
**Symptom.** Re-uploading the same file reports `dedup_chunks: 0`.
**Diagnose.** Dedup is byte-exact per tenant: different content hashes differently, and with `encryption.enabled=true` the store key is `BLAKE3(tenant_id || plain_hash)`, so cross-tenant duplication can never hit.
**Fix.** Compare bytes (`sha256sum` locally) and stay within one tenant; the [GC Operations](./gc-operations.md) page explains the refcount interplay.

### 18. Tokens expire "early" on some nodes
**Symptom.** Intermittent 401s across a cluster with one shared secret.
**Diagnose.** Per-node clock skew against the 5 s JWT leeway: `date -u; curl -sI localhost:8080/health | grep -i date`.
**Fix.** Sync clocks (chrony/NTP); keep `auth.access_ttl_secs` (900) realistic for your session flow.

### 19. Container cannot write the data volume
**Symptom.** Crash loop at boot with permission denied on `/var/lib/file-storage/data`.
**Diagnose.** The distroless runtime runs as a nonroot UID (65532); a fresh named volume is root-owned.
**Fix.** Initialise ownership once: `docker run --rm -v fs_primary:/d alpine chown -R 65532:65532 /d`, or bind-mount a host directory pre-owned by that UID.

### 20. grpcurl works but the SDK fails
**Symptom.** `grpcurl -insecure` succeeds; your client gets TLS errors or connection refused.
**Diagnose.** grpcurl with `-insecure` skips verification — your SDK does not. Compare the certificate SANs (`openssl x509 -in cert.pem -text`) with the hostname/IP you dial; also confirm the SDK dials 9090 and speaks h2.
**Fix.** Dial the exact DNS name (or IP SAN) in the certificate and pass the matching CA; see [TLS](../deployment/tls.md).

## Examples

A compact first-response script that labels the four buckets:

```bash
#!/usr/bin/env bash
B=http://localhost:8080
curl -s $B/ready
curl -s -o /dev/null -w 'auth-probe=%{http_code}\n' -X POST $B/api/v1/auth/login \
  -H 'Content-Type: application/json' -d '{"username":"admin","password":"wrong"}'
curl -s $B/metrics | grep -E 'hash_mismatch_total|replication_queue_depth|gc_bytes_reclaimed_total'
```

## See also

- [Runbook](./runbook.md) — the five incident scenarios behind issues 12, 14, 16 and more.
- [GC Operations](./gc-operations.md) — deletion semantics and space reclaim.
- [Replication Ops](./replication-ops.md) — queue, backpressure and repair.
- [Configuration reference](../configuration/reference.md) — every limit and default quoted above.
- [Monitoring](./monitoring.md) — the metrics used as diagnostic evidence.
