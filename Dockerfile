# syntax=docker/dockerfile:1
#
# amicu-server container image.
#
# One image, two Cloud Run resources (amicu-planning#143, amicu-infra's
# envs/prod/main.tf): the `amicu-api` SERVICE runs it with no arguments
# (ENTRYPOINT only, binds $PORT); the `amicu-migrate` JOB overrides the
# container `command` entirely (Cloud Run Job semantics) with
# `["/usr/local/bin/amicu-server", "migrate"]` to apply pending schema
# migrations, connected as the schema-owning role, and exit — see
# `crates/amicu-server/src/main.rs`'s `Command::Migrate` and
# `ops/sql/provision_app_role.sql`.
#
# Toolchain: CI's primary build/clippy/test job (.github/workflows/ci.yml
# `check`) uses `dtolnay/rust-toolchain@stable` (rolling stable, NOT pinned to
# a single patch release) — only the separate `msrv` job pins 1.97 as a
# compile-only floor check. `rust:1-slim-bookworm` mirrors that: Docker Hub's
# rolling "current stable Rust 1.x" tag, same rolling-stable contract as CI's
# `@stable` toolchain action, so this image tracks whatever `cargo
# clippy/build/test` on `main` was actually built with rather than drifting
# to (or pinning below) it.

FROM rust:1-slim-bookworm AS chef
RUN cargo install cargo-chef --locked
WORKDIR /app

FROM chef AS planner
COPY . .
RUN cargo chef prepare --recipe-path recipe.json

FROM chef AS builder
# Build-time-only native toolchain: aws-lc-sys (pulled in transitively via
# reqwest's default rustls/aws-lc-rs backend, see amicu-api/amicu-providers)
# needs cmake + a C compiler + perl to build its vendored BoringSSL-derived C
# core; `ring` (lettre's SMTP TLS backend) needs a C compiler only. None of
# this survives into the runtime stage.
# Deliberately unpinned (hadolint DL3008, ignored below): these are
# transient build-stage tools, discarded with this whole stage and never
# reaching the runtime image — pinning exact bookworm apt versions here
# would just go stale/404 against the snapshot rotating out from under a
# rarely-rebuilt Dockerfile with no upside (no reproducibility guarantee
# that matters once the resulting binary is what actually ships).
# hadolint ignore=DL3008
RUN apt-get update && apt-get install -y --no-install-recommends \
    build-essential \
    cmake \
    perl \
    pkg-config \
    && rm -rf /var/lib/apt/lists/*

COPY --from=planner /app/recipe.json recipe.json
# The repo commits the root-level .sqlx/ cache (`cargo sqlx prepare
# --workspace`), so `sqlx::query!`/`query_as!` verify against that offline
# cache instead of connecting to a live Postgres at build time — required
# here since the build has no DATABASE_URL and no network path to one.
ENV SQLX_OFFLINE=true
# Dependency-only layer: cached across builds whose recipe.json (i.e.
# Cargo.lock + every crate's Cargo.toml) is unchanged, independent of source
# edits — the whole point of the cargo-chef split.
RUN cargo chef cook --release --recipe-path recipe.json --workspace

# Now bring in real sources and build only the binary Cloud Run runs.
COPY . .
RUN cargo build --release --locked -p amicu-server --bin amicu-server \
    && strip target/release/amicu-server

# ---------------------------------------------------------------------------
# Runtime. amicu-server's TLS stacks are rustls-only end to end (sqlx
# tls-rustls, lettre's rustls/ring SMTP transport, reqwest's rustls/aws-lc-rs
# backend) — no libssl.so anywhere, so distroless's minimal debian12 "cc"
# variant (libc + libgcc, no shell/package manager/coreutils) is sufficient.
# Outbound HTTPS/SMTP still need a CA trust store to validate server
# certificates even with rustls doing the handshake itself — cc-debian12
# bundles /etc/ssl/certs (ca-certificates) for exactly that, so nothing
# extra needs copying in. `:nonroot` runs as uid/gid 65532 (`nonroot`) with
# no further USER directive needed.
# ---------------------------------------------------------------------------
FROM gcr.io/distroless/cc-debian12:nonroot AS runtime

ARG GIT_REVISION=unknown
LABEL org.opencontainers.image.source="https://github.com/andypalmi/amicu" \
      org.opencontainers.image.revision="${GIT_REVISION}" \
      org.opencontainers.image.title="amicu-server" \
      org.opencontainers.image.description="amicu Management API — Cloud Run service (amicu-api) and migrate Job (amicu-migrate) image" \
      org.opencontainers.image.licenses="AGPL-3.0-only"

COPY --from=builder /app/target/release/amicu-server /usr/local/bin/amicu-server

# Reserved by Cloud Run (the platform injects the real PORT env var at
# runtime and amicu-server binds it directly, see AMICU_BIND/$PORT in
# crates/amicu-server/src/lib.rs) — this EXPOSE is documentation / local
# `docker run -P` convenience only, not something Cloud Run reads.
EXPOSE 8080

# "nonroot" (hadolint DL3066, ignored below) is the distroless base's own
# named account (uid/gid 65532, see
# https://github.com/GoogleContainerTools/distroless/blob/main/base/base.bzl)
# — using its name rather than the raw uid is the documented, intended way
# to select it and is what every distroless :nonroot example ships.
# hadolint ignore=DL3066
USER nonroot
ENTRYPOINT ["/usr/local/bin/amicu-server"]
