#!/bin/sh
# Runs packaging/install.sh as root in a throwaway Debian container and checks
# what it leaves behind: the binaries belong to root, so the service account
# cannot swap the program an administrator later runs with sudo, and the
# config, which holds the API key, is readable by its owner alone.
#
#   sh packaging/test-install.sh           (needs docker)
set -eu

if [ "${1:-}" != "--inside" ]; then
    here=$(cd "$(dirname "$0")" && pwd)
    exec docker run --rm -v "$here":/pkg:ro debian:bookworm-slim sh /pkg/test-install.sh --inside
fi

fail=0
check() { # check <what> <expected> <actual>
    if [ "$2" = "$3" ]; then echo "ok   $1 = $3"; else echo "FAIL $1: expected $2, got $3"; fail=1; fi
}

# systemd is not running in a container: record the calls instead.
printf '#!/bin/sh\necho "systemctl $*" >> /tmp/systemctl.log\n' > /usr/local/bin/systemctl
chmod 755 /usr/local/bin/systemctl
mkdir -p /etc/systemd/system

stage=/tmp/stage
mkdir -p "$stage"
cp /pkg/install.sh /pkg/hydranos.service "$stage"/
printf '#!/bin/sh\necho hydranos 0.0.0\n' > "$stage/hydranos"
printf '#!/bin/sh\n' > "$stage/hydranos-update"
chmod 755 "$stage/hydranos" "$stage/hydranos-update"
printf 'data_dir = "./data"\napi_key = ""\n' > "$stage/default.toml.example"

echo "== fresh install"
(cd "$stage" && sh ./install.sh >/dev/null)
check "owner of /opt/hydranos" root "$(stat -c %U /opt/hydranos)"
check "owner of /opt/hydranos/hydranos" root "$(stat -c %U /opt/hydranos/hydranos)"
check "owner of /opt/hydranos/hydranos-update" root "$(stat -c %U /opt/hydranos/hydranos-update)"
check "mode of /etc/hydranos/default.toml" 600 "$(stat -c %a /etc/hydranos/default.toml)"
check "owner of /etc/hydranos/default.toml" hydranos "$(stat -c %U /etc/hydranos/default.toml)"
check "owner of /var/lib/hydranos" hydranos "$(stat -c %U /var/lib/hydranos)"

echo "== re-run over an install made by an older installer"
chown -R hydranos:hydranos /opt/hydranos
chmod 644 /etc/hydranos/default.toml
echo 'api_key = "kept"' >> /etc/hydranos/default.toml
(cd "$stage" && sh ./install.sh >/dev/null)
check "owner of /opt/hydranos" root "$(stat -c %U /opt/hydranos)"
check "owner of /opt/hydranos/hydranos" root "$(stat -c %U /opt/hydranos/hydranos)"
check "mode of /etc/hydranos/default.toml" 600 "$(stat -c %a /etc/hydranos/default.toml)"
check "config kept" 1 "$(grep -c '^api_key = "kept"' /etc/hydranos/default.toml)"

exit $fail
