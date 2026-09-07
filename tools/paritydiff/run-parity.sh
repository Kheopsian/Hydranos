#!/bin/bash
# Build the Rust daemon, stand it up next to the Go reference on the SAME data,
# and compare them.
#
# Runs on Orion. Both daemons sit on the "v4bench" docker network, created
# --internal: no route off the host. That is deliberate. The bench replays real
# production data, and an instance able to reach a tracker would announce those
# torrents a second time from the house IP. The engines are additionally started
# paused with DHT, PEX and webseed off, so two independent things must fail
# before a packet leaves.
#
# Both instances are rebuilt from the fixture on every run. A bench that keeps
# state between runs stops measuring the code and starts measuring the leftovers.
#
#   run-parity.sh [--rebuild] [--requests <file>]
#
# The cargo target sits on a tmpfs (/mnt/v4build), not in a docker volume:
# volumes live on ZFS with block cloning on, and coreutils >= 9 uses
# copy_file_range, which wedges jemalloc's configure forever in D state.
# Measured: 45 minutes of nothing on ZFS, 1 minute of real work on tmpfs.
#   mount -t tmpfs -o size=24g,mode=0755 tmpfs /mnt/v4build
set -euo pipefail

# Only one bench at a time.
#
# Both runners drive containers with FIXED names (v4-go-a, v4-rust). Two runs
# overlapping therefore destroy each other's daemons mid-campaign, and the
# report reads as "the candidate is missing 57 routes" when the truth is that
# the reference was torn down under it. That cost a full investigation.
#
# flock, not a PID file: a run killed mid-flight releases the lock by dying.
exec 9>/tmp/v4bench.lock
if ! flock -n 9; then
  echo "un autre banc tourne deja (/tmp/v4bench.lock) -- abandon" >&2
  exit 3
fi


REPO=/mnt/cache/appdata/hydra-v4
STAGING=/mnt/cache/appdata/hydra-v4-staging
TOOLS=$REPO/tools/paritydiff
NET=v4bench
KEY=change-me-in-production
REQUESTS=requests.json
REBUILD=0

while [ $# -gt 0 ]; do
  case "$1" in
    --rebuild)  REBUILD=1 ;;
    --requests) REQUESTS="$2"; shift ;;
    *) echo "unknown argument: $1" >&2; exit 2 ;;
  esac
  shift
done

# A killed session leaves its container behind, and the NAME then blocks every
# later run with a docker rc=125 that looks like a daemon failure. Removing the
# name first costs nothing and makes the bench restartable after any crash.
prune_name() { docker rm -f "$1" >/dev/null 2>&1 || true; }

step() { printf '\n=== %s\n' "$*"; }

if [ "$REBUILD" = 1 ]; then
  step "building the hydra binary"
  prune_name v4-cargo
  docker run --rm --name v4-cargo --init \
    -v "$REPO":/build -w /build/typhon-engine \
    -v /mnt/v4build/registry:/usr/local/cargo/registry \
    -v /mnt/v4build/target:/build/typhon-engine/target \
    -e RUSTFLAGS="--cfg tokio_unstable" \
    rust:1-bookworm cargo build --bin hydra

  step "running the Rust unit tests"
  prune_name v4-cargo-test
  docker run --rm --name v4-cargo-test --init \
    -v "$REPO":/build -w /build/typhon-engine \
    -v /mnt/v4build/registry:/usr/local/cargo/registry \
    -v /mnt/v4build/target:/build/typhon-engine/target \
    -e RUSTFLAGS="--cfg tokio_unstable" \
    rust:1-bookworm cargo test --bin hydra
fi

# --- fresh, identical state for both sides -------------------------------
# tar rather than cp: cp goes through copy_file_range, which ZFS turns into
# block cloning and which has already hung this machine's builds.
seed() {
  local dir="$1"
  rm -rf "$dir"; mkdir -p "$dir"
  cp "$STAGING/default.toml" "$dir/default.toml"
  if [ -d "$STAGING/fixture" ]; then
    tar -C "$STAGING/fixture" -cf - . | tar -C "$dir" -xf -
  fi
}

# The candidate must not put the production catalogue on the wire. `net = false`
# holds every engine offline: state loaded, no listener, no announce, no DHT.
# The --internal docker network is the second lock, not the only one -- a bench
# that leaks announces is one that tells 244k torrents' trackers about a machine
# nobody meant to publish.
offline() {
  local toml="$1"
  python3 - "$toml" <<'EOF'
import re, sys
p = sys.argv[1]
t = open(p).read()
for section in ("race", "hoard"):
    t = re.sub(r"(?m)^\[%s\]\s*$" % section, "[%s]\nnet = false" % section, t)
open(p, "w").write(t)
EOF
}

