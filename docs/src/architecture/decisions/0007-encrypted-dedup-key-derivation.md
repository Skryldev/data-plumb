<!--
title: ADR 0007 — Encrypted dedup key derivation
category: architecture
audience: security-engineer
last_updated: 2026-11-10
-->

# ADR 0007: Encrypted dedup key derivation

This decision record explains the tension between encryption and dedup, and the derivation `storage_key = BLAKE3(tenant_id || plain_hash)` that resolves it. It is the single most security-sensitive design choice in the system, so the reasoning and the rejected alternatives are spelled out in full.

## Overview

AES-256-GCM sealing ([ADR 0005](./0005-aes-gcm-per-tenant-key.md)) uses a random nonce per encryption, so encrypting the same chunk twice produces different ciphertext — dedup by ciphertext hash is impossible. The service solves this by keying blobs on `BLAKE3(tenant_id || plain_hash)` when encryption is enabled: dedup remains exact within a tenant, becomes structurally impossible across tenants, and leaks nothing about plaintext equality to anyone outside the tenant.

## Status

**Accepted** — 2026-11-10.

## Context

Without encryption, the block store is addressed by `BLAKE3(plaintext)`, and dedup falls out of content addressing ([ADR 0001](./0001-cas-over-block-store.md)). With encryption enabled, the naive pipeline breaks:

1. Chunk plaintext is hashed → `plain_hash`.
2. Compression then encryption with a random nonce → ciphertext blob.
3. If the blob were addressed by `BLAKE3(ciphertext)`, the random nonce would make every encryption unique — no chunk would ever dedup, even re-uploading the identical file.

The alternatives each carry security costs. Deriving an encryption key from plaintext (convergent encryption) enables dictionary attacks. Deterministic encryption (fixed nonce per plaintext) fails catastrophically for GCM, where nonce reuse is fatal. The requirement set was therefore: keep dedup for the duplicate-heavy, same-tenant workloads this service targets; guarantee zero cross-tenant information leakage, including equality (existence) probes; and never weaken AEAD nonce uniqueness.

## Decision

When `encryption.enabled: true`, the block-store key for a chunk is derived as:

```rust
// src/security/encryption.rs
pub fn storage_key(tenant: &Uuid, plain_hash: &[u8; 32]) -> [u8; 32] {
    let mut buf = Vec::with_capacity(16 + 32);
    buf.extend_from_slice(tenant.as_bytes());   // 16 raw bytes
    buf.extend_from_slice(plain_hash);          // 32 raw bytes
    hash_bytes(&buf)                            // BLAKE3
}
```

The upload pipeline computes `storage_hash = Encryptor::storage_key(&tenant, &plain_hash)` per chunk; `ChunkRef.hash` carries the storage key (the blob name and `chunks`-table row) while `ChunkRef.plain_hash` still carries the plaintext BLAKE3 for read-back verification. Dedup is exact within a tenant — identical plaintext under one tenant maps to the same storage key — and impossible across tenants by construction, because a different tenant UUID yields a different key.

## How it works

```text
same tenant, same plaintext:
    BLAKE3(tenant_A || H)  ──►  one blob, refcount++   (dedup works)
different tenants, same plaintext:
    BLAKE3(tenant_A || H) ≠ BLAKE3(tenant_B || H)      (two blobs, no linkage)
```
*Figure 1: The derivation's effect. Tenant identity is mixed into the address, so equality of storage keys implies equality of tenant and plaintext — nothing else.*

On the read path, the retrieval engine decrypts with the tenant's key, decompresses, and verifies the plaintext against `plain_hash`; the storage key's derivation plays no role in verification, so the two hashes serve independent purposes.

## Consequences

**Positive**

- **Dedup survives encryption within a tenant** — the dominant case (repeated uploads, backups, same-name replacement) keeps full storage savings; measured behaviour is unchanged from the unencrypted path.
- **Zero cross-tenant leakage by construction**: no equality oracle exists across tenants. A malicious tenant cannot probe whether another tenant stored a given file, because blob addresses never collide across tenants regardless of content.
- **No cryptographic compromises**: nonces stay random and unique (GCM safety intact); no convergent-encryption weaknesses are introduced.
- **Simple implementation**: one 48-byte BLAKE3 input per chunk; no additional secrets, keys, or protocols beyond the per-tenant key of [ADR 0005](./0005-aes-gcm-per-tenant-key.md).

