#!/bin/sh
# Hydra node enrolment.
#
# Run on the machine that is to become a node. It installs Hydra, gives it a
# freshly generated API key, starts it, and then REGISTERS ITSELF with the
# Hydra that handed out the token.
#
# The direction is the point. The controlling Hydra never opens a session here
# and never holds a credential for this machine, so compromising its API cannot
# become code execution on the fleet. The only authority that crosses the wire
# is a token that is single use and expires in thirty minutes.
#
#   curl -fsSL http://<hydra>/install.sh | sh -s -- --register-to http://<hydra> --token <token>
set -eu

REGISTER_TO=""
TOKEN=""
NAME="$(hostname -s 2>/dev/null || hostname)"
PORT="8199"
DIR="/opt/hydra"
IMAGE="ghcr.io/kheopsian/hydra:latest"

while [ $# -gt 0 ]; do
    case "$1" in
        --register-to) REGISTER_TO="$2"; shift 2 ;;
        --token)       TOKEN="$2";       shift 2 ;;
        --name)        NAME="$2";        shift 2 ;;
        --port)        PORT="$2";        shift 2 ;;
        --dir)         DIR="$2";         shift 2 ;;
        *) echo "unknown option: $1" >&2; exit 2 ;;
    esac
done

[ -n "$REGISTER_TO" ] || { echo "--register-to is required" >&2; exit 2; }
[ -n "$TOKEN" ]       || { echo "--token is required" >&2; exit 2; }

# The address the CONTROLLER will use to reach this node. Taken from the route
# to the controller itself rather than from `hostname -I`: a machine with a
# tunnel and a LAN link has several addresses, and only one of them is the one
# the controller can come back on.
controller_host=$(echo "$REGISTER_TO" | sed -e 's|^[a-z]*://||' -e 's|[:/].*$||')
SELF_IP=$(ip route get "$(getent hosts "$controller_host" | awk '{print $1; exit}' || echo "$controller_host")" 2>/dev/null \
          | awk '{for (i=1;i<=NF;i++) if ($i=="src") {print $(i+1); exit}}')
[ -n "${SELF_IP:-}" ] || { echo "cannot work out which address to publish; pass --name and edit the node afterwards" >&2; exit 1; }

API_KEY=$(head -c 24 /dev/urandom | od -An -tx1 | tr -d ' \n')

echo "==> node   : $NAME"
echo "==> address: $SELF_IP:$PORT"
echo "==> config : $DIR"

mkdir -p "$DIR/data"
if [ ! -f "$DIR/default.toml" ]; then
    cat > "$DIR/default.toml" <<TOML
[daemon]
api_host = "0.0.0.0"
api_port = $PORT
api_key = "$API_KEY"
data_dir = "/configs"

[race]
listen_port = 16371
enable_ipv6 = true

[hoard]
listen_port = 16372
enable_ipv6 = true
TOML
else
    # An existing install keeps its key: re-running enrolment must not lock the
    # operator out of a node they already had.
    API_KEY=$(grep -m1 '^api_key' "$DIR/default.toml" | cut -d'"' -f2)
    echo "==> keeping the existing config and key"
fi

if command -v docker >/dev/null 2>&1; then
    echo "==> installing with docker"
    docker rm -f hydra >/dev/null 2>&1 || true
    docker run -d --name hydra --restart unless-stopped --network host \
        -v "$DIR:/configs" -v "$DIR/data:/data" \
        "$IMAGE" --config /configs/default.toml >/dev/null
else
    echo "==> docker not found, installing the static binary"
    case "$(uname -m)" in
        x86_64|amd64)  ARCH="x86_64-unknown-linux-musl" ;;
        aarch64|arm64) ARCH="aarch64-unknown-linux-musl" ;;
        *) echo "unsupported architecture: $(uname -m)" >&2; exit 1 ;;
    esac
    # The release tarballs are built against musl, so the binary carries its own
    # libc: nothing to install and nothing to match against the host distro.
    URL="https://github.com/Kheopsian/Hydra/releases/latest/download/hydra-$ARCH.tar.gz"
    echo "==> fetching $URL"
    tmp=$(mktemp -d)
    if ! curl -fsSL "$URL" -o "$tmp/hydra.tar.gz"; then
        echo "could not download the release tarball" >&2
        rm -rf "$tmp"; exit 1
    fi
    tar -xzf "$tmp/hydra.tar.gz" -C "$tmp"
    bin=$(find "$tmp" -maxdepth 2 -type f -name hydra | head -1)
    [ -n "$bin" ] || { echo "no hydra binary inside the tarball" >&2; rm -rf "$tmp"; exit 1; }
    install -m 0755 "$bin" /usr/local/bin/hydra
    rm -rf "$tmp"

    if command -v systemctl >/dev/null 2>&1; then
        cat > /etc/systemd/system/hydra.service <<UNIT
[Unit]
Description=Hydra torrent daemon
After=network-online.target
Wants=network-online.target

[Service]
ExecStart=/usr/local/bin/hydra --config $DIR/default.toml
Environment=HYDRA_CONFIG_DIR=$DIR
Restart=always
RestartSec=5
LimitNOFILE=1000000

[Install]
WantedBy=multi-user.target
UNIT
        systemctl daemon-reload
        systemctl enable --now hydra >/dev/null 2>&1 || systemctl restart hydra
    else
        # No init system to hand it to. Saying so beats leaving a binary on disk
        # that the operator believes is running.
        echo "no systemd here: start it yourself with" >&2
        echo "  HYDRA_CONFIG_DIR=$DIR /usr/local/bin/hydra --config $DIR/default.toml" >&2
        exit 1
    fi
fi

echo "==> waiting for the node to answer"
i=0
while [ $i -lt 60 ]; do
    if curl -fsS -m 2 "http://127.0.0.1:$PORT/health" >/dev/null 2>&1; then break; fi
    i=$((i + 1)); sleep 1
done
[ $i -lt 60 ] || { echo "the node did not come up; look at: docker logs hydra" >&2; exit 1; }

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