step "seeding both instances from the fixture"
seed "$STAGING/go"
seed "$STAGING/rust"
offline "$STAGING/rust/default.toml"

# Both sides must stat the SAME filesystem for /api/drain/status to be
# comparable. 3.x creates its race path at startup, so its container reports the
# overlay; a candidate that does not create it reports zeros. Rather than copy
# that mkdir -- creating a missing mount point is how an unmounted disk quietly
# fills the root filesystem -- the bench mounts one shared empty directory into
# both containers.
mkdir -p "$STAGING/racemount"

step "starting the Go reference"
docker rm -f v4-go-a >/dev/null 2>&1 || true
prune_name v4-go-a
docker run -d --name v4-go-a --network $NET \
  -e HYDRA_CONFIG_DIR=/configs \
  -v "$STAGING/go":/configs -v "$STAGING/go":/config \
  -v "$STAGING/racemount":/race \
  hydra-v4:goref >/dev/null

step "starting the Rust daemon"
docker rm -f v4-rust >/dev/null 2>&1 || true
# Run straight out of the cargo target: rebuilding an image per iteration would
# add minutes to a loop meant to be run constantly.
prune_name v4-rust
docker run -d --name v4-rust --network $NET \
  -v /mnt/v4build/target:/target:ro \
  -v "$STAGING/rust":/configs \
  -v "$STAGING/racemount":/race \
  -e RUST_LOG=info \
  --entrypoint /target/debug/hydra \
  rust:1-bookworm --config /configs/default.toml >/dev/null

# Both sides must be WARM before anything is compared. 3.x fills several of its
# caches on a timer -- the store sync that puts categories and completion dates
# on a row is one of them -- so a comparison taken six seconds after boot reads
# a reference that has not finished becoming itself, and reports differences
# that vanish a minute later. Measured: category and completed_time both
# "differed" on hundreds of rows purely from this.
# 110s, not 75 and not 45: /api/hoard/stats grows an "unseeded_peers" key only once a
# periodic computation has run, measured at ~55s on both Go instances. A 45s
# wait landed on the boundary, so the key was present on one side and absent on
# the other from one run to the next. Waiting past it makes the comparison
# deterministic instead of masking the field with an exclusion. 75 still landed
# on the wrong side of it now and then -- unseeded_peers and max_slots both
# appear late -- so the wait is longer than the slowest field observed, not
# merely longer than the average.
SETTLE=${SETTLE:-110}
echo "warming up for ${SETTLE}s before comparing"
sleep "$SETTLE"
for c in v4-go-a v4-rust; do
  if ! docker ps --filter "name=^${c}$" --format '{{.Names}}' | grep -q "$c"; then
    echo "$c did not stay up:" >&2
    docker logs "$c" 2>&1 | tail -20 >&2
    exit 1
  fi
done

step "torrents loaded on each side"
docker logs v4-rust 2>&1 | grep -c "engine state loaded" || true
docker logs v4-rust 2>&1 | grep "engine state loaded" || true

step "comparing Go (reference) against Rust (candidate)"
set +e
prune_name v4-differ
docker run --rm --name v4-differ --network $NET \
  -v "$TOOLS":/t -w /t python:3-slim \
  python3 -u paritydiff.py \
    --a http://v4-go-a:8199 \
    --b http://v4-rust:8199 \
    --key "$KEY" --requests "$REQUESTS" --timeout 8 \
    --json /t/report-rust.json
rc=$?
set -e

# A failure with no logs is a failure you get to reproduce from scratch. Both
# daemons are dumped before teardown so a crash mid-campaign -- which looks
# exactly like "the route is not ported yet" from the differ's side -- can be
# told apart from an actual missing route.
if [ $rc -ne 0 ]; then
  step "state of both daemons after the run"
  for c in v4-go-a v4-rust; do
    echo "--- $c: $(docker inspect -f '{{.State.Status}} exit={{.State.ExitCode}} oom={{.State.OOMKilled}}' "$c" 2>/dev/null)"
    docker logs --tail 25 "$c" 2>&1 | sed "s/^/    /"
  done
fi

step "stopping both daemons"
docker rm -f v4-rust v4-go-a >/dev/null 2>&1 || true

exit $rc
