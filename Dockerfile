# =============================================================
# Stage 1: chef base
# cargo-chef + Rust toolchain + minimal system deps
# =============================================================
FROM lukemathwalker/cargo-chef:latest-rust-1.99.1-bookworm AS chef
WORKDIR /src

# System deps needed by build.rs / linking.
# pkg-config is usually enough; add libssl-dev only if your crate links OpenSSL.
RUN apt-get update \
    && apt-get install -y --no-install-recommends \
        pkg-config \
    && rm -rf /var/lib/apt/lists/* /var/cache/apt/*

# =============================================================
# Stage 2: planner — generate recipe.json from manifests
# =============================================================
FROM chef AS planner

# Copy everything referenced by Cargo.toml so cargo-chef can parse the manifest.
COPY Cargo.toml Cargo.lock* build.rs ./
COPY proto ./proto
COPY src ./src

RUN cargo chef prepare --recipe-path recipe.json

# =============================================================
# Stage 3: builder — cook deps (cached) then build the app
# =============================================================
FROM chef AS builder

# Cook dependencies first — cached until Cargo.toml / Cargo.lock change.
COPY --from=planner /src/recipe.json recipe.json

RUN --mount=type=cache,target=/usr/local/cargo/registry \
    --mount=type=cache,target=/usr/local/cargo/git \
    cargo chef cook --release --locked --recipe-path recipe.json

# Copy the real source and build only the application crates.
COPY Cargo.toml Cargo.lock* build.rs ./
COPY proto ./proto
COPY src ./src
COPY config ./config

RUN --mount=type=cache,target=/usr/local/cargo/registry \
    --mount=type=cache,target=/usr/local/cargo/git \
    --mount=type=cache,target=/src/target \
    cargo build --release --locked \
    && cp target/release/file-storage /usr/local/bin/file-storage

# =============================================================
# Stage 4: runtime — distroless, nonroot, minimal
# =============================================================
FROM gcr.io/distroless/cc-debian12:nonroot

LABEL org.opencontainers.image.title="file-storage" \
      org.opencontainers.image.description="File storage service" \
      org.opencontainers.image.source="https://github.com/skryldev/data-plumb" \
      org.opencontainers.image.version="1.2.0"

# Binary already built; copy only what's needed at runtime.
COPY --from=builder /usr/local/bin/file-storage /usr/local/bin/file-storage

# Config files (default.yaml, production.yaml, prometheus.yml)
COPY --from=builder /src/config /etc/file-storage

ENV RUST_LOG=info \
    RUST_BACKTRACE=0

EXPOSE 8080 9090
WORKDIR /var/lib/file-storage

# Prefer named volumes or bind mounts at `docker run` / compose level.
# VOLUME creates anonymous volumes that are hard to manage.
# VOLUME ["/var/lib/file-storage/data"]

ENTRYPOINT ["/usr/local/bin/file-storage", "--config", "/etc/file-storage/default.yaml"]