**Negative**

- **`tenant_id` becomes part of the key derivation — tenant UUIDs must never change.** Renaming a tenant is fine (names live in the `tenants` table), but re-issuing a tenant UUID would orphan every blob that tenant ever stored, and no in-place migration exists. Treat tenant UUIDs as immutable identifiers.
- **No cross-tenant dedup under encryption**: two tenants storing the same ISO pay for it twice. Accepted: the storage saving is real but small next to the leakage risk, and deployments that need cross-tenant sharing should model it as one tenant.
- **Tenant-scoped, not user-scoped**: all users of one tenant share the dedup domain, since the derivation uses the tenant UUID — consistent with the tenancy model, but worth stating.
- **Mixed-mode replication hazard**: a primary and replica must agree on `encryption.enabled` and share key files, or derived storage keys diverge and reads fail with decryption errors. Pre-provision `data/keys` on replicas.

## Alternatives considered

- **Convergent encryption** (derive the AEAD key from the plaintext hash) — restores global dedup, but is vulnerable to confirmed-dictionary attacks: an attacker who can guess a candidate plaintext can verify it against stored ciphertext. The whole point of per-tenant keys is to prevent exactly this. Rejected.
- **Per-tenant fixed tweak only** (for example, binding `tenant_id` into GCM's associated data while keeping ciphertext addressing) — still requires deterministic encryption to make ciphertext hashes match, which is unsafe for GCM (nonce reuse) or any AEAD. Rejected as technically unsound, not merely risky.
- **No dedup under encryption** (address purely by ciphertext hash) — the safest option and the simplest, at the cost of losing all dedup for encrypted workloads; duplicate-heavy tenants would see multi-fold storage growth. Rejected after weighing the leakage-free alternative above.
- **Dup-less encryption schemes from the literature** (Message-locked encryption with server-aided key generation) — interesting research direction, but no audited, production-grade Rust implementation exists; the complexity and attack surface outweigh the cross-tenant savings. Rejected; revisit only with a credible implementation.

## Usage

The derivation is implicit in `encryption.enabled` — there is no separate toggle:

| Key | Default | Effect on this decision |
|---|---|---|
| `encryption.enabled` | `false` | `false` → storage key = `plain_hash`; `true` → `BLAKE3(tenant_id || plain_hash)` |
| `encryption.keys_dir` | `data/keys` | Tenant keys; must match across primary and replica |

Operational rule: **tenant UUIDs are immutable.** They are minted at tenant creation (or seeded from `tenants[]` in config) and must never be re-generated; renaming via `POST /api/v1/tenants` semantics and config edits is safe.

## Examples

Verify the scoping property on a running pair of tenants:

```bash
# upload the identical file as alice (tenant …002) and bob (tenant …003)
# with encryption enabled on both:
#   alice's report: hash = BLAKE3(tenant_002 || H)   -> blob 1
#   bob's   report: hash = BLAKE3(tenant_003 || H)   -> blob 2 (no shared blob)
# re-upload as alice again:
#   dedup_chunks = chunk_count                        -> refcount on blob 1
```

The manifest makes both derivations visible:

```json
{
  "hash": "9d31c0ffee…(tenant-scoped storage key)",
  "plain_hash": "7c1e9a04b2d8…(plaintext BLAKE3 for verification)",
  "encrypted": true
}
```

## See also

- [ADR 0005 — AES-GCM per-tenant key](./0005-aes-gcm-per-tenant-key.md) — the cipher and key-file design this builds on
- [Security](../security.md) — where this sits in the security model
- [Chunking & Dedup](../chunking.md) — the unencrypted dedup mechanics being scoped
- [Storage Layout](../storage-layout.md) — `ChunkRef.hash` versus `ChunkRef.plain_hash` encodings
- [Threat Model](../../security/threat-model.md) — attacker assumptions about leakage and probing
