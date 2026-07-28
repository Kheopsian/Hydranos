#!/bin/sh
set -e
# Config directory: holds the TOML config, resume data and the SQLite store.
# Defaults to /config (linuxserver/*arr convention). Override with
# HYDRA_CONFIG_DIR to relocate it (e.g. legacy setups mounting /configs).
CFG_DIR="${HYDRA_CONFIG_DIR:-/config}"
# First run: seed the config so an empty volume just works.
if [ ! -f "$CFG_DIR/default.toml" ]; then
    mkdir -p "$CFG_DIR"
    cp /app/configs/default.toml "$CFG_DIR/default.toml"
    # Keep data_dir consistent with the chosen config directory.
    sed -i "s#^data_dir = .*#data_dir = \"$CFG_DIR\"#" "$CFG_DIR/default.toml"
    echo "hydra: seeded $CFG_DIR/default.toml from image defaults (first run)"
fi
# One socket per torrent -> raise the fd limit (needs privileged / SYS_RESOURCE;
# falls back quietly otherwise).
ulimit -n 1000000 2>/dev/null || ulimit -n 200000 2>/dev/null || true
exec hydra --config "$CFG_DIR/default.toml" "$@"
