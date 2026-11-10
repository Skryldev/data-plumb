<!-- title: Runbook category: operations audience: operator last_updated: 2026-11-10 -->

# Runbook

Five incident scenarios with the full response path: **Symptoms → Diagnosis → Mitigation → Resolution → Prevention**. Every command assumes the Compose deployment (primary on 8080/9090, replica on 8081/9091) — substitute your hosts and credentials. Symptom-level quick fixes are in [Troubleshooting](./troubleshooting.md); metric references in [Monitoring](./monitoring.md).

## Overview

Triage order for any incident: (1) is the node ready (`GET /ready`), (2) is data safe (metrics: `hash_mismatch_total`, disk space), (3) is the cluster consistent (replication queue), (4) is access controlled (auth/audit). Mitigate first — stop bleeding (disk, corruption spread) — then resolve and only then tune prevention.

## How it works

Each scenario below is self-contained: the diagnosis commands are copy-pasteable, the mitigation is reversible where possible, and prevention names the alert or config change that stops recurrence. Config changes require a restart (no hot reload); JWT and tenant-key operations deliberately invalidate sessions — that is part of the procedure, not a bug.

## Usage

### Scenario 1 — Primary disk full

**Symptoms.** Uploads fail (500 `io_error` or 503), filesystem near 100%, `gc_bytes_reclaimed_total` flat.

**Diagnosis.**
```bash
df -h /var/lib/file-storage
du -sh /var/lib/file-storage/data/blocks /var/lib/file-storage/data/tmp
curl -s localhost:8080/metrics | grep -E 'gc_bytes_reclaimed_total|gc_files_purged_total'
# Tombstone backlog (admin):
curl -s -H "Authorization: Bearer $ADMIN" \
  'localhost:8080/api/v1/files?include_deleted=true&limit=1000' | jq '[.items[]|select(.deleted)]|length'
```

**Mitigation.** Free space at once: trigger a pass (`POST /api/v1/admin/gc`), and remove spool leftovers in `data/tmp` older than a day if the multipart spool is the offender.

**Resolution.** Shorten the grace period so the backlog purges, restart, trigger GC again, then restore the original value:
```bash
FS_RETENTION__GRACE_PERIOD_SECS=300 docker compose up -d primary
curl -s -X POST -H "Authorization: Bearer $ADMIN" localhost:8080/api/v1/admin/gc
```
For targeted space, purge known-obsolete files directly (`DELETE /api/v1/files/:id?purge=true`, admin) or expand the volume.

**Prevention.** Alert on filesystem utilisation (node_exporter) and on `FileStorageGcStalled` (`gc_bytes_reclaimed_total` flat while disk fills) from [Monitoring](./monitoring.md).

### Scenario 2 — Replica lag growing

**Symptoms.** `replication_queue_depth` climbs toward the 10000 capacity; `replication_failures_total` increases; replica `total` file count falls behind primary.

**Diagnosis.**
```bash
curl -s localhost:8080/metrics  | grep -E 'replication_(queue_depth|failures_total)'
curl -s http://replica:8081/health
docker compose exec primary printenv FS_REPLICATION__API_KEY
docker compose exec replica  printenv FS_REPLICATION__API_KEY
```

**Mitigation.** Restore the target: restart the replica container, fix a mismatched `api_key`, or repair the network path. Uploads are not failing (async mode backpressures near capacity) — do not restart the primary yet: its in-memory queue would be lost.

**Resolution.** Once flushes succeed (queue draining, failures flat), close any manifest gap: resync per file with `POST /api/v1/admin/resync/:id`, or re-seed completely ([Replication Ops](./replication-ops.md)) if the queue was lost.

**Prevention.** Alert `FileStorageReplicationQueueBacklog` (depth > 8000) and `FileStorageReplicationFailing` (failures increasing) — both in [Monitoring](./monitoring.md).

### Scenario 3 — Corruption detected

**Symptoms.** Downloads of specific files return 500 `hash_mismatch`; `hash_mismatch_total` increases; gRPC surfaces `DATA_LOSS`.

**Diagnosis.** Identify the affected file from the audit log (`GET /api/v1/audit`), then pull its chunk map to locate the stored blob:
```bash
curl -s -H "Authorization: Bearer $ADMIN" "localhost:8080/api/v1/files/$ID/meta?chunks=true" | jq
# blob path: $STORAGE_ROOT/<hash[0:2]>/<hash[2:4]>/<hash>.bin (shard_depth=2)
b3sum /var/lib/file-storage/data/blocks/ab/cd/<hash>.bin   # compare against the stored blob's key
```
Remember: for encrypted tenants the blob is ciphertext keyed by `BLAKE3(tenant_id || plain_hash)` — compare against the stored form, not the plaintext hash.

