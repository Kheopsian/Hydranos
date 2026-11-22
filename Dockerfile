# syntax=docker/dockerfile:1
# Stage 1: Typhon engine builder — the BitTorrent engine.
FROM rust:1-bookworm@sha256:e12566d5abc17a0da741628782cdb504b68b8961f5bfcf677936bf3e8b32de29 AS typhon-builder

WORKDIR /build/typhon-engine
ENV RUSTFLAGS="--cfg tokio_unstable"

# Dependencies first, in a layer of their own. The manifest, the lock file and
# the vendored crates (referenced by [patch.crates-io] path = "../third_party/...")
# are all it needs; every target the manifest names gets an empty stand-in so
# cargo accepts it. This layer only rebuilds when one of those inputs changes.
#
# It used to be a RUN --mount=type=cache over target/. That kept local rebuilds
# quick, but a cache mount lives on one builder: CI exports layers to the GHA
# cache, never mounts, so every release runner started empty and recompiled
# all ~360 crates in Cargo.lock for a change under src/. A layer is what the
# GHA cache carries from one run to the next.
COPY typhon-engine/Cargo.toml typhon-engine/Cargo.lock ./
COPY third_party /build/third_party
RUN mkdir -p src/hydra src/update benches \
    && touch src/lib.rs \
    && echo 'fn main() {}' > src/hydra/main.rs \
    && echo 'fn main() {}' > src/update/main.rs \
    && echo 'fn main() {}' > benches/rate_tracker.rs \
    && echo 'fn main() {}' > benches/torrent_to_json.rs \
    && cargo build --release --locked --bin hydranos \
    && rm -rf src benches

# Baked into the binary by include_str!: the changelog the API serves at
# /changelog.md, and the default config a first run writes. They live at the
# repository root, so the builder needs them even though it builds one crate.
# They come after the dependency layer, so editing one never invalidates it.
COPY CHANGELOG.md /build/CHANGELOG.md
# Baked in like the changelog and the front: /install.sh is served to a machine
# that is about to become a node, so it has to travel inside the binary.
COPY install.sh /build/install.sh
COPY configs /build/configs
# The interface, baked into the binary by include_str!/include_bytes! the way
# the Go embedded it. Without this the build fails at compile time, which is
# the good failure -- the one where a page is missing at runtime cost a
# production rollback tonight.
COPY web /build/web
COPY typhon-engine/src ./src
COPY typhon-engine/benches ./benches
# COPY keeps the files' own modification times, and a checkout can be older
# than the stand-ins compiled above: cargo would then call the stub build of
# this crate fresh and ship an empty program. Touching the crate roots makes
# it rebuild; the dependencies stay compiled.
# ⚠ The binary is hydranos, not hydranosnos. The rename was applied with a
# blunt s/hydra/hydranos/ that also rewrote the already-renamed name, while
# entrypoint.sh execs `hydranos`: v4.1.0 through v4.1.4 all built an image
# that exited instantly with "executable file not found in $PATH".
RUN touch src/lib.rs src/hydra/main.rs \
    && cargo build --release --locked --bin hydranos \
    && cp target/release/hydranos /usr/local/bin/hydranos

# Stage 2: Runtime.
FROM debian:bookworm-slim@sha256:7ff6ba31527b9e336162fa061746c366c4b2032f4d2cff2078bea43f78efe3f8

RUN apt-get update && apt-get install -y --no-install-recommends \
    ca-certificates curl iperf3 iproute2 tzdata libssl3 \
    wireguard-tools gosu libcap2-bin \
    && rm -rf /var/lib/apt/lists/*

# jemalloc heap profiling (low-overhead sampling every 512KB). Dumps triggered
# on SIGUSR1 by the watchdog. One process now, so this covers all of it.
# HYDRANOS_HEAP_PROFILE=off (container environment) drops the prof* settings at
# start-up, see entrypoint.sh; unset, this is what runs.
ENV MALLOC_CONF=prof:true,prof_active:true,lg_prof_sample:19,prof_prefix:/config/jeprof
# One binary. The front and the engines are the same process in 4.0.0, so
# there is no hydranos-engine to ship and no unix socket between them.
COPY --from=typhon-builder /usr/local/bin/hydranos /usr/local/bin/hydranos
COPY configs/ /app/configs/
COPY entrypoint.sh /usr/local/bin/entrypoint.sh
RUN chmod +x /usr/local/bin/entrypoint.sh

# The account the daemon runs as: uid/gid 1000 unless PUID/PGID say otherwise
# (see entrypoint.sh). The entrypoint starts as root only to hand /config over
# and drop to it; PUID=0 keeps root.
RUN groupadd -g 1000 hydranos \
 && useradd -u 1000 -g 1000 -d /config -s /usr/sbin/nologin hydranos

WORKDIR /app
ENTRYPOINT ["/usr/local/bin/entrypoint.sh"]
