<!-- title: REST API category: api audience: developer last_updated: 2026-11-10 -->

# REST API

The REST API is the primary HTTP interface of file-storage, served by axum on `server.port` (default `8080`) under the base path `/api/v1`. Every request and response uses JSON (except binary uploads/downloads), and every failure returns the same `{"error":{"code","message"}}` envelope. This page documents every endpoint, its authentication requirement, request/response schemas, and every error it can return.

## Overview

| Status | Meaning | Typical `error.code` |
|---|---|---|
| 200 | OK — login, refresh, list, meta, admin results | — |
| 201 | Created — upload, tenant creation | — |
| 204 | No Content — delete, logout, internal purge | — |
| 206 | Partial Content — range download | — |
| 304 | Not Modified — `If-None-Match` matched | — |
| 400 | Bad request — bad JSON, UUID, Range or hash | `validation_failed` |
| 401 | Missing/invalid `Authorization` or `X-Api-Key` | `unauthorized` |
| 403 | Role or tenant scope violation | `forbidden` |
| 404 | Unknown file or tenant | `not_found` |
| 409 | Conflicting state — e.g. resync with replication disabled | `conflict` |
| 413 | Body exceeds `limits.max_body` / `limits.max_file_size` | `payload_too_large` |
| 416 | Range unsatisfiable (`Content-Range: bytes */size`) | `range_not_satisfiable` |
| 500 | Server-side failure (corruption, IO, database, …) | `hash_mismatch`, `corruption`, `io_error`, `database_error`, … |
| 502 | Replication target failed | `replication_error` |
| 503 | Not ready (probe) or degraded dependency | `service_unavailable` |

## How it works

Requests flow through middleware in order: request-id assignment → body size cap (`DefaultBodyLimit` = `limits.max_body`) → rate limiting → auth. When `rate_limit.enabled` is true, a `tower_governor` token bucket keyed by **client IP** guards all of `/api/v1` (`rate_limit.per_second` / `rate_limit.burst`); over-limit callers receive `429 Too Many Requests` (not part of the `StorageError` catalog — it is emitted by the governor layer). Requests behind a reverse proxy are keyed by proxy IP; see [Troubleshooting](../operations/troubleshooting.md).

Authentication is `Authorization: Bearer <jwt>` for `/api/v1` (except `/auth/*`, which is public) and `X-Api-Key` for `/internal/*`. `/health`, `/ready` and `/metrics` are public. Successful uploads are content-defined chunked, hashed (BLAKE3), compressed and optionally encrypted; **uploading a file under a name that already exists in your tenant replaces the previous file — the old manifest is soft-deleted and stays visible only with `include_deleted=true` (admin)**. Deletes are soft by default and purged by GC after `retention.grace_period_secs`.

## Usage

### POST /api/v1/auth/login — public

Body `{"username":"admin","password":"admin123"}` → `200` `{"access_token","refresh_token","token_type":"Bearer","expires_in":900}`. Errors: `unauthorized` (401), `validation_failed` (400).

### POST /api/v1/auth/refresh — public

Body `{"refresh_token":"..."}` → `200` same shape; the presented token is revoked (single-use rotation). Errors: 401, 400.

### POST /api/v1/auth/logout — public

Body `{"refresh_token":"..."}` → `204`, idempotent. Revokes the refresh token; the access token stays valid until `exp` (stateless).

### POST /api/v1/files — Bearer, write role

Two accepted forms:

- **multipart** (`multipart/form-data`): required `file` field plus optional `name`, `tags` (comma-separated), `mime`/`content_type` text fields (each ≤ 4096 bytes). The file field is spooled to `storage.temp_dir`.
- **raw body**: any content type; metadata from query string `?name=&mime=&tags=a,b`. Content-Type becomes the MIME when `mime` is unset.

Returns `201`:

```json
{"file_id":"01930000-0000-7000-8000-0000000000aa","name":"report.pdf","mime":"application/pdf",
 "size":1048576,"size_stored":524288,"chunk_count":8,"content_hash":"<64-hex blake3>",
 "dedup_chunks":3,"dedup_bytes":393216,"created_at_ms":1730000000000}
```

Size is checked early against `Content-Length` and again while streaming (`limits.max_file_size`, default 10 GiB). Errors: 400 (bad multipart, missing `file`), 401, 403 (viewer role), 413.

### GET /api/v1/files — Bearer

| Query param | Type | Default | Notes |
|---|---|---|---|
| `limit` | int | 50 | clamped to 1..1000 |
| `offset` | int | 0 | pagination skip |
| `tag` | string | — | takes precedence over `name`; exact match |
| `name` | string | — | exact match |
| `include_deleted` | bool | false | **admin only** |

→ `200` `{"total","limit","offset","items":[{file_id,name,mime,size,size_stored,chunk_count,tags,created_at_ms,deleted}]}`. Errors: 401, 403 (`include_deleted` without admin).

