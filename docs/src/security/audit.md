<!-- title: Audit Log category: security audience: security-engineer|operator|developer last_updated: 2026-11-10 -->

# Audit Log

file-storage records a structured, append-only audit trail for every security-relevant action: file uploads, downloads, deletes and purges, logins, tenant creation, GC runs and replication activity. This page documents the event format, the full action catalog, how to query the log on both transports, and the retention properties — including what the audit log deliberately does not contain.

## Overview

Events are persisted as JSON rows in the redb `audit` table, keyed by **UUIDv7** so the table scan order is chronological. Writing is fire-and-forget with a safety valve: if persisting an event fails, the error is logged and counted in metrics but **never fails the user's request** — audit problems degrade observability, not availability.

## How it works

### Event format

| Field | Type / values | Meaning |
|---|---|---|
| `id` | UUIDv7 | Time-ordered primary key |
| `ts_ms` | epoch millis | Event timestamp |
| `actor` | `system` or username | Who acted (`system` for GC, replication, etc.) |
| `tenant_id` | UUID or null | Tenant scope of the action |
| `action` | string from the catalog below | What happened |
| `object` | string | Target of the action (file id, tenant id, ...) |
| `ip` | IP string or `-` | Client address (where applicable) |
| `status` | `ok`, `failure`, HTTP-ish status labels | Outcome |
| `detail` | string | Short, structured extras (e.g. `l1 hit`, byte counts) |

### Where it lives

The audit trail is stored in the **same redb database file as the index** (`database.path`, default `data/index.redb`), in the `audit` table. It is append-only through the API surface: there is no endpoint that updates or deletes audit rows. Consequence: the audit log shares the fate of the data disk — for tamper evidence beyond a compromised-disk adversary, **back up the database off-host and/or ship audit events to an external collector** (see the hardening checklist).

### Retention

There is **no automatic rotation or pruning** of the audit table. It grows with request volume. Manage retention operationally:

- Periodically export the audit table (admin query API in a loop, or a copy of the redb file) and archive it externally.
- Size the data volume with audit growth in mind for your request rate.
- Restore-point your backups if you need historical queries — see the Backup & Restore page in the Operations section.

## Configuration

### Query API

| Transport | Call | Access |
|---|---|---|
| REST | `GET /api/v1/audit?limit=N` → `[AuditEntryDto]` | admin only |
| gRPC | `AdminService/QueryAudit{limit, tenant_id}` → `{entries, total}` | admin only |

`limit` caps the number of returned events (most recent first). The gRPC variant accepts an optional `tenant_id` filter; over REST, filter client-side after retrieval.

### What is NOT logged

- **Response bodies** — no file bytes ever pass through the audit trail.
- **File content** — only ids, names-as-object-labels and counts appear in `detail`.
- No request headers beyond the client IP; tokens and passwords are never recorded.

### Failure behaviour

If the redb write fails (disk full, IO error), the service logs the error (`failed to persist audit event`), increments the `file_storage_audit_events_total{action}` counter for the attempted action, and continues serving the request. Monitoring that metric against your request rate reveals audit-logging health.

## Examples

### Query the trail (REST)

```bash
TOKEN=$(... admin login ...)
curl -s "localhost:8080/api/v1/audit?limit=5" -H "Authorization: Bearer $TOKEN" | jq
```

Example output (abbreviated):

```json
[
  {
    "id": "01930000-0000-7000-8000-0000000000e1",
    "ts_ms": 1762700012345,
    "actor": "alice",
    "tenant_id": "01930000-0000-7000-8000-000000000002",
    "action": "file.upload",
    "object": "01930000-0000-7000-8000-0000000000ab",
    "ip": "10.0.0.7",
    "status": "ok",
    "detail": "chunks=3 dedup=0"
  }
]
```

### Query the trail (gRPC)

```bash
grpcurl -plaintext -H "authorization: Bearer $ADMIN_JWT" \
  -d '{"limit": 10, "tenant_id": "01930000-0000-7000-8000-000000000002"}' \
  localhost:9090 file_storage.v1.AdminService/QueryAudit
```

### Action catalog

| Action | Emitted when |
|---|---|
| `file.upload` | A manifest is committed (either transport) |
| `file.download` | A download completes, including 304 and range responses (status carries the outcome, detail e.g. `l1 hit`) |
| `file.delete` | A soft delete tombstones a manifest |
| `file.purge` | An admin hard-purges a manifest |
| `auth.login.success` | Valid credentials issued a token pair |
| `auth.login.failure` | Wrong password or unknown user (both look identical from outside) |
| `tenant.create` | A tenant is created via the API |
| `replication.apply` | The replica applies shipped operations |
| `storage.gc` | A GC pass runs (actor `system`) |
| `replication.resync` | An admin triggers a full re-push of a file bundle |

The metric `file_storage_audit_events_total{action}` counts every emitted event, so you can alert on unexpected spikes (for example a burst of `auth.login.failure` indicating credential stuffing).

## See also

- [Authentication](./authentication.md) — the login lifecycle behind `auth.login.*` events.
- [Tenants](./tenants.md) — tenant scoping of events and queries.
- [Threat Model](./threat-model.md) — audit tampering threats and the off-host mitigation.
- [Hardening Checklist](./hardening.md) — audit export and alerting rows.
- [REST API](../api/rest.md) — the `AuditEntryDto` shape.
- [gRPC API](../api/grpc.md) — `AdminService` details.
