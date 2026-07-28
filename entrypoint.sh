#!/bin/sh
# Raise file descriptor limit for Rain (1 socket per torrent).
# Requires --privileged or CAP_SYS_RESOURCE.
ulimit -n 200000 2>/dev/null || true
exec hydra "$@"
