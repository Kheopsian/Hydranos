#!/usr/bin/env bash
# Interoperability suite: Hydranos against software somebody else wrote.
#
#   tools/interop/run.sh
#
# Starts, on a throwaway Docker network:
#   - opentracker, the most deployed public tracker, over HTTP and UDP;
#   - Torrust Tracker in PRIVATE mode (keys, REST API);
#   - qBittorrent (libtorrent), the client most swarms are made of;
# runs the `interop_*` tests against them, and removes everything it started,
# pass or fail.
#
# What the tests check is read back from the OTHER side's state: opentracker's
# scrape, Torrust's peer table, and qBittorrent's own piece verification.
#   typhon-engine/src/hydra/announce/interop.rs   the announcer vs the trackers
#   typhon-engine/tests/interop_libtorrent.rs      the peer wire vs libtorrent
#
# Needs only Docker. Optional settings:
#   FILTER=name            run only the tests whose name contains this
#   CARGO_REGISTRY=/path   cargo registry cache to mount (default: a named volume)
#   CARGO_TARGET=/path     target directory to mount     (default: a named volume)
set -euo pipefail

here="$(cd "$(dirname "$0")" && pwd)"
repo="$(cd "$here/../.." && pwd)"
net="hydranos-interop-$$"
ot="hydranos-interop-ot-$$"
tr="hydranos-interop-torrust-$$"
qb="hydranos-interop-qbit-$$"
token="interop-admin-token" # must match tools/interop/torrust.toml

# Pinned by digest: a result is only worth quoting if the next run talks to the
# same software. Moving to a newer version is a deliberate edit here.
OPENTRACKER=lednerb/opentracker-docker@sha256:a055447e44450036b7b2a923618b12d4ecf1eff3c894e969cc2c42f466b2c6f6
TORRUST=torrust/tracker@sha256:5801a73010692b820da0d33ecd5505922a41b4c9df3060c89de100bcaa89aa68
QBITTORRENT=linuxserver/qbittorrent@sha256:2be038f3421f60f62e8e4bf201f66f385b68e4fbc9ed3ab79051069ea22e2650 # 5.2.3, libtorrent 2.0.14
CURL=curlimages/curl@sha256:58adaa4e8dca9c988bae2aba4ab3434a0bb2da16bbe3f92dec39ec7785166777
ALPINE=alpine@sha256:294b683cb724975bec92580e1e685676bd4b50bda910ddb8c51d4cabeaec77e6
RUST=rust:1-bookworm
scratch="$(mktemp -d)"

cleanup() {
    docker rm -f "$ot" "$tr" "$qb" >/dev/null 2>&1 || true
    docker network rm "$net" >/dev/null 2>&1 || true
    # Files written by root inside the containers: removed by a container.
    docker run --rm -v "$scratch":/s "$ALPINE" sh -c 'rm -rf /s/*' >/dev/null 2>&1 || true
    rm -rf "$scratch"
}
trap cleanup EXIT

docker network create "$net" >/dev/null
docker run -d --name "$ot" --network "$net" "$OPENTRACKER" >/dev/null
docker run -d --name "$tr" --network "$net" \
    -v "$here/torrust.toml:/etc/torrust/tracker/tracker.toml:ro" \
    "$TORRUST" >/dev/null

# qBittorrent with the WebUI open to this network only, DHT/PEX/LSD off so the
# only peer it ever meets is the one a test hands it. The config is copied:
# qBittorrent rewrites its file on exit.
mkdir -p "$scratch/qbit-config/qBittorrent" "$scratch/shared"
cp "$here/qbittorrent/qBittorrent.conf" "$scratch/qbit-config/qBittorrent/"
chmod -R 777 "$scratch"
docker run -d --name "$qb" --network "$net" \
    -e PUID=0 -e PGID=0 -e WEBUI_PORT=8080 -e TORRENTING_PORT=6881 \
    -v "$scratch/qbit-config":/config \
    -v "$scratch/shared":/interop \
    "$QBITTORRENT" >/dev/null

# All three must answer before a test may count on them. One still starting
# would make the first test fail for a reason that is not ours.
for i in $(seq 1 60); do
    if docker run --rm --network "$net" "$CURL" -sf -o /dev/null \
        "http://$tr:1313/health_check" 2>/dev/null \
        && docker run --rm --network "$net" "$CURL" -s -o /dev/null \
        "http://$ot:6969/stats" 2>/dev/null \
        && docker run --rm --network "$net" "$CURL" -sf -o /dev/null \
        "http://$qb:8080/api/v2/app/version" 2>/dev/null; then
        break
    fi
    [ "$i" = 60 ] && { echo "the interop services did not come up" >&2; docker logs "$tr" | tail -20 >&2; exit 1; }
    sleep 1
done

filter="${FILTER:-interop_}"
registry="${CARGO_REGISTRY:-hydranos-interop-registry}"
target="${CARGO_TARGET:-hydranos-interop-target}"

docker run --rm --network "$net" \
    -v "$repo":/build \
    -v "$registry":/usr/local/cargo/registry \
    -v "$target":/build/typhon-engine/target \
    -v "$scratch/shared":/interop \
    -w /build/typhon-engine \
    -e RUSTFLAGS="--cfg tokio_unstable" \
    -e HYDRANOS_INTEROP_OPENTRACKER="http://$ot:6969/announce" \
    -e HYDRANOS_INTEROP_OPENTRACKER_UDP="udp://$ot:6969/announce" \
    -e HYDRANOS_INTEROP_TORRUST="http://$tr:7070/announce" \
    -e HYDRANOS_INTEROP_TORRUST_API="http://$tr:1212" \
    -e HYDRANOS_INTEROP_TORRUST_TOKEN="$token" \
    -e HYDRANOS_INTEROP_QBIT="http://$qb:8080" \
    -e HYDRANOS_INTEROP_QBIT_PEER="$qb:6881" \
    -e HYDRANOS_INTEROP_SHARED=/interop \
    -e HYDRANOS_INTEROP_QBIT_SHARED=/interop \
    -e FILTER="$filter" \
    "$RUST" \
    sh -c 'cargo test --bin hydranos "$FILTER" -- --ignored --test-threads=1 \
        && cargo test --test interop_libtorrent "$FILTER" -- --ignored --test-threads=1 --nocapture'

