#!/bin/sh
# Hydranos node enrolment.
#
# Run on the machine that is to become a node. It installs Hydranos, gives it a
# freshly generated API key, starts it, and then REGISTERS ITSELF with the
# Hydranos that handed out the token.
#
# The direction is the point. The controlling Hydranos never opens a session here
# and never holds a credential for this machine, so compromising its API cannot
# become code execution on the fleet. The only authority that crosses the wire
# is a token that is single use and expires in thirty minutes.
#
#   curl -fsSL http://<hydranos>/install.sh | sh -s -- --register-to http://<hydranos> --token <token>
#
# Three ways to install, picked in this order unless --user forces the last:
#   docker  docker is on the PATH: a container, config in --dir (/opt/hydranos)
#   system  root without docker: /usr/local/bin and a system unit
#   user    neither root nor docker (a seedbox account), or --user: the binary
#           in ~/.local/bin, config and data under XDG, a systemd --user unit
set -eu

REGISTER_TO=""
TOKEN=""
NAME="$(hostname -s 2>/dev/null || hostname)"
PORT="8199"
DIR=""
CONTAINER="hydranos"
IMAGE=""
MODE=""

while [ $# -gt 0 ]; do
    case "$1" in
        --register-to) REGISTER_TO="$2"; shift 2 ;;
        --token)       TOKEN="$2";       shift 2 ;;
        --name)        NAME="$2";        shift 2 ;;
        --port)        PORT="$2";        shift 2 ;;
        --dir)         DIR="$2";         shift 2 ;;
        --container)   CONTAINER="$2";   shift 2 ;;
        --image)       IMAGE="$2";       shift 2 ;;
        --user)        MODE="user";      shift ;;
        *) echo "unknown option: $1" >&2; exit 2 ;;
    esac
done

[ -n "$REGISTER_TO" ] || { echo "--register-to is required" >&2; exit 2; }
[ -n "$TOKEN" ]       || { echo "--token is required" >&2; exit 2; }

if [ -z "$MODE" ]; then
    if command -v docker >/dev/null 2>&1; then
        MODE="docker"
    elif [ "$(id -u)" -eq 0 ]; then
        MODE="system"
    else
        # A seedbox account: no root and no docker. Before this mode the
        # script went on to write /usr/local/bin and /etc/systemd and died on
        # the first permission error, half installed.
        MODE="user"
    fi
fi

# data_dir is the path the DAEMON sees. In the container that is the mount
# point, not the host directory; everywhere else the two are the same. The
# binary install wrote "/configs" here too, a directory that does not exist
# outside the container.
case "$MODE" in
    user)
        if [ -n "$DIR" ]; then
            CFG_DIR="$DIR"
            DATA_DIR="$DIR/data"
        else
            CFG_DIR="${XDG_CONFIG_HOME:-$HOME/.config}/hydranos"
            DATA_DIR="${XDG_DATA_HOME:-$HOME/.local/share}/hydranos"
        fi
        BIN="$HOME/.local/bin/hydranos"
        CONFIG_DATA_DIR="$DATA_DIR"
        ;;
    *)
        CFG_DIR="${DIR:-/opt/hydranos}"
        DATA_DIR="$CFG_DIR/data"
        BIN="/usr/local/bin/hydranos"
        if [ "$MODE" = "docker" ]; then CONFIG_DATA_DIR="/configs"; else CONFIG_DATA_DIR="$DATA_DIR"; fi
        ;;
esac
CONFIG="$CFG_DIR/default.toml"

if [ "$MODE" = "system" ] && [ "$(id -u)" -ne 0 ]; then
    echo "the system install needs root: rerun with sudo, or pass --user" >&2
    exit 1
fi

# The address the CONTROLLER will use to reach this node. Taken from the route
# to the controller itself rather than from `hostname -I`: a machine with a
# tunnel and a LAN link has several addresses, and only one of them is the one
# the controller can come back on.
controller_host=$(echo "$REGISTER_TO" | sed -e 's|^[a-z]*://||' -e 's|[:/].*$||')
SELF_IP=$(ip route get "$(getent hosts "$controller_host" | awk '{print $1; exit}' || echo "$controller_host")" 2>/dev/null \
          | awk '{for (i=1;i<=NF;i++) if ($i=="src") {print $(i+1); exit}}')
[ -n "${SELF_IP:-}" ] || { echo "cannot work out which address to publish; pass --name and edit the node afterwards" >&2; exit 1; }

