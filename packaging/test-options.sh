#!/bin/sh
# The install-time options of install.sh (--harden, --listen) and the
# entrypoint's HYDRANOS_HEAP_PROFILE switch, with their defaults unchanged.
# Runs the real scripts with stand-ins for curl, systemctl and the network
# tools: no docker, no network, nothing outside a temporary directory. The
# "system" unit is only written when run as root inside a container.
#
#   sh packaging/test-options.sh
set -eu

root=$(cd "$(dirname "$0")/.." && pwd)
T=$(mktemp -d)
trap 'rm -rf "$T"' EXIT
export T
fail=0
check() { if [ "$2" = "$3" ]; then echo "ok   $1 = $3"; else echo "FAIL $1: expected $2, got $3"; fail=1; fi; }
has() { # has <what> <file> <fixed text>
    if grep -qF -- "$3" "$2" 2>/dev/null; then echo "ok   $1"; else echo "FAIL $1: '$3' not in $2"; fail=1; fi
}
hasnt() {
    if grep -qF -- "$3" "$2" 2>/dev/null; then echo "FAIL $1: '$3' found in $2"; fail=1; else echo "ok   $1"; fi
}

# --- stand-ins ----------------------------------------------------------
mkdir -p "$T/bin" "$T/pkg"
cat > "$T/bin/curl" <<'STUB'
#!/bin/sh
out=""; url=""
while [ $# -gt 0 ]; do
    case "$1" in
        -o) out=$2; shift 2 ;;
        -m|-H|-d) shift 2 ;;
        -*) shift ;;
        *) url=$1; shift ;;
    esac
done
echo "$url" >> "$T/curl.log"
case "$url" in
    *.sha256) cat "$T/rel.sha" ;;
    *.tar.gz) cp "$T/rel.tar.gz" "$out" ;;
    */health) echo '{"status": "ok", "version": "9.9.9"}' ;;
    *) echo '{}' ;;
