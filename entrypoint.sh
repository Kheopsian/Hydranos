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
# falls back quietly otherwise). Done before dropping privileges so the limit is
# inherited by the unprivileged process.
ulimit -n 1000000 2>/dev/null || ulimit -n 200000 2>/dev/null || true

# PUID/PGID (linuxserver convention): when either is set, run as that uid/gid so
# the files Hydra writes are owned by your user and *arr can hardlink them.
# Unset (the default) keeps the historical behaviour: everything runs as root.
if [ -n "$PUID" ] || [ -n "$PGID" ]; then
    if [ "$(id -u)" != "0" ]; then
        echo "hydra: already running as uid $(id -u), ignoring PUID/PGID"
    else
        RUN_UID="${PUID:-1000}"
        RUN_GID="${PGID:-1000}"
        # Re-point the baked-in hydra account, reusing an existing account when
        # the id is already taken (uid 99/gid 100 on Unraid map to nobody/users).
        EXIST_GRP="$(getent group "$RUN_GID" | cut -d: -f1)"
        if [ -z "$EXIST_GRP" ]; then
            groupmod -o -g "$RUN_GID" hydra
            EXIST_GRP=hydra
        fi
        EXIST_USR="$(getent passwd "$RUN_UID" | cut -d: -f1)"
        if [ -z "$EXIST_USR" ]; then
            usermod -o -u "$RUN_UID" -g "$RUN_GID" hydra
            EXIST_USR=hydra
        fi
        # The config directory is small (config, resume data, SQLite store), so
        # a recursive chown is cheap. The payload directory is NOT touched: it
        # can hold millions of files, and its ownership is yours to manage.
        # Skip with HYDRA_SKIP_CHOWN=1 if you already got the permissions right.
        if [ "$HYDRA_SKIP_CHOWN" != "1" ]; then
            chown -R "$RUN_UID:$RUN_GID" "$CFG_DIR" 2>/dev/null || \
                echo "hydra: could not chown $CFG_DIR, continuing"
        fi
        echo "hydra: dropping privileges to $RUN_UID:$RUN_GID ($EXIST_USR:$EXIST_GRP)"
        # Fwmark-based VPN routing sets SO_MARK, which needs CAP_NET_ADMIN. It is
        # lost when we drop privileges, so keep it as an ambient capability only
        # when asked for (HYDRA_CAP_NET_ADMIN=1) -- it is not needed otherwise.
        if [ "$HYDRA_CAP_NET_ADMIN" = "1" ] && command -v capsh >/dev/null 2>&1; then
            # The container only has CAP_NET_ADMIN if it was granted one
            # (--cap-add=NET_ADMIN or --privileged). Probe before exec'ing, so a
            # container without it still starts instead of dying on capsh.
            if capsh --caps="cap_net_admin+eip cap_setuid+ep cap_setgid+ep" \
                --addamb=cap_net_admin -- -c true >/dev/null 2>&1; then
                exec capsh --caps="cap_net_admin+eip cap_setuid+ep cap_setgid+ep" \
                    --keep=1 --user="$EXIST_USR" --addamb=cap_net_admin \
                    -- -c 'exec hydra --config "$0" "$@"' "$CFG_DIR/default.toml" "$@"
            fi
            echo "hydra: CAP_NET_ADMIN not available, add --cap-add=NET_ADMIN to keep fwmark routing"
        fi
        exec gosu "$RUN_UID:$RUN_GID" hydra --config "$CFG_DIR/default.toml" "$@"
    fi
fi
exec hydra --config "$CFG_DIR/default.toml" "$@"
