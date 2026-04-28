# syntax=docker/dockerfile:1

# ── builder ──────────────────────────────────────────────────────────────────
FROM rust:1-bookworm AS builder

WORKDIR /build

# Cache dependency compilation separately from source.
COPY Cargo.toml Cargo.lock rust-toolchain.toml ./
COPY bidder-core/Cargo.toml   bidder-core/Cargo.toml
COPY bidder-server/Cargo.toml bidder-server/Cargo.toml
COPY bidder-bench/Cargo.toml  bidder-bench/Cargo.toml

# Stub sources so cargo can resolve and compile deps.
RUN mkdir -p bidder-core/src bidder-server/src bidder-bench/src bidder-bench/benches \
 && echo "pub fn _stub() {}" > bidder-core/src/lib.rs \
 && echo "fn main() {}"      > bidder-server/src/main.rs \
 && echo "pub fn _stub() {}" > bidder-bench/src/lib.rs \
 && echo 'use criterion::{criterion_group,criterion_main,Criterion}; fn b(_c:&mut Criterion){} criterion_group!(g,b); criterion_main!(g);' \
      > bidder-bench/benches/pipeline.rs

RUN cargo build --release -p bidder-server

# Remove stub artifacts so real sources link fresh.
RUN cargo clean -p bidder-core -p bidder-server

# Copy real sources and build.
COPY bidder-core/   bidder-core/
COPY bidder-server/ bidder-server/
COPY config.toml    ./

RUN cargo build --release -p bidder-server

# ── runtime ──────────────────────────────────────────────────────────────────
FROM gcr.io/distroless/cc-debian12:nonroot AS runtime

COPY --from=builder /build/target/release/bidder-server /bidder-server
COPY --from=builder /build/config.toml                  /config.toml

EXPOSE 8080 9090

ENTRYPOINT ["/bidder-server"]
