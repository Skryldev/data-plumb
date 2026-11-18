# Design: Storage Format v2 (metadata engine inside redb)

Status: APPROVED (V9 PG-1 precondition, contract §2.0; satisfies V8
condition C-7). Implementation commits reference this document.

## 1. Goals

Versioned, cursor-friendly, quota-correct metadata inside redb (D1 — the
engine stays; the schema is what changes), closing the lineage tickets
M-16, V6-08+M-03, V6-10 as part of the rewrite rather than as side
patches.

## 2. Schema version gate (P5)

- redb key `schema/version` (table `meta`), value = u16 little-endian.
- v1 dirs: no version key ⇒ treated as version 1.
- Boot matrix:
  | dir version | binary knows | behavior |
  |---|---|---|
  | absent/1 | 2 | refuse boot with actionable error naming `xtask migrate`, UNLESS `migration.auto=true` (default false) → migrate inline |
  | 2 | 2 | open |
  | >2 | 2 | refuse: "data dir schema v{n} is newer than this binary (supports v{known}) — upgrade the binary" (V8 §7.7 rollback policy: old code NEVER opens newer dirs) |
- The gate runs BEFORE any write surface binds (see boot.md ordering).

## 3. v2 trees

| tree | key | value |
|---|---|---|
| `meta` | `schema/version` | u16 LE |
| `manifests_v2` | tenant_uuid(16) ‖ 0x00 ‖ path | manifest v2 record (below) |
| `summary` | same key | {size u64, chunk_count u32, content_type, mtime_ms i64, tags_hash u64} — the ONLY tree list endpoints read (M-03/V6-08: O(page) lists) |
| `index_by_mtime` | tenant ‖ mtime_ms(8 BE) ‖ path | empty (ordering index) |
| `index_by_name_ci` | tenant ‖ lowercase(path) ‖ path | empty |
| `chunk_refs` | hash(32) | {refcount u64, size u64} (v1 `chunks` + path removed: blob location is derived from the hash — shard layout) |
| `quotas` | tenant | {committed_bytes u64, committed_files u64} — commit-time re-check source |
| `pipeline_jobs` | job_id(16) | reserved layout documented now, activated PG-3 |
| `audit_watermarks` | reserved | activated PG-4 |

v1 trees (`files`, `name_index`, `tag_index`, `chunks`, `tenants`) remain
READABLE during the migration window (dual-read) and are removed by the
final cutover xtask step. The v1 `tenants` tree is kept as-is (already
adequate; quotas tree adds the commit-time counters).

### Manifest v2 record

bincode, fixed field order, with a trailing `record_crc u32` (crc32c over
the preceding canonical bytes — a corrupted record is DETECTABLE at read,
contract §2.1):

```
tenant(16) path(len-prefixed) content_hash(32) total_size(u64)
version_id(16) seq(u64) is_current(u8)          // reserved, PG-3 activates
durability_class(u8)                             // 0=durable 1=batched
created_ms(i64) updated_ms(i64) deleted_ms_opt(i64+flag)
chunk_refs: [(hash32, plain_hash32, offset u64, size_orig u64, size_stored u64, flags u8)]
record_crc u32
```

Legacy `version: u64` field maps to `seq`; `file_id` is superseded by
`(tenant, path)` addressing (v1 id kept in a migration side-table for the
dual-read window only).

## 4. Cursor lists (M-03/V6-08)

Cursor = base64url(`direction u8 ‖ last_key_bytes`) — opaque to clients.
Range scan on `summary` (or an index tree when `sort=mtime|name`);
`limit` clamped 1..=1000. Every list endpoint: REST `?cursor=&limit=`,
gRPC `cursor` field; response carries `next_cursor` (empty = end).
Cost is O(page) regardless of tenant size — gate
`list_cursor_cost_is_page_size`.

## 5. Quotas — commit-time re-check (V8 §7.5)

Reserve at the admission gate (existing UploadGate); at MANIFEST COMMIT
the redb txn re-checks `quotas` against `limits.max_quota_*` when a
tenant quota is configured; overshoot → abort the txn (WAL txn never
commits) and return 413-class. Gate `quota_commit_race_cannot_overshoot`.

## 6. Migration (P5, resumable, verified)

`xtask migrate --dry-run|--commit|--verify`:

1. **prepare**: open v1 trees read-only; create v2 trees; write
   `meta/migration_state = {phase, last_key, counts}` (checkpoint every
   500 records — resume re-reads the state).
2. **copy** (per record, atomic): v1 manifest → v2 record + summary row
   + both indexes + chunk_refs; quotas derived by fold; mtime/name_ci
   populated. A failed record is logged with its key and retried on
   resume — never silently skipped.
3. **verify**: re-walk v1 and v2; compare counts, sizes, per-record CRC;
   any delta = non-zero exit naming the key (gate
   `migration_verify_catches_dropped_record`).
4. **cutover**: set `schema/version = 2`. `--dry-run` runs steps 1–3
   against a throwaway in-memory/cloned dir and writes no state.

Dual-read: while `schema/version == 1` and v2 trees exist, reads prefer
v2 rows and fall back to v1 (migration window only). Boot refuses v1
unless `migration.auto=true`.

## 7. M-16 — scoped dedup keys (security closure, V8 C-6)

Chunk addressing key = `dedup_key(tenant, plain_hash)`:

- `dedup.scope = tenant` (DEFAULT from v2): `BLAKE3(tenant_id ‖ plain_hash)`
  — same bytes in two tenants land under different keys; the
  cross-tenant existence oracle (V3 M-16) is closed by construction.
- `dedup.scope = global`: legacy raw-hash addressing, an explicit
  operator opt-in for trusted single-tenant deployments; WARN at boot.

New writes always use the configured scope. Existing v1 stores carry
globally-keyed chunks; `xtask rotate-keys --scope tenant` re-derives
every chunk key, renames the blob file, rewrites `chunk_refs`, and
rewrites every manifest's chunk-ref hashes — resumable and verified like
§6 (checkpoint per record; `--verify` re-walks and byte-compares).
README/threat-model caveat text flips from "open ticket" to "resolved;
residual: intra-tenant dedup-hit timing channel, documented".

Gate: `cross_tenant_dedup_never_shares_chunks`.

## 8. Absorbed tickets

- V6-10: per-chunk registration removed — chunk registration joins the
  manifest commit txn (batch, atomic); the pipeline retains the V6-07
  post-commit presence re-assert. `rg 'register_chunk_verified\(' src/`
  → 0 (singular per-chunk variant deleted; plural batch form renamed to
  `register_chunks_at_commit`).
- V6-08+M-03: cursor lists (§4) + reconcile cursor walk (wal.md §7) —
  O(page) and resumable at 1M-row scale.
- V8 C-3/C-4/C-5/C-8: absorbed by the crash-consistency harness, the
  boot-sweep resurrection re-check, the config boot lint, and the
  ci-liveness fixture hardening respectively (see crash-consistency.md /
  boot.md).

## 9. Rollback statement

v2 code opens a v1 dir (dual-read + migrate-on-open). **v1 code must
never open a v2 dir** — the version gate in the v1 binary did not exist,
so the operator contract is: do not downgrade after cutover; snapshot
before `xtask migrate --commit` (PG-2 makes this a rehearsed procedure).
