# What Hydra 4 still has to do

The 3.x front is not only an HTTP server. It runs 74 long-lived goroutines, and
a route-by-route port cannot see them: the differential bench compares
responses, so a subsystem that produces no response is invisible to it. That is
how this port reported "169/169 routes, 100%" while the announcer -- one of the
two things the Go actually does -- had not been written.

This file is the denominator that count was missing. It is organised by what
happens if the behaviour is absent, because that is what decides the order.

## Ported and verified

- The whole HTTP surface: 169 routes, compared byte-for-byte against 3.180.0,
  plus 115 write mutations replayed against both stores.
- The engines themselves, in-process, on the network (`session::start`).
- The race lifecycle recorder (`raceevents.rs`), the bench database, the store
  and its rescue path.

## Gone by construction -- do not port

These exist to maintain the Go's mirror of engine state. Hydra 4 reads the
engine's own map, so there is nothing to refresh.

| Go | Why it disappears |
|---|---|
| `hoard.go statsRefreshLoop` | refreshes a copy that no longer exists |
| `hoard.go torrentListRefreshLoop` | same |
| `race.go statsRefreshLoop` | same |
| `api/snapshot_pusher.go` | pushed the copy to clients; handlers read the engine |

## Ported since this file was written

| Subsystem | Verified by |
|---|---|
| The announcer, whole | a staging instance announced to a real tracker and read its answer |
| verify throttle, download slots | unit tests; the throttle stopped itself in staging |
| the eight health invariants | unit tests |
| the race drain | unit tests; two gates, off by default |
| move jobs | unit tests, including the hardlink refusal |
| VPN speedtest | unit tests |
| qBittorrent import | unit tests; wired to its route |
| NAT-PMP port forwarding | unit tests |
| WireGuard config parsing and redaction | unit tests |

## Still missing

- Transmission import (the qBittorrent one is done; this is the same shape
  against a different API).
- The engine watchdog and `extrasmgr`'s reconcile loop.
- Production: the release binary exists and runs in staging. It has not
  replaced the Go in production.

## Was missing, in order of what breaks

### 1. The announcer -- nothing works without it

~3 000 lines, `internal/engine/tracker_*.go` + `hoard_announce*.go`. The Rust
engine has never announced: `tracker::start_announce_loop` there is the dial
queue consumer, and the engine source says so -- "announces themselves belong
to the Go control plane".

Without it Hydra 4 seeds, listens and connects, and every tracker forgets it.

- per-torrent announce loops (race) and a scheduler with a worker pool (hoard)
- passkey overrides per tracker host
- client spoofing: peer_id prefix and user agent per tracker
- announce IP modes, secondary stats modes
- the tracker registry, the breaker, UDP trackers
- family selection (v4/v6) and per-binding proxy

### 2. Behaviour an operator would notice missing

| Go | What it does |
|---|---|
| `hoard.go downloadSlotManager` | caps concurrent downloads |
| `hoard.go verifyThrottle` | paces rechecks |
| `hoard.go staggerStart` | spreads startup over time |
| `race.go trackerWatchdog` | notices a tracker that stopped answering |
| `race.go updatePeerIntel` | peer intelligence |
| `internal/drain` | race drain |
| `internal/jobs` | move-data workers (the routes are ported, the workers are not) |
| `health` scanner | the 5-minute anomaly scan |
| `portfwd` | gluetun port forwarding |
| `wgtun` / `wireguard.go follow` | WireGuard tunnels |
| `extrasmgr reconcileLoop` | engine extras |
| `watchdog.go` | engine liveness |

### 3. Started per request, not loops

Import (qbit, transmission), magnet resolve, reachability probes, VPN
speedtest. These are `go func` on the request path; in Rust they are
`tokio::spawn` from the handler. Cheap, but they are behaviour, not routes.

## How this gets verified

Not by the response oracles -- they are blind to all of it. An announcer is
verified by what leaves the machine: a staging instance announcing to a real
tracker, and the tracker's own view of it.
