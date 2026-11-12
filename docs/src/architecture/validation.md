# Request Validation Contract

> Status: normative since V5 Wave 2 (audit finding M-04). Enforced by CI
> rule `no-handler-validation` (`.github/workflows/ci.yml` +
> `scripts/check_no_handler_validation.sh`).

## The rule

There is **exactly one place** that decides whether an upload request is
well-formed: `src/core/validation.rs`. REST and gRPC handlers reduce to
three steps:

1. **Extract** fields from the wire type (`Request<Body>` query/multipart
   fields, or the gRPC `UploadHeader`) into an `UploadRequestView`.
2. **Validate** by calling `validation::validate_upload(&view, &limits)`.
   No handler contains validation *logic* — only this call.
3. **Process** by handing the already-validated fields to the upload
   pipeline (whose head separately enforces magic-byte/MIME content
   checks — audit finding H-03 — and the size ceiling during streaming).

Transport-level code that appears to need a new validation rule is a
design smell: the rule belongs in `core::validation`, where both
transports inherit it automatically. CI fails the build if validation
logic (`validate_name|validate_tags|validate_mime|check_size|len() </> NNN`)
re-appears under `src/api/` or `src/grpc/services/`.

## The rules themselves

| # | Rule | Closes |
|---|---|---|
| 1 | `name`: 1..=255 **bytes** after trim; C0 (`0x00-0x1F`) and C1 (`0x7F-0x9F`) control characters rejected. V4 §5.5 says "chars"; the byte cap is the conservative encoding-safe superset (chars ≤ bytes). | M-04b |
| 2 | `tags`: at most 32 tags, each 1..=64 bytes after trim, no control characters. | L-06 |
| 3 | `mime`: at most 128 bytes, no control characters. Content vs. magic-byte parity is the pipeline head's job (H-03). | M-04 helpers |
| 4 | `expected_size`, when present and non-zero: ≤ `limits.max_file_size`. Checked **before** admission control on both transports, so an over-cap declaration is a clean `400 Bad Request` / `INVALID_ARGUMENT`, never a budget `413` / `RESOURCE_EXHAUSTED`. REST gained the optional `?expected_size=` query param in the same move. | M-04e |
| 5 | `include_deleted=true` requires the admin role — refused with `403 Forbidden` / `PERMISSION_DENIED` on **both** transports (no silent coercion). | M-04a |
| 6 | Chunk lists served by read endpoints are capped at 1000 entries with an explicit `chunks_truncated` flag (REST meta DTO and gRPC `GetManifest`); replication transfers are never truncated. | M-04c |
| 7 | List `total` values come from one shared implementation
(`IndexDb::list_filtered`), so REST and gRPC totals cannot drift. | M-04d |

## Error mapping parity

| Validation failure | REST | gRPC |
|---|---|---|
| name / tags / mime / expected_size | `400 Bad Request` (`validation_failed`) | `INVALID_ARGUMENT` |
| `include_deleted` without admin | `403 Forbidden` (`forbidden`) | `PERMISSION_DENIED` |

The mapping is the standard `StorageError` table
(`error.rs::status_code`, `grpc::proto_auth::status_from_error`); the
validation layer only produces `Validation` and `Forbidden` variants.

## What does NOT live here

* Magic-byte / declared-MIME content checks — `core::validator`
  (`FileValidator::validate_head`), run by the pipeline head (H-03).
* Size ceilings during streaming — `FileValidator::validate_size` plus the
  H-01 `UploadGate` byte budget.
* AuthN / authZ — `security::auth` and the middleware/interceptors.
* Cross-tenant access — `api::middleware::tenant`.

## Regression gates

`tests/regression/m04_validation.rs` pins the contract:

* `parity_validation_matrix` — one table, both transports, identical
  verdicts for six malformed-input classes.
* `viewers_cannot_list_deleted_both_transports` — 403 / PERMISSION_DENIED
  parity, admins unaffected.
* `list_total_matches_rest` — same `total` from both transports, page
  smaller than the match set.
* `rest_accepts_expected_size_query` — accepted in-range, 400 over-cap.
* `getmanifest_truncates_chunks` — 1000-entry cap + `chunks_truncated`.
