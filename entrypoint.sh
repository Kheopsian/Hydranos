#!/bin/sh
set -e
# First run: seed the config so an empty /configs volume just works.
if [ ! -f /configs/default.toml ]; then
    mkdir -p /configs
    cp /app/configs/default.toml /configs/default.toml
    echo "hydra: seeded /configs/default.toml from image defaults (first run)"
fi
# One socket per torrent -> raise the fd limit (needs privileged / SYS_RESOURCE;
# falls back quietly otherwise).
ulimit -n 1000000 2>/dev/null || ulimit -n 200000 2>/dev/null || true
exec hydra --config /configs/default.toml "$@"
