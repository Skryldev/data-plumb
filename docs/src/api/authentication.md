<!-- title: Authentication category: api audience: developer last_updated: 2026-11-10 -->

# Authentication

file-storage authenticates users with username/password exchanged for an HS256 JWT access token plus an opaque, rotating refresh token. The same pair works on both protocols: REST takes the `Authorization` header, gRPC takes the `authorization` metadata. Roles (`admin` > `user` > `viewer`) and tenant membership are baked into the token and enforced on every request.

## Overview

- **Access token** — stateless HS256 JWT; TTL `auth.access_ttl_secs` (default **900 s = 15 min**). Cannot be revoked; it simply expires.
- **Refresh token** — opaque UUIDv7, TTL `auth.refresh_ttl_secs` (default 7 days), stored **in memory** server-side, **single-use** (rotated on every refresh), lost on restart.
- **Users** come from config only (`auth.users[]`) — there is no user-registration endpoint. Passwords may be plaintext (`password`, dev) or an argon2id PHC string (`password_hash`, production).
- Login attempts for unknown users burn a dummy argon2 verification, so response timing does not reveal whether the username exists.

## How it works

`POST /api/v1/auth/login` verifies the password (argon2id against `password_hash`, or constant-time compare for `password`) and issues a pair. The JWT claims:

| Claim | Value |
|---|---|
| `sub` | username |
| `tenant` | tenant UUID the user belongs to |
| `role` | `admin`, `user` or `viewer` |
| `iat` | issued-at (unix seconds) |
| `exp` | `iat + access_ttl_secs` |
| `jti` | UUIDv7 token id |

Verification allows **5 s of clock leeway** and pins the algorithm to HS256 with `auth.jwt_secret` — anyone holding that secret can mint valid tokens, so treat it like a root credential ([Secrets](../configuration/secrets.md)).

**Refresh rotation:** `POST /auth/refresh` consumes the presented token and returns a *new* pair; the old refresh token is revoked immediately, so a replayed token fails with 401. Because the refresh store is in-memory, a **restart logs every client out of refresh capability** (access tokens keep working until they expire). **Logout** revokes only the presented refresh token and is idempotent; the access token remains usable until `exp`.

## Usage

Login and use the token on both protocols:

```bash
curl -s localhost:8080/api/v1/auth/login \
  -d '{"username":"alice","password":"alice123"}'
```

```json
{"access_token":"eyJhbGciOiJIUzI1NiJ9.eyJzdWIiOiJhbGljZSIsInRlbmFudCI6IjAxOTMwMDAwLTAwMDAtNzAwMC04MDAwLTAwMDAwMDAwMDAwMiIsInJvbGUiOiJ1c2VyIiwiaWF0IjoxNzMwMDAwMDAwLCJleHAiOjE3MzAwMDA5MDAsImp0aSI6IjAxOTMwMDAwLW…","refresh_token":"01930000-0000-7000-8000-00000000feed","token_type":"Bearer","expires_in":900}
```

```bash
# REST
curl -H "Authorization: Bearer $ACCESS_TOKEN" localhost:8080/api/v1/files

# gRPC (metadata; lowercase key)
grpcurl -plaintext -H "authorization: Bearer $ACCESS_TOKEN" …
```

Role matrix (✓ allowed):

| Operation | admin | user | viewer |
|---|---|---|---|
| Download / meta / list own tenant | ✓ | ✓ | ✓ |
| Upload, soft delete (own tenant) | ✓ | ✓ | — |
| Cross-tenant file access | ✓ | — | — |
| `include_deleted` listings | ✓ | — | — |
| Hard purge (`?purge=true`) | ✓ | — | — |
| Tenants create/list | ✓ | — | — |
| Audit query, GC trigger, resync | ✓ | — | — |
| Login/refresh/logout | ✓ | ✓ | ✓ |

Hashing passwords for production — generate an argon2id PHC string with the built-in subcommand and put it in `auth.users[].password_hash` (drop the `password` field):

```bash
file-storage hash-password --password 'S3cret-hunter2'
# $argon2id$v=19$m=65536,t=3,p=4$… (paste into config/auth.users[].password_hash)
```

## Configuration

| Key | Default | Notes |
|---|---|---|
| `auth.jwt_secret` | — | ≥ 16 chars, enforced at boot; rotate per [Secrets](../configuration/secrets.md) |
| `auth.access_ttl_secs` | 900 | access-token lifetime |
| `auth.refresh_ttl_secs` | 604800 | refresh lifetime (7 d) |
| `auth.users[].username` / `tenant_id` / `role` | — | `role` ∈ `admin\|user\|viewer`; each user needs `password` **or** `password_hash` |

The dev/default config ships the test users `admin/admin123`, `alice/alice123`, `bob/bob123`, `viewer/viewer123` — never use them outside a sandbox.

## Examples

Refresh (note the single-use rotation — store the *new* refresh token):

```bash
curl -s localhost:8080/api/v1/auth/refresh -d '{"refresh_token":"0193…feed"}'
```

Logout — revokes the refresh token, always 204:

```bash
curl -s -X POST localhost:8080/api/v1/auth/logout -d '{"refresh_token":"0193…feed"}' -o /dev/null -w '%{http_code}\n'
```

A 401 body looks like `{"error":{"code":"unauthorized","message":"invalid credentials"}}`; a viewer attempting an upload gets `403 {"error":{"code":"forbidden","message":"write role required"}}`.

## See also

- [REST](./rest.md), [gRPC](./grpc.md) — how auth plugs into each transport
- [Errors](./errors.md) — 401/403 mapping
- [Configuration reference](../configuration/reference.md) — `auth.*` keys
- [Secrets](../configuration/secrets.md) — jwt_secret rotation
- [Threat model](../security/threat-model.md), [Hardening checklist](../security/hardening.md)