API_KEY=$(head -c 24 /dev/urandom | od -An -tx1 | tr -d ' \n')

# The node runs the controller's version, not whatever :latest is today: a
# re-run months later must not upgrade one node of the fleet behind the
# operator's back. HYDRANOS_VERSION_PIN overrides it.
CTRL_VER=$(curl -fsS -m 10 "$REGISTER_TO/health" 2>/dev/null | sed -n 's/.*"version": *"\([^"]*\)".*/\1/p' | head -1)
VER="${HYDRANOS_VERSION_PIN:-${CTRL_VER:+v$CTRL_VER}}"

echo "==> node   : $NAME"
echo "==> address: $SELF_IP:$PORT"
echo "==> mode   : $MODE"
echo "==> config : $CONFIG"
echo "==> data   : $DATA_DIR"

mkdir -p "$CFG_DIR" "$DATA_DIR"
if [ ! -f "$CONFIG" ]; then
    cat > "$CONFIG" <<TOML
[daemon]
api_host = "0.0.0.0"
api_port = $PORT
api_key = "$API_KEY"
data_dir = "$CONFIG_DATA_DIR"

[race]
listen_port = 16371
enable_ipv6 = true

[hoard]
listen_port = 16372
enable_ipv6 = true
TOML
    # The key in here is the node's whole authority. On a shared seedbox the
    # default umask leaves it readable by every other account on the host.
    if [ "$MODE" = "user" ]; then chmod 600 "$CONFIG"; fi
else
    # An existing install keeps its key: re-running enrolment must not lock the
    # operator out of a node they already had.
    API_KEY=$(grep -m1 '^api_key' "$CONFIG" | cut -d'"' -f2)
    echo "==> keeping the existing config and key"
fi

# Download the release binary into a temporary directory and print its path.
fetch_binary() {
    # The names the release publishes: hydranos-<tag>-linux-amd64.tar.gz.
    # 4.3 asked for x86_64-unknown-linux-musl, which no release has.
    case "$(uname -m)" in
        x86_64|amd64)  ARCH="amd64" ;;
        aarch64|arm64) ARCH="arm64" ;;
        *) echo "unsupported architecture: $(uname -m)" >&2; exit 1 ;;
    esac
    # The release tarballs are built against musl, so the binary carries its own
    # libc: nothing to install and nothing to match against the host distro.
    # ⚠ The asset name carries the version, so "latest/download/<name>" cannot
    # be spelled without knowing it. This asked for hydranos-$ARCH.tar.gz,
    # which no release has ever published: the binary install path answered
    # 404 from the day it was written. The tag comes from the API first.
    [ -n "$VER" ] || VER="$(curl -fsSL https://api.github.com/repos/Kheopsian/Hydranos/releases/latest | sed -n 's/.*"tag_name": *"\([^"]*\)".*/\1/p' | head -1)"
    [ -n "$VER" ] || { echo "could not resolve the latest release tag" >&2; exit 1; }
    URL="https://github.com/Kheopsian/Hydranos/releases/download/$VER/hydranos-$VER-linux-$ARCH.tar.gz"
    echo "==> fetching $URL" >&2
    tmp=$(mktemp -d)
    if ! curl -fsSL "$URL" -o "$tmp/hydranos.tar.gz"; then
        echo "could not download the release tarball" >&2
        rm -rf "$tmp"; exit 1
    fi
    tar -xzf "$tmp/hydranos.tar.gz" -C "$tmp"
    bin=$(find "$tmp" -maxdepth 2 -type f -name hydranos | head -1)
    [ -n "$bin" ] || { echo "no hydranos binary inside the tarball" >&2; rm -rf "$tmp"; exit 1; }
    # Moved to the top of the temporary directory so place_binary can remove
    # all of it from the path alone: this runs in a subshell, and nothing it
    # sets survives it.
    mv "$bin" "$tmp/.hydranos"
    echo "$tmp/.hydranos"
}

# Put the downloaded binary in place. Renamed over the old one rather than
# written into it: a running daemon keeps its inode, where writing into the
# file would fail with "text file busy".
place_binary() {
    mkdir -p "$(dirname "$BIN")"
    install -m 0755 "$1" "$BIN.new"
    mv -f "$BIN.new" "$BIN"
    rm -rf "$(dirname "$1")"
}

case "$MODE" in
docker)
    echo "==> installing with docker"
    [ -n "$IMAGE" ] || IMAGE="ghcr.io/kheopsian/hydranos:${VER:-latest}"
    echo "==> image  : $IMAGE"
    # Only a container this script made is replaced. 4.3 ran `docker rm -f
    # hydranos` whatever it was -- on the controller's own machine, that was
    # the controller.
    if docker ps -aq -f "name=^${CONTAINER}\$" | grep -q .; then
        if [ "$(docker inspect -f '{{index .Config.Labels "io.hydranos.enrolled"}}' "$CONTAINER" 2>/dev/null)" = "true" ]; then
            docker rm -f "$CONTAINER" >/dev/null
        else
            echo "a container named $CONTAINER already exists and was not made by this script;" >&2
            echo "pass --container <another-name>, or remove it yourself" >&2
            exit 1
        fi
    fi
    # --stop-timeout: docker's default is 10 s, and the shutdown flush is
    # allowed HYDRANOS_STOP_TIMEOUT (120 s) after up to 5 s of departures.
    docker run -d --name "$CONTAINER" --label io.hydranos.enrolled=true --restart unless-stopped --network host \
        --stop-timeout 150 \
        -v "$CFG_DIR:/configs" -v "$DATA_DIR:/data" \
        "$IMAGE" --config /configs/default.toml >/dev/null
    LOGS_HINT="docker logs $CONTAINER"
    ;;

