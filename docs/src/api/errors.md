<!-- title: Errors category: api audience: developer last_updated: 2026-11-10 -->

# Errors

Every failure across both protocols comes from one unified error type, so a given condition always surfaces with the same semantics whether you called REST or gRPC. REST encodes it as an HTTP status plus a stable snake_case `code` in a JSON envelope; gRPC encodes it as a tonic status code plus the error message. This page is the complete mapping table plus debugging hints for the failures you will actually hit.

## Overview

The REST error body always has this shape (`error.code` is one of the stable strings in the table below, `error.message` is human-readable):

```json
{"error":{"code":"not_found","message":"not found: file 0193…-aa"}}
```

The gRPC equivalent is `Status{code: NOT_FOUND, message: "not found: file 0193…-aa"}` — the same Display text is used as the status message.

## How it works

Handlers return `Result<T, StorageError>`; the framework (REST `IntoResponse`, gRPC `status_from_error`) maps the variant to a transport code. 5xx outcomes are logged at `error` level with their code, 4xx at `warn`. Rate-limit rejections are the exception: on REST the tower_governor layer answers `429 Too Many Requests` directly (plain text, not the JSON envelope); on gRPC the per-tenant bucket fails unary calls with `RESOURCE_EXHAUSTED`.

## Usage

### Full error catalog

| StorageError variant | HTTP | gRPC Code | REST `code` |
|---|---|---|---|
| `NotFound` | 404 | `NOT_FOUND` | `not_found` |
| `Validation` | 400 | `INVALID_ARGUMENT` | `validation_failed` |
| `Unauthorized` | 401 | `UNAUTHENTICATED` | `unauthorized` |
| `Forbidden` | 403 | `PERMISSION_DENIED` | `forbidden` |
| `Conflict` | 409 | `ALREADY_EXISTS` | `conflict` |
| `TooLarge` | 413 | `RESOURCE_EXHAUSTED` | `payload_too_large` |
| `RangeNotSatisfiable` | 416 | `OUT_OF_RANGE` | `range_not_satisfiable` |
| `HashMismatch` | 500 | `DATA_LOSS` | `hash_mismatch` |
| `Corruption` | 500 | `DATA_LOSS` | `corruption` |
| `Compression` | 500 | `INTERNAL` | `compression_error` |
| `Encryption` | 500 | `INTERNAL` | `encryption_error` |
| `AuthBackend` | 500 | `INTERNAL` | `auth_backend_error` |
| `Io` | 500 | `INTERNAL` | `io_error` |
| `Database` | 500 | `INTERNAL` | `database_error` |
| `Serialization` | 500 | `INTERNAL` | `serialization_error` |
| `Replication` | 502 | `UNAVAILABLE` | `replication_error` |
| `Unavailable` | 503 | `UNAVAILABLE` | `service_unavailable` |
| `Internal` | 500 | `INTERNAL` | `internal_error` |

Example REST body and gRPC status message per family:

| Variant | REST example | gRPC status message example |
|---|---|---|
| `NotFound` | `{"error":{"code":"not_found","message":"not found: file <uuid>"}}` | `not found: file <uuid>` |
| `Validation` | `{"error":{"code":"validation_failed","message":"unsupported Range header 'bytes=x-y'"}}` | `validation failed: invalid uuid 'xyz'` |
| `Unauthorized` | `{"error":{"code":"unauthorized","message":"missing Authorization header"}}` | `missing authorization metadata` |
| `Forbidden` | `{"error":{"code":"forbidden","message":"admin role required"}}` | `forbidden: admin role required` |
| `Conflict` | `{"error":{"code":"conflict","message":"conflict: replication is disabled"}}` | `conflict: no upstream configured` |
| `TooLarge` | `{"error":{"code":"payload_too_large","message":"payload too large: 10737418241 > 10737418240"}}` | `payload too large: stream exceeded limits.max_file_size` |
| `RangeNotSatisfiable` | `{"error":{"code":"range_not_satisfiable","message":"range not satisfiable (size=100)"}}` | `range not satisfiable (size=100)` |
| `HashMismatch` | `{"error":{"code":"hash_mismatch","message":"hash mismatch: expected ab…, got cd…"}}` | `hash mismatch: expected ab…, got cd…` |
| `Corruption` | `{"error":{"code":"corruption","message":"corruption: chunk blob truncated"}}` | `corruption: chunk blob truncated` |
| `Compression` | `{"error":{"code":"compression_error","message":"compression error: zstd decode failed"}}` | `compression error: zstd decode failed` |
| `Encryption` | `{"error":{"code":"encryption_error","message":"encryption error: decryption failed (wrong key or tampered data)"}}` | `encryption error: decryption failed (wrong key or tampered data)` |
| `AuthBackend` | `{"error":{"code":"auth_backend_error","message":"authentication backend error: stored hash invalid"}}` | `authentication backend error: …` |
| `Io` | `{"error":{"code":"io_error","message":"io error: No such file or directory"}}` | `io error: …` |
| `Database` | `{"error":{"code":"database_error","message":"database error: redb read transaction failed"}}` | `database error: …` |
| `Serialization` | `{"error":{"code":"serialization_error","message":"serialization error: bincode decode at offset 12"}}` | `serialization error: …` |
| `Unavailable` | `{"error":{"code":"service_unavailable","message":"service unavailable: redb ping failed"}}` | `service unavailable: redb ping failed` |
| `Replication` | `{"error":{"code":"replication_error","message":"replication error: target connect timeout"}}` | `replication error: target connect timeout` |
| `Internal` | `{"error":{"code":"internal_error","message":"internal error: body read: unexpected EOF"}}` | `internal error: …` |

