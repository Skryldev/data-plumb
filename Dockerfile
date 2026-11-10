# syntax=docker/dockerfile:1

# ---------- chef base ----------
# cargo-chef pre-installed; no need to cargo install at build time
FROM lukemathwalker/cargo-chef:latest-rust-1.99-slim-trixie AS chef
WORKDIR /src

# System deps needed by build.rs / linking (pkg-config for vendored protoc is usually enough,
# add libssl-dev only if your crate actually links openssl)
RUN apt-get update && apt-get install -y --no-install-recommends \
        pkg-config \
    && rm -rf /var/lib/apt/lists/*

# ---------- planner: generate recipe.json ----------
FROM chef AS planner
COPY Cargo.toml Cargo.lock* build.rs ./
COPY proto ./proto
COPY src ./src
RUN cargo chef prepare --recipe-path recipe.json

# ---------- builder: cook deps then build app ----------
FROM chef AS builder

# Copy the dependency recipe first — this layer stays cached unless
# Cargo.toml / Cargo.lock change.
COPY --from=planner /src/recipe.json recipe.json

# Cook dependencies with BuildKit cache mounts for cargo registry/git
RUN --mount=type=cache,target=/usr/local/cargo/registry \
    --mount=type=cache,target=/usr/local/cargo/git \
    cargo chef cook --release --locked --recipe-path recipe.json

# Now copy the full source and build only the application crates
COPY Cargo.toml Cargo.lock* build.rs ./
COPY proto ./proto
COPY src ./src

RUN --mount=type=cache,target=/usr/local/cargo/registry \
    --mount=type=cache,target=/usr/local/cargo/git \
    cargo build --release --locked

# ---------- runtime stage ----------
FROM gcr.io/distroless/cc-debian12:nonroot
COPY --from=builder /src/target/release/file-storage /usr/local/bin/file-storage
COPY --from=builder /src/config /etc/file-storage

ENV RUST_LOG=info
EXPOSE 8080 9090
WORKDIR /var/lib/file-storage
VOLUME ["/var/lib/file-storage/data"]
ENTRYPOINT ["/usr/local/bin/file-storage", "--config", "/etc/file-storage/default.yaml"]