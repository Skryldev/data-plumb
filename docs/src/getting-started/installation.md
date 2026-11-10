<!-- title: Installation category: getting-started audience: developer|operator last_updated: 2026-11-10 -->

# Installation

This page covers building file-storage from source, the release profile that produces the small production binary, running it as a systemd service, and an optional static-musl build. If you prefer containers, skip ahead to [Docker](./docker.md); for a five-minute evaluation see [Quickstart](./quickstart.md).

## Overview

file-storage compiles to a single stripped binary of roughly **12 MB** with no runtime dependencies beyond libc — no OpenSSL, no system `protoc`, no external database. The build intentionally keeps the toolchain footprint small: the protobuf compiler is vendored into the build via the `protoc-bin-vendored` crate, and all TLS is handled by rustls in-process.

## How it works

`cargo` drives everything. The `build.rs` script compiles the protobuf definitions using a vendored protoc binary (so `protoc` does **not** need to be installed on the build host), and code generation happens entirely offline. The release profile applies aggressive optimisation settings that trade compile time for a compact, fast binary:

| Profile setting | Value | Effect |
|---|---|---|
| `opt-level` | `3` | Maximum optimisation |
| `lto` | `thin` | Cross-crate inlining without full-LTO build times |
| `codegen-units` | `1` | Better optimisation, slower compile |
| `strip` | `symbols` | Strips debug symbols from the binary |

The measured result of `cargo build --release` is a ~11.7 MB binary.

## Configuration

### Requirements

| Requirement | Detail |
|---|---|
| Rust toolchain | **1.80 or newer**, installed via rustup |
| Build space | ~1 GB for the cargo target directory |
| protoc | **Not required** — `build.rs` vendors it via `protoc-bin-vendored` |
| OpenSSL | **Not required** — TLS uses rustls; only `pkg-config` may be needed in minimal container images |

### Build from source

```bash
git clone <your-repo-url> file-storage && cd file-storage
cargo build --release --locked
./target/release/file-storage --config config/default.yaml
```

The project enforces quality gates that you can reproduce locally:

```bash
cargo fmt --check          # formatting must be clean
cargo clippy --all-targets # 0 warnings expected
cargo test                 # 131 unit + 18 gRPC + 17 REST integration tests
```

### Running as a systemd service

Create `/etc/file-storage/` with your configuration, then install a unit such as:

```ini
# /etc/systemd/system/file-storage.service
[Unit]
Description=file-storage content-addressed file service
After=network-online.target
Wants=network-online.target

[Service]
Type=simple
User=file-storage
Group=file-storage
ExecStart=/usr/local/bin/file-storage --config /etc/file-storage/default.yaml
Restart=on-failure
RestartSec=5
StateDirectory=file-storage
Environment=RUST_LOG=info

# Hardening
NoNewPrivileges=true
ProtectSystem=strict
ReadWritePaths=/var/lib/file-storage
PrivateTmp=true

[Install]
WantedBy=multi-user.target
```

`StateDirectory=file-storage` makes systemd create and own `/var/lib/file-storage`, which is where the production configuration points `storage.root`, `storage.temp_dir`, `database.path` and `encryption.keys_dir` (see the production template in `config/production.yaml`). Enable and start:

```bash
sudo systemctl daemon-reload
sudo systemctl enable --now file-storage
systemctl status file-storage
```

### Optional: static musl build

For a fully static binary (useful in scratch containers or minimal images), build against musl. This is **documented as optional**; the glibc build is the supported default.

```bash
sudo apt-get install musl-tools          # Debian/Ubuntu
rustup target add x86_64-unknown-linux-musl
cargo build --release --target x86_64-unknown-linux-musl
# binary: target/x86_64-unknown-linux-musl/release/file-storage
```

## Examples

### Post-install smoke checklist

Run through this list after any fresh install before declaring it healthy:

1. `GET /health` returns `{"status":"ok", ...}` with the expected version and a rising `uptime_secs`.
2. `GET /ready` returns `{"status":"ok","database":"ok","storage":"ok"}`.
3. Login as a configured user succeeds and returns both an access and a refresh token.
4. A small upload returns HTTP 201 with a `content_hash`; the same file downloaded again carries a matching `X-Content-Hash` header.
5. `GET /metrics` exposes `file_storage_*` Prometheus metrics.
6. `grpcurl -plaintext localhost:9090 list` shows the four `file_storage.v1` services plus health and reflection.
7. Journal logs (`journalctl -u file-storage`) show no error lines at `info` level.

## See also

- [Quickstart](./quickstart.md) — first commands after installing.
- [Docker](./docker.md) — the containerised equivalent of this page.
- [Configuration reference](../configuration/reference.md) — every key, default and `FS_` env override.
- [Monitoring](../operations/monitoring.md) — metrics and dashboards for a running install.
- [TLS](../deployment/tls.md) — enabling TLS on the gRPC listener and mTLS for replication.
