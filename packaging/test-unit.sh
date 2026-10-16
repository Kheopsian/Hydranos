#!/bin/sh
# Rates packaging/hydranos.service with `systemd-analyze security` in a
# throwaway Debian container and fails above 5.0 (systemd's "OK" band).
#
#   sh packaging/test-unit.sh                  (needs docker and network for apt)
#   sh packaging/test-unit.sh --run <hydranos> also boots systemd in a privileged
#       container, starts that binary under the unit and asks /health, plain and
#       with the NET_ADMIN drop-in the unit documents
set -eu

here=$(cd "$(dirname "$0")" && pwd)
THRESHOLD=50 # systemd-analyze counts tenths: 50 is an exposure of 5.0

if [ "${1:-}" = "--inside" ]; then
    apt-get update -qq >/dev/null && apt-get install -y -qq systemd >/dev/null 2>&1
    cp /pkg/hydranos.service /tmp/hydranos.service
    systemd-analyze security --offline=true /tmp/hydranos.service | tail -1
    if systemd-analyze security --offline=true --threshold="$THRESHOLD" /tmp/hydranos.service >/dev/null 2>&1; then
        echo "ok   exposure at or under 5.0"
    else
        echo "FAIL exposure above 5.0"
        exit 1
    fi
    exit 0
fi

docker run --rm -v "$here":/pkg:ro debian:bookworm-slim sh /pkg/test-unit.sh --inside

[ "${1:-}" = "--run" ] || exit 0
bin=$2
name="hydranos-unit-test-$$"
fail=0
check() { if [ "$2" = "$3" ]; then echo "ok   $1 = $3"; else echo "FAIL $1: expected $2, got $3"; fail=1; fi; }

docker run -d --name "$name" --privileged --cgroupns=host -v /sys/fs/cgroup:/sys/fs/cgroup:rw \
    -v "$here":/pkg:ro -v "$bin":/bin-under-test:ro debian:bookworm-slim \
    sh -c 'apt-get update -qq >/dev/null && apt-get install -y -qq systemd systemd-sysv curl >/dev/null 2>&1 && exec /lib/systemd/systemd' >/dev/null
trap 'docker rm -f "$name" >/dev/null 2>&1' EXIT
i=0; until docker exec "$name" systemctl is-system-running 2>/dev/null | grep -qE 'running|degraded' || [ $i -ge 120 ]; do i=$((i + 1)); sleep 1; done

docker exec "$name" sh -c '
    useradd --system --home-dir /var/lib/hydranos --shell /usr/sbin/nologin hydranos
    install -d -o root -g root -m 0755 /opt/hydranos
    install -d -o hydranos -g hydranos /etc/hydranos /var/lib/hydranos
    install -m 0755 /bin-under-test /opt/hydranos/hydranos
    printf "[daemon]\napi_host = \"127.0.0.1\"\napi_port = 8199\napi_key = \"\"\ndata_dir = \"/var/lib/hydranos\"\n" > /etc/hydranos/default.toml
    chown hydranos:hydranos /etc/hydranos/default.toml; chmod 600 /etc/hydranos/default.toml
    install -m 0644 /pkg/hydranos.service /etc/systemd/system/hydranos.service
    systemctl daemon-reload'

up() { # up <label>: start, wait for /health, report
    docker exec "$name" systemctl restart hydranos
    i=0; code=000
    while [ $i -lt 90 ]; do
        code=$(docker exec "$name" curl -s -o /dev/null -w '%{http_code}' http://127.0.0.1:8199/health || true)
        [ "$code" = 200 ] && break
        i=$((i + 1)); sleep 1
    done
    check "$1: /health" 200 "$code"
    [ "$code" = 200 ] || docker exec "$name" journalctl -u hydranos --no-pager -n 30
}

up "hardened unit"
check "API key written back into /etc/hydranos" yes \
    "$(docker exec "$name" sh -c 'grep -q "^api_key = \"[0-9a-f]\{16,\}\"" /etc/hydranos/default.toml && echo yes || echo no')"

docker exec "$name" sh -c 'mkdir -p /etc/systemd/system/hydranos.service.d &&
    printf "[Service]\nAmbientCapabilities=CAP_NET_ADMIN\nCapabilityBoundingSet=CAP_NET_ADMIN\n" > /etc/systemd/system/hydranos.service.d/net-admin.conf &&
    systemctl daemon-reload'
up "with the NET_ADMIN drop-in"
check "bounding set with the drop-in" cap_net_admin \
    "$(docker exec "$name" systemctl show -p CapabilityBoundingSet --value hydranos)"

exit $fail
