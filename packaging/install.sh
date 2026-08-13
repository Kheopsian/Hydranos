#!/bin/sh
# Hydra bare-metal installer: binaries in /opt/hydra, config in /etc/hydra,
# data in /var/lib/hydra, runs as a dedicated "hydra" user under systemd.
# Re-runnable: upgrades binaries in place without touching config or data.
set -e

PREFIX=/opt/hydra
CFG=/etc/hydra
DATA=/var/lib/hydra

if [ "$(id -u)" -ne 0 ]; then
    echo "install.sh must run as root (try: sudo ./install.sh)" >&2
    exit 1
fi

# Dedicated system user (no login shell, home = data dir).
if ! id hydra >/dev/null 2>&1; then
    useradd --system --home-dir "$DATA" --shell /usr/sbin/nologin hydra
fi

install -d -o hydra -g hydra "$PREFIX" "$CFG" "$DATA"
install -m 0755 hydra hydra-engine "$PREFIX"/

# Seed config only on first install; never clobber an existing one.
if [ ! -f "$CFG/default.toml" ]; then
    install -m 0644 default.toml.example "$CFG/default.toml"
    sed -i "s#^data_dir = .*#data_dir = \"$DATA\"#" "$CFG/default.toml"
    chown hydra:hydra "$CFG/default.toml"
    echo "hydra: seeded $CFG/default.toml (data_dir=$DATA)"
fi

install -m 0644 hydra.service /etc/systemd/system/hydra.service
systemctl daemon-reload
systemctl enable --now hydra

echo
echo "Hydra installed and started. UI: http://<this-host>:8199"
echo
echo "Create the admin account by opening the UI. For safety that first-run"
echo "screen only answers callers on the same machine or a private network, so"
echo "an instance exposed to the internet cannot be claimed by a stranger."
echo "On a remote host (a seedbox), set the password here instead:"
echo "  $PREFIX/hydra reset-password '<newpassword>' $CFG/default.toml"
echo "  systemctl restart hydra"
echo
echo "Manage: systemctl {status,restart,stop} hydra   |   logs: journalctl -u hydra -f"
echo "Upgrade: unpack a newer tarball and re-run ./install.sh"
