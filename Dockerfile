# Stage 1: Typhon engine builder — the BitTorrent engine.
FROM rust:1-bookworm AS typhon-builder

WORKDIR /build/typhon-engine
COPY typhon-engine/Cargo.toml typhon-engine/Cargo.lock ./
COPY typhon-engine/src ./src
COPY typhon-engine/benches ./benches
ENV RUSTFLAGS="--cfg tokio_unstable"
RUN cargo build --release --bin typhon-engine

# Stage 2: Go builder.
FROM golang:1.25-bookworm AS go-builder

WORKDIR /build
COPY . .
RUN CGO_ENABLED=0 go build -mod=vendor -ldflags="-s -w" -o /hydra ./cmd/hydra

# Stage 3: Runtime.
FROM debian:bookworm-slim

RUN apt-get update && apt-get install -y --no-install-recommends \
    ca-certificates iperf3 iproute2 tzdata libssl3 \
    wireguard-tools \
    && rm -rf /var/lib/apt/lists/*

# jemalloc heap profiling on the Rust engine (low-overhead sampling every
# 512KB). Dumps triggered on SIGUSR1 by the watchdog. Go (hydra) ignores it.
ENV MALLOC_CONF=prof:true,prof_active:true,lg_prof_sample:19,prof_prefix:/config/jeprof
COPY --from=typhon-builder /build/typhon-engine/target/release/typhon-engine /usr/local/bin/hydra-engine

COPY --from=go-builder /hydra /usr/local/bin/hydra
COPY web/ /app/web/
COPY CHANGELOG.md /app/web/static/CHANGELOG.md
COPY configs/ /app/configs/
COPY entrypoint.sh /usr/local/bin/entrypoint.sh
RUN chmod +x /usr/local/bin/entrypoint.sh

WORKDIR /app
ENV GOMEMLIMIT=8GiB
ENTRYPOINT ["/usr/local/bin/entrypoint.sh"]
