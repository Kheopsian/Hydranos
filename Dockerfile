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
    cargo build --release --bin hydra \
    && cp target/release/hydra /usr/local/bin/hydra

# Stage 2: Runtime.
FROM debian:bookworm-slim

RUN apt-get update && apt-get install -y --no-install-recommends \
    ca-certificates iperf3 iproute2 tzdata libssl3 \
    wireguard-tools gosu libcap2-bin \
    && rm -rf /var/lib/apt/lists/*

# jemalloc heap profiling (low-overhead sampling every 512KB). Dumps triggered
# on SIGUSR1 by the watchdog. One process now, so this covers all of it.
ENV MALLOC_CONF=prof:true,prof_active:true,lg_prof_sample:19,prof_prefix:/config/jeprof
# One binary. The front and the engines are the same process in 4.0.0, so
# there is no hydra-engine to ship and no unix socket between them.
COPY --from=typhon-builder /usr/local/bin/hydra /usr/local/bin/hydra
COPY configs/ /app/configs/
COPY entrypoint.sh /usr/local/bin/entrypoint.sh
RUN chmod +x /usr/local/bin/entrypoint.sh

# Unprivileged account used when PUID/PGID are set (see entrypoint.sh). The
# container still runs as root by default, so existing setups are unchanged.
RUN groupadd -g 1000 hydra \
 && useradd -u 1000 -g 1000 -d /config -s /usr/sbin/nologin hydra

WORKDIR /app
ENTRYPOINT ["/usr/local/bin/entrypoint.sh"]
