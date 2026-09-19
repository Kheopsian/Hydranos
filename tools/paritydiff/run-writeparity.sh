#!/bin/bash
# Compare the WRITE half of the API: same mutations, both instances, then the
# store of each compared row by row.
#
#   run-writeparity.sh            Go against Rust
#   run-writeparity.sh --twin     Go against Go, to calibrate
#
# The twin run is not optional in spirit: it is what tells apart "the port is
# wrong" from "this column moves on its own". Every column silenced in
# writediff.py's VOLATILE_COLUMNS has to show up in a twin run first.
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
TWIN=0
[ "${1:-}" = "--twin" ] && TWIN=1

# A killed session leaves its container behind, and the NAME then blocks every
# later run with a docker rc=125 that looks like a daemon failure. Removing the
# name first costs nothing and makes the bench restartable after any crash.
prune_name() { docker rm -f "$1" >/dev/null 2>&1 || true; }

step() { printf '\n=== %s\n' "$*"; }

seed() {
  local dir="$1"
  rm -rf "$dir"; mkdir -p "$dir"
  cp "$STAGING/default.toml" "$dir/default.toml"
  tar -C "$STAGING/fixture" -cf - . | tar -C "$dir" -xf -
}

# The binary is ALWAYS rebuilt here. Running `cargo test` and then this script
# by hand once left the bench driving a stale daemon: the new routes answered
# 405, which reads exactly like "not ported yet". Building is a few seconds when
# nothing changed; guessing wrong costs a debugging round.
if [ "$TWIN" = 0 ]; then
  step "building the hydra binary"
  prune_name v4-cargo
  docker run --rm --name v4-cargo --init \
    -v "$REPO":/build -w /build/typhon-engine \
    -v /mnt/v4build/registry:/usr/local/cargo/registry \
    -v /mnt/v4build/target:/build/typhon-engine/target \
    -e RUSTFLAGS="--cfg tokio_unstable" \
    rust:1-bookworm cargo build --bin hydra
fi

step "seeding both instances"
mkdir -p "$STAGING/racemount"
seed "$STAGING/go"
seed "$STAGING/rust"

docker rm -f v4-go-a v4-rust >/dev/null 2>&1 || true

step "starting A (Go reference)"
prune_name v4-go-a
docker run -d --name v4-go-a --network $NET -e HYDRANOS_CONFIG_DIR=/configs \
  -v "$STAGING/go":/configs -v "$STAGING/go":/config -v "$STAGING/racemount":/race \
  hydra-v4:goref >/dev/null

if [ "$TWIN" = 1 ]; then
  step "starting B (a second Go, for calibration)"
  prune_name v4-rust
  docker run -d --name v4-rust --network $NET -e HYDRANOS_CONFIG_DIR=/configs \
    -v "$STAGING/rust":/configs -v "$STAGING/rust":/config -v "$STAGING/racemount":/race \
    hydra-v4:goref >/dev/null
else
  step "starting B (Rust candidate)"
  prune_name v4-rust
  docker run -d --name v4-rust --network $NET \
    -v /mnt/v4build/target:/target:ro -v "$STAGING/rust":/configs \
    -v "$STAGING/racemount":/race -e RUST_LOG=info -e HYDRANOS_ENGINE_NET=0 \
    --entrypoint /target/debug/hydra rust:1-bookworm --config /configs/default.toml >/dev/null
fi

# 110s, matching run-parity.sh. max_slots and unseeded_peers appear late on the
# reference, and 75 landed on the wrong side of them often enough to look like a
# regression. Wait longer than the SLOWEST field observed, not the average.
sleep "${SETTLE:-110}"

step "applying the mutations to both"
set +e
prune_name v4-writediff
docker run --rm --name v4-writediff --network $NET \
  -v "$TOOLS":/t -v "$STAGING/go":/dba -v "$STAGING/rust":/dbb -w /t python:3-slim \
  python3 -u writediff.py --a http://v4-go-a:8199 --b http://v4-rust:8199 \
    --key "$KEY" --scenarios scenarios.json \
    --db-a /dba/hydra.db --db-b /dbb/hydra.db
rc=$?
set -e

# A failure with no logs is a failure you get to reproduce from scratch.
if [ $rc -ne 0 ]; then
  step "state of both daemons after the run"
  for c in v4-go-a v4-rust; do
    echo "--- $c: $(docker inspect -f '{{.State.Status}} exit={{.State.ExitCode}} oom={{.State.OOMKilled}}' "$c" 2>/dev/null)"
    docker logs --tail 20 "$c" 2>&1 | sed "s/^/    /"
  done
fi

step "stopping both"
docker rm -f v4-go-a v4-rust >/dev/null 2>&1 || true
exit $rc