esac
STUB
printf '#!/bin/sh\necho "192.168.99.1 controller.example"\n' > "$T/bin/getent"
printf '#!/bin/sh\necho "192.168.99.1 dev eth0 src 192.168.99.5 uid 0"\n' > "$T/bin/ip"
cat > "$T/bin/systemctl" <<'STUB'
#!/bin/sh
echo "systemctl $*" >> "$T/systemctl.log"
exit 0
STUB
printf '#!/bin/sh\necho yes\n' > "$T/bin/loginctl"
chmod 755 "$T"/bin/*

case "$(uname -m)" in aarch64|arm64) ARCH=arm64 ;; *) ARCH=amd64 ;; esac
printf '#!/bin/sh\necho hydranos 9.9.9\n' > "$T/pkg/hydranos"
chmod 755 "$T/pkg/hydranos"
tar -czf "$T/rel.tar.gz" -C "$T/pkg" hydranos
(cd "$T" && sha256sum rel.tar.gz | sed "s/rel.tar.gz/hydranos-v9.9.9-linux-$ARCH.tar.gz/") > "$T/rel.sha"

enrol() { # enrol <home> <args...>: the user install, everything stubbed
    h=$1; shift
    mkdir -p "$h"
    : > "$T/curl.log"
    env HOME="$h" PATH="$T/bin:$PATH" XDG_CONFIG_HOME="" XDG_DATA_HOME="" \
        sh "$root/install.sh" --user --register-to http://controller.example --token t0k3n \
        --port 18199 "$@" > "$T/enrol.log" 2>&1 || { echo "install.sh failed:"; cat "$T/enrol.log"; return 1; }
}

unit_of() { echo "$1/.config/systemd/user/hydranos.service"; }
conf_of() { echo "$1/.config/hydranos/default.toml"; }

echo "== install.sh, no option: the unit and the bind address are what they were"
h=$T/h0
enrol "$h" || fail=1
has "default: binds every interface" "$(conf_of "$h")" 'api_host = "0.0.0.0"'
hasnt "default: unit not sandboxed" "$(unit_of "$h")" "NoNewPrivileges"
has "default: the node is probed on loopback" "$T/curl.log" "http://127.0.0.1:18199/health"

echo "== install.sh --harden --listen <address>"
h=$T/h1
enrol "$h" --harden --listen 192.168.99.5 || fail=1
has "--listen: written as api_host" "$(conf_of "$h")" 'api_host = "192.168.99.5"'
hasnt "--listen: no wildcard left" "$(conf_of "$h")" 'api_host = "0.0.0.0"'
has "--listen: the node is probed where it listens" "$T/curl.log" "http://192.168.99.5:18199/health"
has "--harden: NoNewPrivileges" "$(unit_of "$h")" "NoNewPrivileges=yes"
has "--harden: LockPersonality" "$(unit_of "$h")" "LockPersonality=yes"
has "--harden: the daemon is still started" "$(unit_of "$h")" "ExecStart="
# What would stop a user from keeping their data wherever they like.
hasnt "--harden: no ProtectHome" "$(unit_of "$h")" "ProtectHome"
hasnt "--harden: no ProtectSystem" "$(unit_of "$h")" "ProtectSystem"
hasnt "--harden: no empty capability set" "$(unit_of "$h")" "CapabilityBoundingSet"

if [ "$(id -u)" = 0 ] && [ -e /.dockerenv ] && ! command -v docker >/dev/null 2>&1; then
    echo "== install.sh system mode (root, in a container)"
    unit=/etc/systemd/system/hydranos.service
    mkdir -p /etc/systemd/system
    : > "$T/curl.log"
    env PATH="$T/bin:$PATH" sh "$root/install.sh" --register-to http://controller.example --token t0k3n \
        --port 18199 --dir "$T/sys0" > "$T/enrol.log" 2>&1 || { cat "$T/enrol.log"; fail=1; }
    hasnt "system, default: unit not sandboxed" "$unit" "NoNewPrivileges"
    env PATH="$T/bin:$PATH" sh "$root/install.sh" --register-to http://controller.example --token t0k3n \
        --port 18199 --dir "$T/sys1" --harden > "$T/enrol.log" 2>&1 || { cat "$T/enrol.log"; fail=1; }
    has "system, --harden: NoNewPrivileges" "$unit" "NoNewPrivileges=yes"
    has "system, --harden: PrivateTmp" "$unit" "PrivateTmp=yes"
    hasnt "system, --harden: no ProtectHome" "$unit" "ProtectHome"
    hasnt "system, --harden: no ProtectSystem" "$unit" "ProtectSystem"
    hasnt "system, --harden: no empty capability set" "$unit" "CapabilityBoundingSet"
else
    echo "skip install.sh system mode (needs root in a container)"
fi

echo "== entrypoint.sh, HYDRANOS_HEAP_PROFILE"
mkdir -p "$T/cfg" "$T/ebin"
printf 'data_dir = "/config"\n' > "$T/cfg/default.toml"
# shellcheck disable=SC2016 # expanded by the stand-in when it runs, not here
printf '#!/bin/sh\necho "MALLOC_CONF=${MALLOC_CONF-<unset>}"\n' > "$T/ebin/hydranos"
chmod 755 "$T/ebin/hydranos"
MC="prof:true,prof_active:true,lg_prof_sample:19,prof_prefix:/config/jeprof"
ep() { env -i PATH="$T/ebin:/usr/bin:/bin" HYDRANOS_CONFIG_DIR="$T/cfg" MALLOC_CONF="$MC" "$@" sh "$root/entrypoint.sh" 2>/dev/null | tail -1; }
if [ "$(id -u)" = 0 ]; then
    echo "skip entrypoint.sh (it drops privileges as root; run this test as a normal user)"
else
    check "default: MALLOC_CONF untouched" "MALLOC_CONF=$MC" "$(ep)"
    check "off: heap profiling removed" "MALLOC_CONF=<unset>" "$(ep HYDRANOS_HEAP_PROFILE=off)"
    MC="narenas:4,prof:true,prof_active:true,lg_prof_sample:19,dirty_decay_ms:1000"
    check "off: the other jemalloc settings stay" "MALLOC_CONF=narenas:4,dirty_decay_ms:1000" \
        "$(ep HYDRANOS_HEAP_PROFILE=off)"
fi

exit $fail