## Examples

### Common scenarios and debugging hints

- **File not found (404 / `NOT_FOUND`)** — Files are tenant-scoped: a valid UUID from another tenant reads as 404 unless you are admin with cross-tenant access. **Soft-deleted files also read as 404** on normal reads; list with `include_deleted=true` (admin) to see them, or check `GET /files/:id/meta` after a same-name overwrite (the old file is soft-deleted).
- **Quota errors (409/413 "quota")** — There is **no quota enforcement**: `Tenant.quota_bytes` is reserved and always reported as 0 (= unlimited). The only size caps are `limits.max_file_size` (per upload, default 10 GiB) and `limits.max_body` (HTTP body, default 1 GiB). If you see 413, compare your payload against those two values.
- **Rate limited** — REST: HTTP 429 from the governor when `rate_limit.enabled=true`, keyed by client IP (`rate_limit.per_second`/`burst`). Behind a proxy, every client shares the proxy's bucket — see [Troubleshooting](../operations/troubleshooting.md). gRPC: `RESOURCE_EXHAUSTED` on unary RPCs from the per-tenant bucket; **streaming RPCs are never rate limited**. Disable via `FS_RATE_LIMIT__ENABLED=false` for load tests.
- **Checksum mismatch (`hash_mismatch`, 500 / `DATA_LOSS`)** — The read path BLAKE3-verifies every chunk loaded from disk; a mismatch means **on-disk corruption**, not a client bug. Check `file_storage_hash_mismatch_total` in [Monitoring](../operations/monitoring.md), then follow the recovery steps in the [Runbook](../operations/runbook.md) (re-replicate or restore from backup; the affected blob must not be served).
- **Replication failed (`replication_error`, 502 / `UNAVAILABLE`)** — The primary could not reach the target. Check `replication.target_url` and `replication.api_key` match the replica, network/DNS, and `file_storage_replication_failures_total` / `replication_queue_depth`. In sync mode the upload itself fails with 502; in async mode it is queued (backpressure at `queue_capacity`).
- **Validation errors (400 / `INVALID_ARGUMENT`)** — the usual suspects: a file id that is not a 36-char UUID; a `Range` header not matching `bytes=start-end|start-|-N` (multi-range takes the first range); a hash that is not exactly **32 bytes / 64 hex chars** on `/internal/*` endpoints; a multipart body without a `file` field; names/tags metadata fields over 4096 bytes.

Retry guidance: 429 and `RESOURCE_EXHAUSTED` (rate limit) are retryable after backoff; 500/`DATA_LOSS` are not; 502/`UNAVAILABLE` from replication are safe to retry once the target recovers. The Rust SDK retries unary calls on `Unavailable`/`DeadlineExceeded`/`Aborted` (3 attempts, 100 ms→2 s).

## Configuration

Error behavior is not directly configurable, but several knobs change which errors occur: `limits.*` (413), `rate_limit.*` (429/`RESOURCE_EXHAUSTED`), `replication.*` (502/409), `retention.*` (whether purged files disappear). See the [Configuration reference](../configuration/reference.md).

## See also

- [REST](./rest.md) — per-endpoint error lists
- [gRPC](./grpc.md) — per-RPC status codes
- [Authentication](./authentication.md) — 401 vs 403 semantics
- [Troubleshooting](../operations/troubleshooting.md), [Runbook](../operations/runbook.md)
- [Monitoring](../operations/monitoring.md) — error metrics
- [Configuration reference](../configuration/reference.md)
