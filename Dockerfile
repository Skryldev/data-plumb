# syntax=docker/dockerfile:1

# ---------- build stage ----------
# protoc is unnecessary on the build host: build.rs uses the vendored
# compiler from the `protoc-bin-vendored` crate.
FROM rust:1.83-slim AS build
WORKDIR /src
RUN apt-get update && apt-get install -y --no-install-recommends pkg-config && rm -rf /var/lib/apt/lists/*
COPY Cargo.toml Cargo.lock* build.rs ./
COPY proto ./proto
COPY src ./src
RUN cargo build --release --locked 2>/dev/null || cargo build --release

# ---------- runtime stage ----------
# C-01: NO configuration is baked into the image. The operator MUST mount a
# secret (compose `secrets:` / k8s Secret) — a container booted without one
# exits non-zero with the validation violation list.
FROM gcr.io/distroless/cc-debian12:nonroot
COPY --from=build /src/target/release/file-storage /usr/local/bin/file-storage
ENV RUST_LOG=info
EXPOSE 8080 9090
WORKDIR /var/lib/file-storage
VOLUME ["/var/lib/file-storage/data"]
ENTRYPOINT ["/usr/local/bin/file-storage"]
# C-01: the config path points at the mounted secret; there is no default.
CMD ["--config", "/run/secrets/file-storage.yaml"]
