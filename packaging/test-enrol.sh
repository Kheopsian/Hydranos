#!/bin/sh
# Runs the node enrolment script (install.sh at the root) in its "system" mode,
# as root in a throwaway Debian container, against a stand-in controller and
# release mirror on loopback. Checks that an archive whose SHA-256 does not
# match the published one is never installed, and that the config holding the
# node's API key is readable by its owner alone.
#
#   sh packaging/test-enrol.sh           (needs docker and network for apt)
set -eu

if [ "${1:-}" != "--inside" ]; then
    root=$(cd "$(dirname "$0")/.." && pwd)
    exec docker run --rm -v "$root/install.sh":/src/install.sh:ro -v "$root/packaging/test-enrol.sh":/src/test-enrol.sh:ro \
        debian:bookworm-slim sh /src/test-enrol.sh --inside
fi

apt-get update -qq >/dev/null && apt-get install -y -qq curl iproute2 python3 >/dev/null 2>&1

fail=0
check() { # check <what> <expected> <actual>
    if [ "$2" = "$3" ]; then echo "ok   $1 = $3"; else echo "FAIL $1: expected $2, got $3"; fail=1; fi
}

printf '#!/bin/sh\necho "systemctl $*" >> /tmp/systemctl.log\n' > /usr/local/bin/systemctl
chmod 755 /usr/local/bin/systemctl
mkdir -p /etc/systemd/system

case "$(uname -m)" in aarch64|arm64) ARCH=arm64 ;; *) ARCH=amd64 ;; esac
VER=v9.9.9
srv=/tmp/srv
mkdir -p "$srv/dl/$VER" /tmp/good /tmp/bad
printf '#!/bin/sh\necho hydranos 9.9.9 good\n' > /tmp/good/hydranos
printf '#!/bin/sh\necho hydranos 9.9.9 altered\n' > /tmp/bad/hydranos
chmod 755 /tmp/good/hydranos /tmp/bad/hydranos
tar -czf /tmp/good.tar.gz -C /tmp/good hydranos
tar -czf /tmp/bad.tar.gz -C /tmp/bad hydranos
name="hydranos-$VER-linux-$ARCH.tar.gz"
(cd /tmp && sha256sum good.tar.gz | sed "s/good.tar.gz/$name/") > "$srv/dl/$VER/$name.sha256"

# The controller and the mirror in one: /health, the downloads, and the
# registration the script ends with. The node's own /health is the same port.
cat > /tmp/srv.py <<'PY'
import http.server, os, sys
srv = sys.argv[1]
class H(http.server.BaseHTTPRequestHandler):
    def log_message(self, *a): pass
    def reply(self, code, body, ctype="application/json"):
        self.send_response(code); self.send_header("Content-Type", ctype)
        self.send_header("Content-Length", str(len(body))); self.end_headers(); self.wfile.write(body)
    def do_GET(self):
        if self.path == "/health":
            return self.reply(200, b'{"status": "ok", "version": "9.9.9"}')
        p = os.path.join(srv, self.path.lstrip("/"))
        if os.path.isfile(p):
            return self.reply(200, open(p, "rb").read(), "application/octet-stream")
        self.reply(404, b"{}")
    def do_POST(self):
        self.rfile.read(int(self.headers.get("Content-Length", 0)))
        self.reply(200, b'{"ok": true}')
http.server.ThreadingHTTPServer(("127.0.0.1", 18199), H).serve_forever()
PY
python3 /tmp/srv.py "$srv" &
i=0; until curl -fs -m 1 http://127.0.0.1:18199/health >/dev/null 2>&1 || [ $i -ge 50 ]; do i=$((i + 1)); sleep 0.2; done

enrol() {
    HYDRANOS_RELEASE_BASE=http://127.0.0.1:18199/dl sh /src/install.sh \
        --register-to http://127.0.0.1:18199 --token t0k3n --port 18199 --dir /opt/hydranos >/tmp/enrol.log 2>&1
}

echo "== an archive that does not match its published checksum"
cp /tmp/bad.tar.gz "$srv/dl/$VER/$name"
if enrol; then status=installed; else status=refused; fi
check "altered archive" refused "$status"
check "binary left in place" absent "$( [ -e /usr/local/bin/hydranos ] && echo present || echo absent)"
check "mode of the config" 600 "$(stat -c %a /opt/hydranos/default.toml)"

echo "== the archive the checksum describes"
cp /tmp/good.tar.gz "$srv/dl/$VER/$name"
if enrol; then status=installed; else status=refused; cat /tmp/enrol.log; fi
check "matching archive" installed "$status"
check "installed binary" "hydranos 9.9.9 good" "$(/usr/local/bin/hydranos 2>/dev/null || true)"
check "mode of the config" 600 "$(stat -c %a /opt/hydranos/default.toml)"

exit $fail
