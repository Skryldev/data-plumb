<!-- title: Tenants category: security audience: developer|architect|security-engineer last_updated: 2026-11-10 -->

# Tenants

Every file in file-storage belongs to exactly one tenant, and every request is scoped to the tenant embedded in the caller's token. This page documents the isolation guarantees, how tenants are registered and seeded, usage accounting, and the deliberate limits (notably: quotas are accounted but not enforced).

## Overview

Multi-tenancy is enforced at three layers: the name and tag indexes are **tenant-prefixed** in the redb key space, every handler re-checks that the manifest's tenant matches the caller's tenant, and audit events record the acting tenant. Admins are the single exception — they may read and manage any tenant's data, and every such event remains auditable.

## How it works

### Isolation guarantees

| Guarantee | Mechanism |
|---|---|
| Namespaces cannot collide | `name_index` keys are `tenant ‖ 0x00 ‖ name`; two tenants can store files with the same name independently |
| Tag listings are scoped | `tag_index` keys are `tenant ‖ 0x00 ‖ tag ‖ 0x00 ‖ file_id` |
| Cross-tenant reads fail | Every download/meta/delete/list call checks `manifest.tenant_id` against the token's tenant |
| Admin exception | `admin` role bypasses tenant checks (used for management and support); the check is centralised in one function |
| Encrypted dedup scoping | With encryption on, storage keys are `BLAKE3(tenant ‖ plain_hash)` — cross-tenant dedup cannot happen (see [Encryption at Rest](./encryption.md)) |

The access check is implemented in `src/security/tenant.rs` (`check_access`) and applied by the API middleware (`src/api/middleware/tenant.rs`) and each gRPC service.

### Tenant registration

Tenants enter the system through two routes:

1. **Configuration seed** — the `tenants:` list in YAML is upserted idempotently at boot (`TenantStore::seed`). This is how the development tenants `default`, `acme` and `globex` exist on a fresh node.
2. **API creation** — `POST /api/v1/tenants` (admin only, body `{"name": "..."}`) mints a new UUIDv7 tenant record. The same capability exists as `AdminService/CreateTenant` over gRPC.

A third, automatic route exists for replication: when a replica applies a manifest for a tenant it has never seen, `TenantStore::ensure` creates a placeholder record so accounting stays correct (ensure-on-apply).

### Usage accounting

Each tenant record carries `file_count` and `bytes_used`, updated **transactionally** with the same redb commit that registers or removes manifests — uploads increment, deletes and purges decrement, and the numbers cannot drift from the index. They are visible via `GET /api/v1/tenants` and `AdminService/GetTenant`.

**Quotas are not enforced.** The accounting fields are informational only; there is no `quota_bytes` key and no code path rejects an upload because a tenant is "full". If you need hard limits, gate uploads at your API gateway using the accounting numbers.

### Viewer role and tenant scoping

The `viewer` role is read-only *and* tenant-scoped: it can list, download and read metadata only for its own tenant. Cross-tenant listing requires admin regardless of role. Together with tenant-prefixed indexes this makes viewers safe to hand to external auditors or read-only integrations.

### Audit scoping

Every audit event records the acting tenant (`tenant_id` field, null for system events such as GC). Querying by tenant is possible on both transports (`AdminService/QueryAudit{tenant_id}`); see [Audit Log](./audit.md).

## Configuration

```yaml
tenants:
  - id: "01930000-0000-7000-8000-000000000001"
    name: "default"
  - id: "01930000-0000-7000-8000-000000000002"
    name: "acme"
```

Rules:

- Seeding is an idempotent upsert — existing tenants are never overwritten at boot.
- Tenant names must be 1–128 characters (validated on create).
- **Tenant UUIDs are immutable once blobs exist**: with encryption enabled they are part of key filenames and storage-key derivation, and they scope the index namespaces (see [Encryption at Rest](./encryption.md)).
- Each configured user maps to exactly one `tenant_id` (see [Authentication](./authentication.md)).

## Examples

### List isolation: bob cannot see alice's files

```bash
# alice (tenant acme) uploads a file
ALICE=$(curl -s -X POST localhost:8080/api/v1/auth/login \
  -H 'Content-Type: application/json' \
  -d '{"username":"alice","password":"alice123"}' | jq -r .access_token)
curl -s -X POST "localhost:8080/api/v1/files?name=acme-doc.txt" \
  -H "Authorization: Bearer $ALICE" --data-binary 'acme secret' | jq -r .file_id

# bob (tenant globex) sees an empty listing
BOB=$(curl -s -X POST localhost:8080/api/v1/auth/login \
  -H 'Content-Type: application/json' \
  -d '{"username":"bob","password":"bob123"}' | jq -r .access_token)
curl -s "localhost:8080/api/v1/files?limit=10" -H "Authorization: Bearer $BOB" | jq '.total, .items'
# 0
# []

# bob also cannot fetch alice's file directly
curl -s -o /dev/null -w '%{http_code}\n' -H "Authorization: Bearer $BOB" \
  "localhost:8080/api/v1/files/$FILE_ID"
# 403 (or 404 — never alice's bytes)
```

### Creating and inspecting tenants

```bash
ADMIN=$(...admin login...)
curl -s -X POST localhost:8080/api/v1/tenants \
  -H "Authorization: Bearer $ADMIN" -H 'Content-Type: application/json' \
  -d '{"name":"initech"}' | jq
curl -s localhost:8080/api/v1/tenants -H "Authorization: Bearer $ADMIN" | jq
```

### Replication ensure-on-apply

When the replica receives `replication.apply` for tenant `0193...42` which it has never seen, it creates a placeholder tenant record with the primary's UUID so `file_count`/`bytes_used` bookkeeping remains transactional on both sides. The placeholder name is a fallback label; reconcile names at the next config seed.

## See also

- [Authentication](./authentication.md) — the `tenant` claim and role matrix.
- [Encryption at Rest](./encryption.md) — tenant-scoped dedup keys.
- [Audit Log](./audit.md) — tenant-scoped audit queries.
- [Threat Model](./threat-model.md) — cross-tenant access threats and mitigations.
- [Configuration reference](../configuration/reference.md) — `tenants[]` and `auth.users[].tenant_id`.
