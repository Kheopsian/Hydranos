# syntax=docker/dockerfile:1
# Stage 1: Typhon engine builder — the BitTorrent engine.
FROM rust:1-bookworm AS typhon-builder

WORKDIR /build/typhon-engine
COPY typhon-engine/Cargo.toml typhon-engine/Cargo.lock ./
COPY typhon-engine/src ./src
COPY typhon-engine/benches ./benches
# Vendored crate referenced by [patch.crates-io] path = "../third_party/...".
COPY third_party /build/third_party
ENV RUSTFLAGS="--cfg tokio_unstable"
# Without these caches, changing one line of src/ recompiled all 205 crates --
# some 200 of them third-party dependencies that never move -- for about 30
# minutes per build.
# The binary has to be copied OUT of the cache in the same RUN: a cache mount
# does not exist in the final layer, so the runtime stage cannot read from it.
RUN --mount=type=cache,target=/usr/local/cargo/registry,sharing=locked \
    --mount=type=cache,target=/build/typhon-engine/target,sharing=locked \
    cargo build --release --bin typhon-engine \
    && cp target/release/typhon-engine /usr/local/bin/typhon-engine

# Stage 2: Go builder.
FROM golang:1.25-bookworm AS go-builder

WORKDIR /build
COPY . .
RUN --mount=type=cache,target=/root/.cache/go-build \
    CGO_ENABLED=0 go build -mod=vendor -ldflags="-s -w" -o /hydra ./cmd/hydra

# Stage 3: Runtime.
FROM debian:bookworm-slim

RUN apt-get update && apt-get install -y --no-install-recommends \
    ca-certificates iperf3 iproute2 tzdata libssl3 \
    wireguard-tools gosu libcap2-bin \
    && rm -rf /var/lib/apt/lists/*

# jemalloc heap profiling on the Rust engine (low-overhead sampling every
# 512KB). Dumps triggered on SIGUSR1 by the watchdog. Go (hydra) ignores it.
ENV MALLOC_CONF=prof:true,prof_active:true,lg_prof_sample:19,prof_prefix:/config/jeprof
COPY --from=typhon-builder /usr/local/bin/typhon-engine /usr/local/bin/hydra-engine

COPY --from=go-builder /hydra /usr/local/bin/hydra
COPY configs/ /app/configs/
COPY entrypoint.sh /usr/local/bin/entrypoint.sh
RUN chmod +x /usr/local/bin/entrypoint.sh

# Unprivileged account used when PUID/PGID are set (see entrypoint.sh). The
# container still runs as root by default, so existing setups are unchanged.
RUN groupadd -g 1000 hydra \
 && useradd -u 1000 -g 1000 -d /config -s /usr/sbin/nologin hydra

WORKDIR /app
ENV GOMEMLIMIT=8GiB
ENTRYPOINT ["/usr/local/bin/entrypoint.sh"]
