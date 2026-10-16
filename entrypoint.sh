#!/bin/sh
set -e
# Config directory: holds the TOML config, resume data and the SQLite store.
# Defaults to /config (linuxserver/*arr convention). Override with
# HYDRANOS_CONFIG_DIR to relocate it (e.g. legacy setups mounting /configs).
CFG_DIR="${HYDRANOS_CONFIG_DIR:-/config}"
# 3.x agent containers took their engine identity from these variables and ran
# with no config file. 4.x has no agent mode: dropping --config for them started
# a daemon on built-in defaults, with no file to show for it and nothing said.
# They are refused -- named, not honoured -- and the container starts as a
# normal instance on its own config, which is what the binary does with the
# matching 3.x flags (--agent-only and friends).
refuse_legacy_env() {
    echo "hydranos: ignoring $1: the 3.x agent mode was removed in 4.x and Hydranos" \
         "now runs as one process. Starting a normal instance on $CFG_DIR/default.toml." \
         "To spread torrents over several machines, enrol each one as a node instead" \
         "(install.sh --register-to <url> --token <token>; the Nodes page gives the full" \
         "command). Remove $1 from the container's environment to silence this." >&2
}
if [ -n "$HYDRANOS_ENGINE_ID" ]; then refuse_legacy_env HYDRANOS_ENGINE_ID; fi
if [ -n "$HYDRANOS_ENGINES" ]; then refuse_legacy_env HYDRANOS_ENGINES; fi
# First run: seed the config so an empty volume just works.
if [ ! -f "$CFG_DIR/default.toml" ]; then
    mkdir -p "$CFG_DIR"
    cp /app/configs/default.toml "$CFG_DIR/default.toml"
    # Keep data_dir consistent with the chosen config directory.
    sed -i "s#^data_dir = .*#data_dir = \"$CFG_DIR\"#" "$CFG_DIR/default.toml"
    echo "hydranos: seeded $CFG_DIR/default.toml from image defaults (first run)"
fi
# Always a --config, quoted as one argv entry: a config directory containing a
# space must not split into two arguments and start hydranos on a truncated path.
set -- --config "$CFG_DIR/default.toml" "$@"
# One socket per torrent -> raise the fd limit (needs privileged / SYS_RESOURCE;
# falls back quietly otherwise). Done before dropping privileges so the limit is
# inherited by the unprivileged process.
# shellcheck disable=SC3045 # ulimit -n is outside POSIX but busybox ash, which
# is what runs this in the Alpine image, implements it; the || true already
# covers a shell that does not.
ulimit -n 1000000 2>/dev/null || ulimit -n 200000 2>/dev/null || true

# PUID/PGID (linuxserver convention): the uid/gid the daemon runs as, 1000 when
# unset, so the files it writes are owned by your user and *arr can hardlink
# them. Like qBittorrent's image, it never runs as root unless asked: PUID=0
# keeps root, which is how every container ran before 4.5.
if [ "$(id -u)" != "0" ]; then
    if [ -n "$PUID" ] || [ -n "$PGID" ]; then
        echo "hydranos: already running as uid $(id -u), ignoring PUID/PGID"
    fi
elif [ "${PUID:-1000}" = "0" ]; then
    echo "hydranos: PUID=0, running as root"
else
    RUN_UID="${PUID:-1000}"
    RUN_GID="${PGID:-1000}"
    # Re-point the baked-in hydranos account, reusing an existing account when
    # the id is already taken (uid 99/gid 100 on Unraid map to nobody/users).
    EXIST_GRP="$(getent group "$RUN_GID" | cut -d: -f1)"
    if [ -z "$EXIST_GRP" ]; then
        groupmod -o -g "$RUN_GID" hydranos
        EXIST_GRP=hydranos
    fi
    EXIST_USR="$(getent passwd "$RUN_UID" | cut -d: -f1)"
    if [ -z "$EXIST_USR" ]; then
        usermod -o -u "$RUN_UID" -g "$RUN_GID" hydranos
        EXIST_USR=hydranos
    fi
    # The config directory is small (config, resume data, SQLite store), so
    # a recursive chown is cheap. Of the payload directory only the folder
    # itself is handed over, so a fresh volume is writable: what is inside can
    # hold millions of files, and its ownership is yours to manage (folders an
    # older root container created there are left for you to chown).
    # Skip with HYDRANOS_SKIP_CHOWN=1 if you already got the permissions right.
    if [ "$HYDRANOS_SKIP_CHOWN" != "1" ]; then
        chown -R "$RUN_UID:$RUN_GID" "$CFG_DIR" 2>/dev/null || \
            echo "hydranos: could not chown $CFG_DIR, continuing"
        if [ -d /data ]; then
            chown "$RUN_UID:$RUN_GID" /data 2>/dev/null || \
                echo "hydranos: could not chown /data, continuing"
        fi
    fi
    echo "hydranos: dropping privileges to $RUN_UID:$RUN_GID ($EXIST_USR:$EXIST_GRP)"
    # Managed WireGuard (the Network tab's WireGuard mode) runs `ip` and
    # `wg`, and fwmark-based VPN routing sets SO_MARK: both need
    # CAP_NET_ADMIN. It is lost when we drop privileges, so it is kept as an
    # AMBIENT capability -- inherited by `ip` and `wg` -- whenever the
    # container was granted it (--cap-add=NET_ADMIN): granting it is the ask.
    # A container that ran as root before 4.5 kept it the same way.
    # HYDRANOS_CAP_NET_ADMIN=0 drops it anyway.
    if [ "$HYDRANOS_CAP_NET_ADMIN" != "0" ] && command -v capsh >/dev/null 2>&1; then
        # The container only has CAP_NET_ADMIN if it was granted one
        # (--cap-add=NET_ADMIN or --privileged). Probe before exec'ing, so a
        # container without it still starts instead of dying on capsh.
        if capsh --caps="cap_net_admin+eip cap_setuid+ep cap_setgid+ep" \
            --addamb=cap_net_admin -- -c true >/dev/null 2>&1; then
            exec capsh --caps="cap_net_admin+eip cap_setuid+ep cap_setgid+ep" \
                --keep=1 --user="$EXIST_USR" --addamb=cap_net_admin \
                -- -c 'exec hydranos "$@"' hydranos "$@"
        fi
        if [ "$HYDRANOS_CAP_NET_ADMIN" = "1" ]; then
            echo "hydranos: CAP_NET_ADMIN not available, add --cap-add=NET_ADMIN for managed WireGuard and fwmark routing"
        fi
    fi
    exec gosu "$RUN_UID:$RUN_GID" hydranos "$@"
fi
exec hydranos "$@"
