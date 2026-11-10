<!--
title: ADR 0005 — Per-tenant AES-256-GCM encryption
category: architecture
audience: security-engineer
last_updated: 2026-11-10
-->

# ADR 0005: AES-256-GCM with per-tenant keys

This decision record explains the at-rest encryption design: AES-256-GCM with one 32-byte key per tenant, a self-describing `nonce || ciphertext || tag` blob layout, and key files with `0600` permissions. It also records honestly what is *not* provided — KMS integration and in-place key rotation — and why ChaCha20-Poly1305 was passed over.

## Overview

`encryption.enabled: true` adds one pipeline stage: after compression, every chunk is sealed with the owning tenant's AES-256-GCM key. Keys are generated per tenant on first use and stored as hex files under `encryption.keys_dir` with owner-only permissions. The design favours operational simplicity (no external key service) and hardware throughput (AES-NI), while keeping per-tenant blast-radius isolation.

## Status

**Accepted** — 2026-11-10.

## Context

Encryption at rest had to satisfy:

- **Tenant blast-radius isolation** — compromise of one tenant's key must not expose other tenants' blobs.
- **Throughput** — encryption sits inline in the upload pipeline between zstd and the blob write; a slow cipher would cap upload throughput on commodity servers.
- **Self-describing blobs** — the reader must know how to decode a blob from its manifest alone, without probing formats.
- **No external dependencies in the default deployment** — a KMS or HSM requirement would break the single-process, single-volume model and complicate air-gapped installs.
- **Random-nonce AEAD**, since chunks are encrypted concurrently by the 8-wide pipeline and there is no shared nonce counter.

## Decision

Use AES-256-GCM from the `aes-gcm` 0.10 crate, with one 32-byte key per tenant:

- **Blob layout**: `nonce(12) || ciphertext || tag(16)` — a fresh random 96-bit nonce per encryption, the GCM tag appended; the layout is written as-is to the block store, so blob size grows by 28 bytes.
- **Key storage**: 32 random bytes per tenant, hex-encoded at `<keys_dir>/<tenant>.key`, created with `create_new` under `0600` (owner-only) and fsynced; a `key_file_permissions_ok` check verifies Unix permissions. Keys are cached in a `DashMap` after first load.
- **Explicit flags**: `ChunkRef.encrypted` records the state per chunk — no magic-byte sniffing at decode time.
- **Key lifetime**: the service never rewrites key files. An `Encryptor::key_age_ms` diagnostic exposes the key file's modification time so operators can track key age and drive external rotation; rotating a key today means re-encrypting that tenant's blobs out of band. Full envelope encryption and KMS integration are documented as future work, not implemented features.

## How it works

- **Write**: `Encryptor::encrypt(tenant, compressed_bytes)` → `nonce || ct || tag` → `BlockStore::write` under the tenant-derived storage key (see [ADR 0007](./0007-encrypted-dedup-key-derivation.md) for why the key derivation changes).
- **Read**: the retrieval engine decrypts (GCM authentication failure → `Encryption` error, surfaced as HTTP 500 / gRPC `INTERNAL` with code `encryption_error`), then decompresses, then verifies BLAKE3 against `plain_hash` — so tampering is caught twice: by AEAD and by content hash.
- **Concurrent safety**: random nonces per `encrypt` call are the correct choice for concurrent pipelines; the guideline ceiling for random 96-bit nonces is to keep per-key encryption counts well below 2³² (collision probability ~2⁻³² at that scale), which this service's chunk counts stay far under.

```text
plaintext chunk ──► zstd-3 ──► AES-256-GCM(seal)  ──► nonce(12) || ct || tag(16) ──► blob
                                  key: data/keys/<tenant>.key (0600, hex)
```
*Figure 1: Where the cipher sits in the pipeline. Encryption is applied after compression, so incompressible random data still benefits from ciphertext size reduction upstream.*

## Consequences

**Positive**

- **Per-tenant isolation**: one key file per tenant; losing or rotating one tenant's key affects only that tenant's blobs.
- **Hardware speed**: AES-NI makes AES-256-GCM the fastest AEAD in the Rust ecosystem on typical server CPUs; encryption adds little to the measured 273 MB/s in-process pipeline rate.
- **Authenticated integrity**: GCM tags plus post-decrypt BLAKE3 verification give two independent tamper checks per chunk.
- **Simple ops**: keys are plain files with OS-enforced permissions; no KMS contract, no network hop, works air-gapped.
- **Self-describing storage**: manifest flags say how to decode; the blob needs no headers.