system)
    echo "==> docker not found, installing the static binary"
    # Assigned first: a failed download then stops the script under set -e,
    # where inside an argument its exit status would be lost.
    new_bin=$(fetch_binary)
    place_binary "$new_bin"
    if command -v systemctl >/dev/null 2>&1; then
        cat > /etc/systemd/system/hydranos.service <<UNIT
[Unit]
Description=Hydranos torrent daemon
After=network-online.target
Wants=network-online.target

[Service]
ExecStart=$BIN --config $CONFIG
Restart=always
RestartSec=5
# Above HYDRANOS_STOP_TIMEOUT (120 s) plus 5 s of departures; systemd's
# default of 90 s killed the resume flush midway.
TimeoutStopSec=150
LimitNOFILE=1000000

[Install]
WantedBy=multi-user.target
UNIT
        systemctl daemon-reload
        # enable, then restart: `enable --now` leaves an already running
        # daemon alone, so a re-run kept serving the binary it just replaced.
        systemctl enable hydranos >/dev/null 2>&1 || echo "warning: could not enable hydranos at boot" >&2
        systemctl restart hydranos
        LOGS_HINT="journalctl -u hydranos"
    else
        # No init system to hand it to. Saying so beats leaving a binary on disk
        # that the operator believes is running.
        echo "no systemd here: start it yourself with" >&2
        echo "  $BIN --config $CONFIG" >&2
        exit 1
    fi
    ;;

user)
    echo "==> installing for $(id -un) alone, in its home directory"
    new_bin=$(fetch_binary)
    UNIT_DIR="${XDG_CONFIG_HOME:-$HOME/.config}/systemd/user"
    # `show-environment` answers only when this login has a user manager to
    # talk to. An ssh session without pam_systemd, or `su` to the account, has
    # none even on a systemd host, and every `systemctl --user` then fails.
    if command -v systemctl >/dev/null 2>&1 && systemctl --user show-environment >/dev/null 2>&1; then
        place_binary "$new_bin"
        mkdir -p "$UNIT_DIR"
        # ⚠ No LimitNOFILE: a user manager cannot raise the hard limit, and a
        # value above it fails the unit with 205/LIMITS before the daemon even
        # runs. The shell lifts the soft limit to the hard one instead, which
        # needs no privilege. `$$` is systemd's escape for a literal `$`.
        cat > "$UNIT_DIR/hydranos.service" <<UNIT
[Unit]
Description=Hydranos torrent daemon
After=network-online.target

[Service]
ExecStart=/bin/sh -c 'ulimit -n "\$\$(ulimit -Hn)" 2>/dev/null; exec "$BIN" --config "$CONFIG"'
Restart=always
RestartSec=5
# Above HYDRANOS_STOP_TIMEOUT (120 s) plus 5 s of departures.
TimeoutStopSec=150

