#!/bin/sh
# Hydranos bare-metal installer: binaries in /opt/hydranos, config in /etc/hydranos,
# data in /var/lib/hydranos, runs as a dedicated "hydranos" user under systemd.
# Re-runnable: upgrades binaries in place without touching config or data.
set -e

PREFIX=/opt/hydranos
CFG=/etc/hydranos
DATA=/var/lib/hydranos

if [ "$(id -u)" -ne 0 ]; then
    echo "install.sh must run as root (try: sudo ./install.sh)" >&2
    exit 1
fi

# Dedicated system user (no login shell, home = data dir).
if ! id hydranos >/dev/null 2>&1; then
    useradd --system --home-dir "$DATA" --shell /usr/sbin/nologin hydranos
fi

install -d -o hydranos -g hydranos "$PREFIX" "$CFG" "$DATA"
# hydranos-update ships in the same tarball and replaces both on upgrade; 4.3
# installed hydranos-engine (a 3.x leftover nothing starts) and left the
# updater behind.
install -m 0755 hydranos hydranos-update "$PREFIX"/

# Seed config only on first install; never clobber an existing one.
if [ ! -f "$CFG/default.toml" ]; then
    install -m 0644 default.toml.example "$CFG/default.toml"
    sed -i "s#^data_dir = .*#data_dir = \"$DATA\"#" "$CFG/default.toml"
    chown hydranos:hydranos "$CFG/default.toml"
    echo "hydranos: seeded $CFG/default.toml (data_dir=$DATA)"
fi

# The unit is installed once. A re-run used to overwrite it, losing any edit;
# changes belong in a drop-in (systemctl edit hydranos), and a newer shipped
# unit is left beside it for comparison.
UNIT=/etc/systemd/system/hydranos.service
if [ ! -f "$UNIT" ]; then
    install -m 0644 hydranos.service "$UNIT"
elif ! cmp -s hydranos.service "$UNIT"; then
    install -m 0644 hydranos.service "$UNIT.new"
    echo "hydranos: kept your $UNIT; the shipped one is at $UNIT.new"
fi
systemctl daemon-reload
systemctl enable --now hydranos

echo
echo "Hydranos installed and started. UI: http://<this-host>:8199"
echo
echo "Create the admin account by opening the UI. For safety that first-run"
echo "screen only answers callers on the same machine or a private network, so"
echo "an instance exposed to the internet cannot be claimed by a stranger."
echo "On a remote host (a seedbox), set the password here instead:"
echo "  $PREFIX/hydranos reset-password '<newpassword>' $CFG/default.toml"
echo "  systemctl restart hydranos"
echo
echo "Manage: systemctl {status,restart,stop} hydranos   |   logs: journalctl -u hydranos -f"
echo "Upgrade: $PREFIX/hydranos-update, or unpack a newer tarball and re-run ./install.sh"
