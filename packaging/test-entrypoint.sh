#!/bin/sh
# Runs entrypoint.sh in a throwaway Debian container, as root like Docker does,
# with a stand-in hydranos that reports who it runs as. Checks that the daemon
# never runs as root unless PUID=0 asks for it, and what gets chowned.
#
#   sh packaging/test-entrypoint.sh           (needs docker and network for apt)
set -eu

if [ "${1:-}" != "--inside" ]; then
    root=$(cd "$(dirname "$0")/.." && pwd)
    exec docker run --rm -v "$root/entrypoint.sh":/src/entrypoint.sh:ro \
        -v "$root/packaging/test-entrypoint.sh":/src/test-entrypoint.sh:ro \
        debian:bookworm-slim sh /src/test-entrypoint.sh --inside
fi

apt-get update -qq >/dev/null && apt-get install -y -qq gosu >/dev/null 2>&1
useradd -u 1000 -U -M -s /usr/sbin/nologin hydranos
mkdir -p /app/configs /data/old
printf 'data_dir = "/config"\n' > /app/configs/default.toml
# shellcheck disable=SC2016 # expanded by the stand-in when it runs, not here
printf '#!/bin/sh\necho "$(id -u):$(id -g)"\n' > /usr/local/bin/hydranos
chmod 755 /usr/local/bin/hydranos

fail=0
check() { if [ "$2" = "$3" ]; then echo "ok   $1 = $3"; else echo "FAIL $1: expected $2, got $3"; fail=1; fi; }
run() { rm -rf /config; mkdir -p /config; chown -R 0:0 /data; env "$@" sh /src/entrypoint.sh 2>/dev/null | tail -1; }

check "no PUID/PGID" "1000:1000" "$(run)"
check "  /config handed over" 1000 "$(stat -c %u /config/default.toml)"
check "  /data itself handed over" 1000 "$(stat -c %u /data)"
check "  what is inside /data left alone" 0 "$(stat -c %u /data/old)"
check "PUID=99 PGID=100" "99:100" "$(run PUID=99 PGID=100)"
check "PUID=0 PGID=0 keeps root" "0:0" "$(run PUID=0 PGID=0)"
exit $fail