### GET /api/v1/files/:id — Bearer

Downloads the full file. Headers: `ETag: "<64-hex>"`, `Cache-Control: private, max-age=3600`, `Content-Disposition: inline; filename="…"`, `X-Content-Hash`. `If-None-Match` matching the ETag → `304` (no body). Errors: 400 (bad UUID), 401, 403 (cross-tenant without admin), **404 — soft-deleted files read as 404**.

### GET /api/v1/files/:id/range — Bearer

`Range` header **required**: `bytes=start-end` (inclusive end), `bytes=-N` (suffix), open end `bytes=10-`. **Multi-range requests take the first range only.** End is clamped to the file size. → `206` with `Content-Range: bytes start-end/size`; unsatisfiable start → `416` with `Content-Range: bytes */size`. Missing/invalid header → 400. Also 401/403/404.

### GET /api/v1/files/:id/meta?chunks=true|false — Bearer

→ `200` `FileMetaDto`: `file_id,name,tenant_id,mime,size,size_stored,content_hash,chunk_count,tags,created_at_ms,updated_at_ms,deleted,version,href`, plus optional `chunks:[{offset,size_original,size_stored,hash,compressed,encrypted}]` (first 1000 chunks; `chunks_truncated=true` if more). Errors: 400/401/403/404.

### DELETE /api/v1/files/:id?purge=true|false — Bearer

Soft delete (default) → `204`; sets `deleted_at_ms`, keeps blobs until GC. `purge=true` is **admin only**: removes the manifest, decrements chunk refcounts and deletes blobs reaching zero. Errors: 400, 401, 403 (purge without admin; any delete without write role), 404.

### POST /api/v1/tenants, GET /api/v1/tenants — Bearer, admin

Body `{"name":"acme"}` → `201` `TenantDto {id,name,created_at_ms,file_count,bytes_used}`; GET → array. Errors: 401, 403.

### GET /api/v1/audit?limit= — Bearer, admin

Most recent events first, `limit` clamped 1..1000 (default 100) → array of `{id,ts_ms,actor,tenant_id,action,object,ip,status,detail}`. Errors: 401, 403.

### POST /api/v1/admin/gc — Bearer, admin

Runs one GC pass → `200` `{manifests_purged,chunks_removed,orphans_removed,bytes_reclaimed}`. Errors: 401, 403, 500.

### POST /api/v1/admin/resync/:id — Bearer, admin

Re-pushes a file (manifest + chunk blobs) to the replication target → `200` `{file_id,replicated:true}`. Errors: 401, 403, 404, **409 when replication is disabled / no target configured**, 502.

### Probes — public

- `GET /health` → `200` `{status,version,uptime_secs}`.
- `GET /ready` → `200` `{status:"ready",database:true,storage:true}` or `503` with `status:"degraded"` and failing booleans.
- `GET /metrics` → `200` Prometheus text (see [Monitoring](../operations/monitoring.md)).

### /internal/* — X-Api-Key

Peer replication endpoints, guarded by a constant-time comparison against `replication.api_key` (not JWT): `POST /internal/chunk?hash=&plain_hash=&size_original=&size_stored=&compressed=&encrypted=` (raw blob body), `GET /internal/chunk/:hash` (raw blob), `POST /internal/manifest` (msgpack manifest), `DELETE /internal/manifest/:id`, `POST /internal/batch` (chunk frame batch), `GET /internal/file/:id` (bundle), `POST /internal/file/:id` (apply bundle). Internal transfers have a 2 GiB ceiling; writes are idempotent. Errors: 401 (missing/invalid key), 400 (hash must be 64 hex chars), 404, 500.

## Examples

```bash
TOKEN=$(curl -s localhost:8080/api/v1/auth/login \
  -d '{"username":"alice","password":"alice123"}' | jq -r .access_token)

curl -s -H "Authorization: Bearer $TOKEN" \
  -F file=@report.pdf -F name=report.pdf -F tags=reports,q3 \
  localhost:8080/api/v1/files

curl -s -H "Authorization: Bearer $TOKEN" -H "Range: bytes=0-1023" \
  -o part.bin localhost:8080/api/v1/files/<file_id>/range
```

A 400 response looks like:

```json
{"error":{"code":"validation_failed","message":"unsupported Range header 'bytes=x-y'"}}
```

## See also

- [gRPC](./grpc.md) — the same operations over HTTP/2 streaming
- [Errors](./errors.md) — full error catalog and debugging hints
- [Authentication](./authentication.md) — tokens, refresh rotation, roles
- [Examples](./examples.md) — copy-paste snippets in four languages
- [Configuration reference](../configuration/reference.md) — keys used above
- [CLI recipes](../integration/cli.md), [Rust client](../integration/rust-client.md)
- [Quickstart](../getting-started/quickstart.md), [Threat model](../security/threat-model.md)
