<!--
title: Bootstrap & secrets
category: operations
audience: operator
last_updated: 2026-11-11
-->

# Bootstrap & secrets

This page is the C-01 remediation contract in operator form: the service refuses to boot with any secret an attacker could know from the public repository. You provision real secrets, or the process exits non-zero with the full violation list.

## Overview

`file-storage` boots only with operator-supplied secrets. `AppConfig::validate()` is fail-closed: it rejects every literal shipped in the repository (`change-me`, `admin123`, `internal-dev-api-key`, ...), secrets shorter than the policy floor (32 bytes for the JWT secret, 24 for the cluster API key), plaintext `password` entries in release builds, and non-argon2id password hashes — and it reports **all** violations at once, not just the first.

## How it works

- The container image contains **no configuration file**. `ENTRYPOINT` is the binary alone; `CMD` points `--config` at `/run/secrets/file-storage.yaml` (the mounted secret). A fresh `docker run` without a mounted config exits non-zero.
- Users are provisioned as **argon2id PHC strings** (`password_hash`). Plaintext `password` fields are a debug-build convenience and are rejected by validation in release builds.
- `docker-compose.yml` binds REST/gRPC to `127.0.0.1` and runs containers as `65532:65532`; TLS termination happens at your edge proxy or via `server.grpc_tls`.

## Configuration

| What | Where |
|---|---|
| Secret file (git-ignored) | `secrets/file-storage.yaml` |
| Example template | `secrets/file-storage.yaml.example` |
| Compose wiring | `secrets:` facility, `user: "65532:65532"` |
| Config path override | `--config PATH` or `FS_CONFIG=PATH` |

## Examples

Generate the secrets:

```bash
openssl rand -hex 32          # auth.jwt_secret
openssl rand -hex 24          # replication.api_key (per node pair)
file-storage hash-password    # reads the password from stdin, prints the PHC string
```

`hash-password` without `--password` reads from stdin so the secret never lands in shell history.

Bootstrap a local compose stack:

```bash
cp secrets/file-storage.yaml.example secrets/file-storage.yaml
$EDITOR secrets/file-storage.yaml        # paste the generated values
docker compose up -d --wait
curl -s http://127.0.0.1:8080/health
```

Verify the fail-closed behavior (this must fail):

```bash
docker compose down && rm secrets/file-storage.yaml
docker compose up -d --wait              # exits non-zero; logs name auth.jwt_secret
```

Verify a release build refuses the shipped dev config:

```bash
cargo build --release
./target/release/file-storage --config config/default.yaml   # non-zero, lists violations
```

## See also

- [Configuration reference](../configuration/reference.md) — every key the secret file accepts
- [Hardening checklist](../security/hardening.md) — the full 50+ item production gate
- [Secrets](../configuration/secrets.md) — rotation procedures for each secret class
- [Runbook](./runbook.md) — incident procedures
