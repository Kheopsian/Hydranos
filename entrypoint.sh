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
# HYDRANOS_HEAP_PROFILE=off: no jemalloc heap profiling. The image turns it on
# through MALLOC_CONF (sampling every 512 KiB, a dump on SIGUSR1); this drops the
# prof* settings from it and keeps the others. Unset, MALLOC_CONF is untouched.
if [ "${HYDRANOS_HEAP_PROFILE:-}" = "off" ] && [ -n "${MALLOC_CONF:-}" ]; then
    MALLOC_CONF=$(printf '%s' "$MALLOC_CONF" | tr ',' '\n' | grep -v '^\(lg_\)\{0,1\}prof' | paste -sd, - || true)
    if [ -n "$MALLOC_CONF" ]; then export MALLOC_CONF; else unset MALLOC_CONF; fi
fi
# First run: seed the config so an empty volume just works.
if [ ! -f "$CFG_DIR/default.toml" ]; then
    mkdir -p "$CFG_DIR"
    cp /app/configs/default.toml "$CFG_DIR/default.toml"
    # Keep data_dir consistent with the chosen config directory.
    sed -i "s#^data_dir = .*#data_dir = \"$CFG_DIR\"#" "$CFG_DIR/default.toml"
    # The same mark the daemon puts in a config it writes itself: torrents
    # already in the store stay off the network until the network mode is
    # chosen, rather than leaving by the default route of a config that was
    # lost and recreated.
    if grep -q '^\[network\]' "$CFG_DIR/default.toml"; then
        sed -i '/^\[network\]/a mode_unconfirmed = true' "$CFG_DIR/default.toml"
    else
        printf '\n[network]\nmode_unconfirmed = true\n' >> "$CFG_DIR/default.toml"
    fi
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
    # Capabilities lost when we drop privileges, kept as AMBIENT ones
    # (inherited by `ip` and `wg`) whenever the container was granted them
    # (--cap-add, or --privileged): granting one is the ask. A container that
    # ran as root before 4.5 kept them the same way.
    #   NET_ADMIN  managed WireGuard (the Network tab's WireGuard mode) runs
    #              `ip` and `wg`, and fwmark-based VPN routing sets SO_MARK.
    #              HYDRANOS_CAP_NET_ADMIN=0 drops it anyway.
    #   SYS_ADMIN  "TRIM after drain" on a race volume (the FITRIM ioctl).
    #              Only NET_ADMIN used to be kept, so a container given
    #              --cap-add=SYS_ADMIN still had every TRIM refused once it
    #              ran as PUID. HYDRANOS_CAP_SYS_ADMIN=0 drops it anyway.
    KEEP=""
    if command -v capsh >/dev/null 2>&1; then
        for cap in net_admin sys_admin; do
            case "$cap" in
                net_admin) [ "$HYDRANOS_CAP_NET_ADMIN" = "0" ] && continue ;;
                sys_admin) [ "$HYDRANOS_CAP_SYS_ADMIN" = "0" ] && continue ;;
            esac
            # Probe each before exec'ing, so a container without it still
            # starts instead of dying on capsh.
            if capsh --caps="cap_$cap+eip cap_setuid+ep cap_setgid+ep" \
                --addamb="cap_$cap" -- -c true >/dev/null 2>&1; then
                KEEP="$KEEP $cap"
            fi
        done
    fi
    case " $KEEP " in
        *" net_admin "*) ;;
        *) if [ "$HYDRANOS_CAP_NET_ADMIN" = "1" ]; then
               echo "hydranos: CAP_NET_ADMIN not available, add --cap-add=NET_ADMIN for managed WireGuard and fwmark routing"
           fi ;;
    esac
    if [ -n "$KEEP" ]; then
        CAPS="cap_setuid+ep cap_setgid+ep"
        AMB=""
        for cap in $KEEP; do
            CAPS="cap_$cap+eip $CAPS"
            AMB="${AMB:+$AMB,}cap_$cap"
        done
        echo "hydranos: keeping$KEEP"
        exec capsh --caps="$CAPS" --keep=1 --user="$EXIST_USR" --addamb="$AMB" \
            -- -c 'exec hydranos "$@"' hydranos "$@"
    fi
    exec gosu "$RUN_UID:$RUN_GID" hydranos "$@"
fi
exec hydranos "$@"
