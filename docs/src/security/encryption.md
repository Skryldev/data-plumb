<!-- title: Encryption at Rest category: security audience: security-engineer|architect last_updated: 2026-11-10 -->

# Encryption at Rest

With `encryption.enabled=true`, every chunk blob is encrypted with AES-256-GCM using a per-tenant key before it touches the disk, and every read decrypts transparently — REST and gRPC callers see plaintext bytes exactly as uploaded. This page documents the blob format, the key storage model, the clever-but-important storage-key derivation that preserves deduplication without leaking information, and the honest limits of the scheme.

## Overview

Each tenant owns a 32-byte AES-256-GCM key materialised as a file in `encryption.keys_dir`, created on first use with `0600` permissions. Blobs on disk are `nonce(12) || ciphertext || tag(16)`. Because AES-GCM nonces are random, identical plaintext encrypts to different ciphertext — content addressing by ciphertext is impossible. file-storage solves this by deriving the storage key as `BLAKE3(tenant_id ‖ plain_hash)`, keeping dedup exact *within* a tenant while making cross-tenant dedup (and the information leak it would create) structurally impossible.

## How it works

### Pipeline position

Encryption sits after compression in the upload pipeline and before the atomic blob write:

```text
 chunk ─► BLAKE3(plain) ─► zstd ─► AES-256-GCM ─► blob write ─► redb transaction
                                      │
        key = <keys_dir>/<tenant_uuid>.key (32 random bytes)
```

### Blob layout

```text
 offset  0                12                          N-16  N
        ┌──────────────────┬───────────────────────────────┬─────────┐
        │ nonce (12 bytes) │ ciphertext (compressed chunk) │ tag 16B │
        └──────────────────┴───────────────────────────────┴─────────┘
```

The 12-byte GCM nonce is fresh random per write; the 16-byte authentication tag makes any tampering fail decryption. Decryption errors surface as `encryption_error` (HTTP 500 / gRPC INTERNAL) and never return partial data.

### Key storage

| Property | Value |
|---|---|
| Location | `<encryption.keys_dir>/<tenant_uuid>.key` |
| Content | 64 hex characters = 32 random bytes |
| Creation | on first encrypt/decrypt for that tenant (`create_new` — never overwritten) |
| Permissions | `0600` (owner read/write only) |
| Caching | Keys are held in an in-process cache after first load |

There is **no KMS integration**: the key files are the trust root. Protect them the way you protect database credentials — dedicated volume, host-level encryption, restricted access — and back them up with the data (a lost key means permanently unreadable blobs).

### Storage-key derivation, and why

Without encryption, a chunk is stored at `BLAKE3(plaintext)`. Random nonces break that, so with encryption enabled the block-store key becomes:

```text
storage_key = BLAKE3( tenant_id ‖ plain_hash )
```

This one-line change delivers three properties at once:

1. **Deterministic addressing is restored** — the same plaintext chunk in the same tenant always maps to the same blob path, so dedup keeps working.
2. **No cross-tenant leakage** — two tenants uploading identical content get different storage keys *and* different ciphertexts. An operator reading both block stores cannot even confirm the files were the same.
3. **Dedup is scoped by construction** — within a tenant, dedup remains exact (`dedup_chunks == chunk_count` on re-upload); across tenants, it never triggers.

The plaintext hash is still recorded per chunk (`plain_hash` in the manifest's `ChunkRef`) and every disk read re-verifies it after decryption, so corruption cannot pass silently.

## Configuration

```yaml
encryption:
  enabled: true
  keys_dir: "/var/lib/file-storage/data/keys"
```

| Key | Default | Meaning |
|---|---|---|
| `encryption.enabled` | `false` | Master switch; encrypts new blobs at upload time |
| `encryption.keys_dir` | `data/keys` | Where per-tenant key files live |

### What is NOT encrypted

Be precise about the boundary:

- **Metadata and manifests** live in the redb database unencrypted: filenames, tags, MIME types, sizes, chunk hashes and audit events.
- **Tenant identity** (UUIDs) appears in key filenames and in the derivation input.
- Only **chunk blobs** under `storage.root` are ciphertext.

If filenames are sensitive, combine at-rest encryption with full-disk encryption so the redb file is covered too.

## Examples

### Key rotation — the honest story

There is no built-in rotation command, and the derivation makes naive rotation dangerous. The storage key depends only on `(tenant_id, plain_hash)` — **not** on the key material. Replacing `<keys_dir>/<tenant>.key` with new key bytes therefore:

1. Changes the AES key used for all *new* writes.
2. Leaves old blobs in place, encrypted under the old key — they now fail authenticated decryption on read (every download of old content errors with `encryption_error`).

The supported migration is read-and-reupload while the old key is still active:

```bash
# 1. while the OLD key is still in place, export every tenant file to plaintext
# 2. hard-purge all tenant files (admin) and let GC remove the old blobs
# 3. replace <keys_dir>/<tenant>.key with a fresh key file (32 random bytes, 0600)
# 4. re-upload the exported data — it is re-encrypted under the new key
```

Skipping the purge is a footgun: without it, re-uploads would dedup onto the old-key ciphertext blobs and remain unreadable. Two diagnostics help you operate this safely: `Encryptor::has_key(tenant)` reports whether a key file exists, and the key-file modification time (exposed by the `key_age_ms` diagnostic) tells you how old the active key is — wire an alert on stale keys if rotation matters to you.

### Verifying encryption is active

```bash
FS_ENCRYPTION__ENABLED=true cargo run --release -- --config config/default.yaml &
# upload anything, then look at the block store:
xxd data/blocks/$(ls data/blocks/ab/cd | head -1 2>/dev/null) | head -2
# ciphertext: no readable compression headers or plaintext
```

Also confirm the manifest reports `encrypted: true` per chunk via `GET /api/v1/files/:id/meta?chunks=true` (the metadata walkthrough is on the Your First File page in the Getting Started section). Reads need no changes on either transport — REST and gRPC decrypt transparently on the way out.

### Consequences to remember

- **Tenant UUIDs are immutable.** The key filename and the storage-key derivation both embed the tenant UUID; re-keying a tenant under a new UUID orphans every blob and loses the key mapping.
- Dedup within a tenant is exact; across tenants it is zero — plan capacity accordingly when you enable encryption on a multi-tenant system with shared content.
- Enabling encryption on an existing plaintext node does not re-encrypt old blobs; only new writes are encrypted. Migrate the same way as rotation: export, purge, re-upload.

## See also

- [Threat Model](./threat-model.md) — which adversary this defends against, and residual risks.
- [Authentication](./authentication.md) — how tenants are determined per request.
- [Tenants](./tenants.md) — tenant registration and the immutability contract.
- [Configuration reference](../configuration/reference.md) — `encryption.*` keys.
- [Hardening Checklist](./hardening.md) — key file permissions and rotation cadence.