**Mitigation.** Stop writes if more files are failing (stop the process) — writes themselves are verified, but a failing disk or RAM can spread damage. Repoint reads at the replica if it is healthy.

**Resolution.** Restore the blob from the replica or backup: fetch the correct bytes (`GET /internal/chunk/:hash` with `X-Api-Key` from the replica, or `ReplicationService/PullChunk`, or your backup copy), overwrite the blob file with correct ownership, then re-download the file and confirm the hash checks out. If several files fail, restore from the last volume snapshot ([Backup & Restore](./backup-restore.md)) and resync forward. Then check hardware: blob writes are fsynced before the index commit, so torn write windows should not exist — persistent mismatches point to RAM, disk or firmware faults; run `smartctl -a`, memtest, and review controller logs.

**Prevention.** Run a periodic verification job (write your own loop: list files, download, compare `X-Content-Hash` with `b3sum`) and page on any `hash_mismatch_total` increase.

### Scenario 4 — JWT secret rotation

**Symptoms.** Planned maintenance: every access token must become invalid (suspected leak, staff departure, schedule).

**Diagnosis.** Confirm scope: which nodes share `FS_AUTH__JWT_SECRET`; how many active sessions exist (audit `auth.login.success` rate).

**Mitigation.** Choose a low-traffic window; generate the new secret (`openssl rand -base64 32`, ≥16 chars required).

**Resolution.** Rolling restart — brief re-login wave, no data risk:
```bash
openssl rand -base64 32   # new secret
# primary: set FS_AUTH__JWT_SECRET=<new>, restart; repeat on the replica
```
All access tokens signed with the old secret are rejected immediately. Refresh tokens are in-memory, so the restart clears them — users simply log in again. A blue-green switch sharing one storage directory is **not** an option: redb is single-writer per data file, so a second process cannot take over the same volume; accept the rolling restart.

**Prevention.** Schedule rotation (e.g. quarterly), keep `auth.access_ttl_secs` short (default 900) so the blast radius of any secret stays small, and store secrets in a managed vault rather than compose files.

### Scenario 5 — Tenant key compromise

**Symptoms.** A per-tenant AES-256-GCM key file (`encryption.keys_dir`) is exposed, or a tenant's credentials are.

**Diagnosis.** Confirm exposure window; review what the attacker could read or write:
```bash
curl -s -H "Authorization: Bearer $ADMIN" 'localhost:8080/api/v1/audit?limit=200' | jq
ls -l /var/lib/file-storage/data/keys/   # mode 0600 per tenant key
```

**Mitigation.** Stop the tenant's writes: rotate its user passwords in config (users are config-defined) and restart. Keys are looked up per tenant id — do **not** simply swap the key file for the same tenant: existing blobs would become undecryptable.

**Resolution.** The practical re-encryption path:
1. With the old key still in place, export the tenant's data: list its files and download every one (plaintext, as an operator or the tenant's admin).
2. Create a new tenant: `POST /api/v1/tenants {"name": "..."}` (admin) — a fresh key file is materialised for the new tenant id.
3. Re-upload the exported files under the new tenant.
4. Delete the old tenant's files (soft delete), trigger GC until `manifests_purged`/`bytes_reclaimed` settle, then remove the old tenant's key file manually so no future blob can be decrypted with it.
5. Invalidate sessions: the restarts in step 1 already cleared refresh tokens; outstanding JWTs expire within `access_ttl_secs`.
6. Complete the audit review: `GET /api/v1/audit` for `file.upload`/`file.download` anomalies during the exposure window, and record the incident against the tenant name.

**Prevention.** Restrict `keys_dir` permissions and host access, audit `auth.login.failure` spikes, and rehearse this procedure on a staging replica before you ever need it.

## Examples

Keep a one-line incident log per scenario; the template matches the structure above:

```text
2026-11-10 SC3 corruption | file 0193...abcd | blob ab/cd/9f86...bin | restored from replica 14:22Z | smartctl clean | follow-up: enable verification loop
```

## See also

- [Troubleshooting](./troubleshooting.md) — fast symptom lookup.
- [Backup & Restore](./backup-restore.md) — restore and verification procedures.
- [Replication Ops](./replication-ops.md) — resync and re-seed mechanics.
- [GC Operations](./gc-operations.md) — grace-period and purge controls used in scenarios 1 and 5.
- [Monitoring](./monitoring.md) — the alert rules referenced for prevention.
