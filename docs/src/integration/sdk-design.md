<!-- title: SDK Design category: integration audience: developer last_updated: 2026-11-10 -->

# SDK Design

This page documents the design patterns implemented by the Rust SDK in `src/grpc/client/` and the ones worth copying when you write a client in any other language. They are not stylistic preferences — each one exists because the protocol is streaming, HTTP/2-backed and auth-metadata-driven, and getting them wrong produces deadlocks, unbounded memory or unretryable-looking failures. Protocol surface: [gRPC API](../api/grpc.md); reference implementation walkthrough: [Rust Client](./rust-client.md).

## Overview

The SDK's job is to hide four protocol realities behind a small facade: (1) streams start with a typed header message, (2) backpressure is HTTP/2 flow control, so client memory must be bounded by design, (3) authentication travels as `authorization` metadata that must be attached manually to streaming RPCs, and (4) only transport-level failures are safe to retry. Everything else — TLS, error mapping, tests — follows from those constraints.

```text
upload(file, AsyncRead)                         download(file_id, AsyncWrite)
    |                                                |
    v                                                v
[ 32-slot bounded channel ]            server stream (HTTP/2 window 4 MiB)
    | 1 MiB UploadChunk frames         header frame -> data frames
    v                                        |
reader task holds the sender                 v
EOF drops sender -> stream closes     writer receives until stream ends
```

*Figure 1: Bounded producer/consumer on both transports; EOF is signalled by dropping the sender, not by a flag.*

## How it works

### Transport-agnostic facade

`FileStorageClient` wraps the three generated clients (`FileServiceClient`, `AuthServiceClient`, `AdminServiceClient<Channel>`) behind one type with plain methods (`login`, `upload`, `download`, `get_manifest`, `list_files`, `delete_file`, `trigger_gc`, `list_tenants`, `query_audit`). Generated tonic clients are cheap to clone, so each method clones its inner client per call — no shared mutable state. Copy this layering in other languages: generated stubs stay mechanical; the facade owns token state, retries and framing defaults.

### Header-first streaming protocol

`UploadChunk` and `DownloadChunk` are `oneof payload { header | data }`, and the first message **must** be the header. Why: the server can authenticate, validate the name and enforce `expected_size` (failing with `RESOURCE_EXHAUSTED`) before it has buffered a single byte of content; on download the header carries `original_hash` first, so the client can plan verification while bytes flow. Never send a bare data message first — the server rejects the stream.

### Bounded channels for backpressure

The upload facade reads your `AsyncRead` in 1 MiB pieces into a **32-deep** bounded channel. HTTP/2 flow control (4 MiB stream window, 8 MiB connection window on the server) applies backpressure to the gRPC call; the channel applies it to the reader task; the file is never fully buffered. This maps application-level streaming onto transport-level windows without custom flow-control code.

### Move the sender into the reader task

The most instructive bug the SDK fixed: if the upload sender is held by the calling future, the stream never sees half-close/EOF and the server waits forever for the final frame — a deadlock. The fix is structural: `tokio::spawn` the reader task and move the sender into it. When the task ends (EOF or error), the sender drops and the gRPC stream half-closes. In Go this is `CloseSend()`; in Python, exhausting the request iterator. Whatever the language, EOF must be derived from the data source, never from a timer or a flag.

### Metadata-based auth for streaming RPCs

tonic interceptors run **only for unary** RPCs, so the SDK attaches `authorization: Bearer <jwt>` (marked sensitive) to the `Request<T>` carrying the stream itself. The server correspondingly verifies JWT and api-key inside streaming handlers. Client lesson: any language whose interceptor layer skips streams needs the header attached at call construction — the Go `metadata.AppendToOutgoingContext` and Python `metadata=` patterns in the client pages exist for this reason.

### Retry policy: transport codes only, idempotent targets

`RetryPolicy` retries exactly `Unavailable`, `DeadlineExceeded` and `Aborted` with exponential backoff doubling from 100 ms, capped at 2 s, three attempts by default. Business rejections (`NotFound`, `PermissionDenied`, `InvalidArgument`) surface immediately — retrying cannot fix them. Retries wrap **unary** calls only; streaming uploads are not retried by the facade. Safety comes from server-side idempotency:

| Operation | Idempotency guarantee |
|---|---|
| Upload (same name) | Replaces the previous manifest atomically; old one soft-deleted |
| Delete | Idempotent — deleting a missing file is 404/`NOT_FOUND`, not an error to unwind |
| GC trigger | Pure function of current state; running it twice is harmless |

### Error surface

Map gRPC codes to your language's exception taxonomy once, in the facade: `NotFound` → not-found error, `PermissionDenied` → authz error, `ResourceExhausted` → quota/size error, `DataLoss` → corruption, `Unavailable` → transient. The complete code ↔ HTTP mapping is in [Errors](../api/errors.md).

### TLS and CA pinning

`connect_tls(endpoint, ca_pem)` pins a private CA when given, otherwise uses native roots; the dial name must match a SAN in the certificate. The same discipline applies to every client: pin in internal clusters, verify names in external ones, and never disable verification to "fix" a SAN mismatch — see [TLS](../deployment/tls.md).

## Usage

### Testing strategy

The SDK is validated by `tests/grpc_test.rs`, and the harness pattern is worth copying for your own clients:

- **Ephemeral-port server fixture.** Bind `TcpListener` to `127.0.0.1:0`, serve on it, extract the real port — no fixed ports, no cross-test interference.
- **wait_for_ready loop.** Poll TCP connect with a bounded retry (100 × 20 ms) instead of `sleep(1)` guesses.
- **Concurrent upload matrix.** Upload the same payload from N tasks simultaneously; dedup means exactly one writer wins per chunk and everything else is a dedup hit — a strong race detector target.
- **Round-trip assertions on both layers.** Byte-identical content plus `DownloadHeader.original_hash` equality; the header check catches framing bugs that content comparison alone can mask.

### Proto versioning and breaking changes

The `file_storage.v1` package follows the standard compatibility contract:

- Field numbers are forever — never reuse or renumber; add new fields only.
- Retired fields get `reserved` (number and name) so accidental reuse fails at codegen.
- Wire-compatible additions (new optional fields, new oneof branches) are minor changes; any change to a field's type, number or streaming contract is major and requires a `v2` package.
- Run `buf breaking` against the last released descriptor set in CI; the server's reflection serves the current descriptors, so clients built from stale protos fail loudly rather than silently.

## Examples

The retry policy, condensed to its decision table:

```rust
// src/grpc/client/retry.rs — the shape to copy
RetryPolicy::new(3, Duration::from_millis(100), Duration::from_secs(2))
// attempt 1 fails Unavailable -> sleep 100ms
// attempt 2 fails Unavailable -> sleep 200ms ... doubling, capped at 2s
// attempt 3 fails Unavailable -> return the error
// any NotFound/PermissionDenied/InvalidArgument -> return immediately, no sleep
```

And the half-close discipline in pseudocode for any language:

```text
spawn reader task:
    loop { n = read(source, 1 MiB) }
        n == 0 -> break            # EOF
        send(UploadChunk{data})
    drop sender                    # <-- half-close; server finishes the upload
await response
```

## See also

- [gRPC API](../api/grpc.md) — messages, windows, server tuning values.
- [Rust Client](./rust-client.md) — the implemented facade in full.
- [Errors](../api/errors.md) — the mapping table referenced above.
- [Authentication](../api/authentication.md) — token model behind the metadata rules.
- [CLI Recipes](./cli.md) — grpcurl, the fastest way to test a client change.
