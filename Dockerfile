# syntax=docker/dockerfile:1.7

# ---------------------------------------------------------------------------
# Stage 1 — chef: compute dependency recipe (cached across source changes)
# ---------------------------------------------------------------------------
FROM lukemathwalker/cargo-chef:latest-rust-1.100.0-slim-trixie AS chef
WORKDIR /src

# ---------------------------------------------------------------------------
# Stage 2 — planner: emit recipe.json from the manifest set only
# ---------------------------------------------------------------------------
FROM chef AS planner
COPY Cargo.toml Cargo.lock* build.rs ./
COPY proto ./proto
COPY src ./src
COPY benches ./benches
RUN cargo chef prepare --recipe-path recipe.json

# ---------------------------------------------------------------------------
# Stage 3 — builder: compile deps first (cache hit on src-only changes),
# then the workspace. `--locked` is mandatory for reproducibility.
# ---------------------------------------------------------------------------
FROM chef AS builder

# Only build-time tooling goes here; keep the layer list minimal and pinned
# in spirit (apt repos are pinned by the base image tag).
RUN apt-get update \
 && apt-get install -y --no-install-recommends \
        pkg-config \
        ca-certificates \
 && rm -rf /var/lib/apt/lists/*

COPY --from=planner /src/recipe.json ./recipe.json
RUN cargo chef cook --release --locked --recipe-path recipe.json

# Now the real sources; this is the only layer invalidated by code changes.
COPY Cargo.toml Cargo.lock* build.rs ./
COPY proto ./proto
COPY src ./src
COPY benches ./benches

# `--locked` guarantees the committed lockfile is honored. No silent fallback:
# a broken lockfile should fail the build, not be masked.
RUN cargo build --release --locked \
 && strip target/release/file-storage

# ---------------------------------------------------------------------------
# Stage 4 — runtime: distroless non-root, no shell, no package manager
# ---------------------------------------------------------------------------
FROM gcr.io/distroless/cc-debian12:nonroot AS runtime

# Metadata (OCI labels; harmless on older engines)
LABEL org.opencontainers.image.title="file-storage" \
      org.opencontainers.image.description="file-storage service" \
      org.opencontainers.image.licenses="UNLICENSED"

COPY --from=builder /src/target/release/file-storage /usr/local/bin/file-storage

# Runtime env only — non-secret defaults. Secrets/config come from mounts.
ENV RUST_LOG=info \
    RUST_BACKTRACE=0 \
    FILE_STORAGE_DATA_DIR=/var/lib/file-storage/data

EXPOSE 8080 9090

# Writable data dir owned by the nonroot user (uid/gid 65532 in distroless).
WORKDIR /var/lib/file-storage
VOLUME ["/var/lib/file-storage/data"]

# C-01: no config baked in. The operator MUST mount a secret at this path;
# a container started without one exits non-zero listing validation errors.
ENTRYPOINT ["/usr/local/bin/file-storage"]
CMD ["--config", "/run/secrets/file-storage.yaml"]