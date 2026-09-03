# syntax=docker/dockerfile:1
# Dev/Tailscale deployment — not a hardened prod image.

FROM rust:1-slim-bookworm AS builder
WORKDIR /workspace

RUN apt-get update && apt-get install -y --no-install-recommends pkg-config libssl-dev \
    && rm -rf /var/lib/apt/lists/*

COPY . .
# Cache mounts persist cargo's registry + incremental build artifacts across
# `docker compose build` runs (this Dockerfile has no other cache — every
# rebuild otherwise recompiles the whole workspace from scratch, ~35-40min on
# this hardware). The mounted /workspace/target isn't part of the final image
# layer, so the binary is copied out to a plain path before the mount drops.
RUN --mount=type=cache,target=/usr/local/cargo/registry \
    --mount=type=cache,target=/workspace/target \
    cargo build --release --bin server && \
    cp target/release/server /workspace/server-bin

FROM debian:bookworm-slim AS runtime
WORKDIR /app

RUN apt-get update && apt-get install -y --no-install-recommends ca-certificates \
    && rm -rf /var/lib/apt/lists/*

COPY --from=builder /workspace/server-bin /app/server

# The client lives in the r4chii-frontend repository, outside this build context, so it is built and
# copied in ahead of `docker compose build` by scripts/sync-client.sh — this
# directory must exist (even empty) or the COPY below fails the build. An
# empty directory just means every request to it 404s; the API itself is
# unaffected.
COPY client-dist /app/client-dist

EXPOSE 8080
ENTRYPOINT ["/app/server"]