[Install]
WantedBy=default.target
UNIT
        systemctl --user daemon-reload
        # enable, then restart, for the same reason as the system unit.
        systemctl --user enable hydranos >/dev/null 2>&1 || echo "warning: could not enable hydranos at login" >&2
        systemctl --user restart hydranos
        LOGS_HINT="journalctl --user -u hydranos"
        # Without lingering the user manager, and the node with it, stops at
        # the last logout and does not come back at boot. Enabling it for
        # oneself is allowed on some hosts and refused on others.
        me=$(id -un)
        if [ "$(loginctl show-user "$me" -p Linger --value 2>/dev/null)" != "yes" ]; then
            loginctl --no-ask-password enable-linger "$me" >/dev/null 2>&1 || true
            if [ "$(loginctl show-user "$me" -p Linger --value 2>/dev/null)" != "yes" ]; then
                echo "" >&2
                echo "!! lingering is off for $me: the node stops when you log out and stays down after a reboot." >&2
                echo "!! ask your host to run:  loginctl enable-linger $me" >&2
                echo "" >&2
            fi
        fi
    else
        # No user manager. Start it by hand so the enrolment below still has a
        # node to register, and say plainly that nothing will bring it back.
        PIDFILE="$DATA_DIR/hydranos.pid"
        # /proc names the binary by its resolved path: a home behind a symlink
        # would otherwise never match and the old node would keep the port.
        bin_real=$(readlink -f "$BIN" 2>/dev/null || echo "$BIN")
        if [ -f "$PIDFILE" ]; then
            old=$(cat "$PIDFILE")
            # Only a pid that is still this binary: after a reboot the number
            # can belong to anything. "(deleted)" is how /proc names a binary
            # that a previous run has since renamed over.
            case "$(readlink "/proc/$old/exe" 2>/dev/null)" in
                "$bin_real"|"$bin_real (deleted)")
                    echo "==> stopping the running node (pid $old)"
                    kill "$old" 2>/dev/null || true
                    i=0
                    while kill -0 "$old" 2>/dev/null && [ $i -lt 150 ]; do i=$((i + 1)); sleep 1; done
                    if kill -0 "$old" 2>/dev/null; then
                        echo "the running node (pid $old) did not stop; stop it and re-run" >&2
                        exit 1
                    fi
                    ;;
            esac
        fi
        place_binary "$new_bin"
        (
            # shellcheck disable=SC3045 # dash, bash and busybox all have -n/-H; a shell without them just keeps its limit
            ulimit -n "$(ulimit -Hn)" 2>/dev/null || true
            exec nohup "$BIN" --config "$CONFIG" </dev/null >/dev/null 2>>"$DATA_DIR/hydranos.stderr"
        ) &
        echo $! > "$PIDFILE"
        LOGS_HINT="$CFG_DIR/hydranos.log and $DATA_DIR/hydranos.stderr"
        echo "" >&2
        echo "!! no systemd user session here: the node is running now, but nothing restarts it." >&2
        echo "!! after a reboot or a crash, start it again with:" >&2
        # shellcheck disable=SC2016 # the $(...) is for the operator's shell, not this one
        printf '!!   (ulimit -n "$(ulimit -Hn)"; nohup %s --config %s </dev/null >/dev/null 2>&1 &)\n' "$BIN" "$CONFIG" >&2
        echo "!! or ask your host to enable a user session for you, then re-run this script:" >&2
        echo "!!   loginctl enable-linger $(id -un)" >&2
        echo "" >&2
    fi
    ;;
esac

echo "==> waiting for the node to answer"
i=0
while [ $i -lt 60 ]; do
    if curl -fsS -m 2 "http://127.0.0.1:$PORT/health" >/dev/null 2>&1; then break; fi
    i=$((i + 1)); sleep 1
done
[ $i -lt 60 ] || { echo "the node did not come up; look at: $LOGS_HINT" >&2; exit 1; }

echo "==> registering with $REGISTER_TO"
body=$(printf '{"token":"%s","name":"%s","url":"http://%s:%s","api_key":"%s"}' \
        "$TOKEN" "$NAME" "$SELF_IP" "$PORT" "$API_KEY")
if curl -fsS -m 15 -H 'Content-Type: application/json' -d "$body" \
     "$REGISTER_TO/api/nodes/register"; then
    echo ""
    echo "==> done. The node is in the fleet."
else
    echo "" >&2
    echo "the node is installed and running, but registering failed." >&2
    echo "add it by hand: url http://$SELF_IP:$PORT  key $API_KEY" >&2
    exit 1
fi
