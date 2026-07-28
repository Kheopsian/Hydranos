#!/bin/sh
set -e
# First run: seed the config so an empty /config volume just works.
if [ ! -f /config/default.toml ]; then
    mkdir -p /config
    cp /app/configs/default.toml /config/default.toml
    echo "hydra: seeded /config/default.toml from image defaults (first run)"
fi
# One socket per torrent -> raise the fd limit (needs privileged / SYS_RESOURCE;
# falls back quietly otherwise).
ulimit -n 1000000 2>/dev/null || ulimit -n 200000 2>/dev/null || true
exec hydra --config /config/default.toml "$@"