**Negative**

- **Key loss is data loss**: there is no escrow, no envelope, no recovery path if a key file is destroyed; backup discipline for `data/keys` is mandatory (see [Secrets](../../configuration/secrets.md)).
- **Rotation is manual and heavy**: replacing a key requires re-encrypting every blob of the tenant out of band; the service exposes `key_age_ms` as a diagnostic but no rotation workflow.
- **No KMS/HSM**: key material lives on the same host as the data (mitigated by `0600` and host hardening); organisations requiring centralized key custody must wait for the documented envelope-encryption future work.
- **28-byte blob overhead** per chunk, and one extra AEAD pass per chunk on both paths.
- **Encryption scopes dedup**: ciphertext-addressed dedup is impossible with random nonces; resolved via the tenant-scoped key derivation in [ADR 0007](./0007-encrypted-dedup-key-derivation.md).

## Alternatives considered

- **ChaCha20-Poly1305** — an excellent AEAD, but its performance advantage exists only on hardware without AES-NI; on the AES-capable servers this service targets, AES-256-GCM is faster. Rejected for the default; remains a reasonable choice for AES-less platforms.
- **XChaCha20-Poly1305** — a 192-bit nonce removes collision concerns entirely, but it is not in the standard AEAD interface set of the crate family and the throughput argument of ChaCha still applies. Rejected with ChaCha.
- **Full envelope encryption with KMS/HSM integration** — data keys wrapped by a master key held in an external service; the right long-term design for regulated environments, but it introduces an availability dependency (KMS outage = no reads) and deployment complexity. Documented as future work, deliberately not half-implemented.
- **Tenant-supplied keys (client-side encryption)** — shifts key management to clients and forfeits server-side dedup entirely under the naive scheme; also complicates every SDK. Rejected for now.
- **No encryption, rely on volume encryption (LUKS/dm-crypt)** — protects against device theft only, not against host-level readers; provides no per-tenant isolation. Rejected as insufficient for the multi-tenant model.

## Usage

| Key | Default | Notes |
|---|---|---|
| `encryption.enabled` | `false` | Enables the pipeline stage and the tenant-scoped storage-key derivation |
| `encryption.keys_dir` | `data/keys` | One `<tenant-uuid>.key` hex file per tenant, `0600` |

Environment overrides: `FS_ENCRYPTION__ENABLED=true`, `FS_ENCRYPTION__KEYS_DIR=/etc/file-storage/keys`. Keys are created lazily per tenant on first encrypted upload; pre-provision them for replicas so both nodes derive the same storage keys.

## Examples

On-disk evidence of the design:

```console
$ ls -l data/keys/
-rw------- 1 app app 64 Nov 10 09:14 01930000-0000-7000-8000-000000000002.key

$ head -c 56 data/keys/<tenant>.key; echo
3f9a4b17c0d8e5f2a1b3c4d5e6f708192a3b4c5d6e7f8091a2b3c4d5e6f70819   # 64 hex chars = 32 bytes
```

A chunk ref under encryption shows both hashes diverging:

```json
{
  "hash": "9d31c0ffee…(storage key = BLAKE3(tenant_id || plain_hash))",
  "plain_hash": "7c1e9a04b2d8…(plaintext BLAKE3, used for verify)",
  "compressed": true,
  "encrypted": true,
  "size_stored": 12132
}
```

`size_stored` includes the 28 bytes of nonce plus tag; the read path strips and checks them before decompression.

## See also

- [Security](../security.md) — the security architecture summary
- [Encryption at Rest](../../security/encryption.md) — operational key management
- [ADR 0007 — Encrypted dedup key derivation](./0007-encrypted-dedup-key-derivation.md) — the storage-key design under encryption
- [Threat Model](../../security/threat-model.md) — what these controls do and do not cover
- [Secrets](../../configuration/secrets.md) — handling `keys_dir` and other secret material
- [Configuration reference](../../configuration/reference.md) — the `encryption.*` keys
