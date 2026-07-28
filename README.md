# Hydra

A self-hosted BitTorrent daemon built for **scale and seeding**: a Go control
plane driving a purpose-built Rust engine ("Typhon"). Hydra holds **100k+
torrents** in a single instance, exposes a live web UI, a native REST API, and a
qBittorrent-compatible shim so your existing `*arr` / autobrr / cross-seed setup
just works.

> Status: Hydra is being opened up from a private homelab project. It is used in
> production but some rough edges remain — issues and PRs welcome.

---

## Why Hydra?

- **Two engine roles.** A **race** session (aggressive, low-latency — for hot
  downloads) and a **hoard** session (upload-optimized — for long-term seeding),
  each tuned independently. You can also run any number of extra engines.
- **Scales.** Tens of thousands of torrents per instance, with a push-based
  (SSE) UI that streams the torrent list instead of shipping giant REST blobs.
- **Flexible networking.** Direct, SOCKS5 egress, a lightweight PROXY-v2 relay
  (change your seedbox's public IP without a VPN tunnel), or gluetun with
  **hot listen-port rebind** (no restart when your VPN's forwarded port rotates).
- **Distributed.** Run engines across several machines and aggregate them behind
  one front; route new torrents per-category with placement/strategy and a
  save-path per agent.
- **Drop-in.** A qBittorrent v2 API shim means autobrr, Sonarr/Radarr,
  cross-seed, etc. talk to Hydra unchanged.

---

## Architecture

```
                ┌──────────────────────── hydra (Go) ────────────────────────┐
  browser ──────┤  Web UI (SSE)   REST /api/*   qBit shim /api/v2/*           │
  *arr/autobrr ─┤                                                             │
                │  api.Server ── category routing ── tracker announce (Go)    │
                └───────────────┬──────────────────────┬────────────────────┘
                                │ unix socket JSON-RPC  │ (or gRPC to remote agents)
                        ┌───────┴───────┐       ┌───────┴───────┐
                        │ Typhon engine │  ...  │ Typhon engine │   (Rust, one per role)
                        │  race         │       │  hoard        │
                        └───────────────┘       └───────────────┘
```

- **Go control plane** (`cmd/hydra`, `internal/api`, `internal/engine`) owns the
  UI, the API, category/routing logic, and tracker announces.
- **Typhon** (`typhon-engine/`, Rust) is the BitTorrent engine: peer wire
  protocol, piece picking, disk I/O, choking. One process per engine, driven
  over a Unix-socket JSON-RPC.
- **Agents.** Any engine can be exposed over gRPC (TLS + token) and driven by a
  remote front, so a single UI can aggregate engines spread across hosts.

See [`docs/`](docs/) for the API reference (`API.md`) and the agent/front
architecture notes.

---

## Quick start (Docker)

```bash
git clone https://github.com/Kheopsian/hydra
cd hydra
docker build -t hydra:latest .

mkdir -p ./data/configs ./data/downloads
cp configs/default.toml ./data/configs/default.toml   # edit to taste

# Bootstrap a login password (writes a bcrypt hash you paste into [auth]):
docker run --rm --entrypoint hydra hydra:latest hash-password 'your-password'

docker run -d --name hydra \
  -p 8199:8199 \                # web UI + API
  -p 16171:16171 -p 16172:16172 \   # BT listen ports (race, hoard)
  -v $PWD/data/configs:/configs \
  -v $PWD/data/downloads:/data \
  hydra:latest
```

Open `http://localhost:8199`, log in with `admin` / your password. The `api_key`
is auto-generated on first start (see `[daemon] api_key` in the config) and is
what the REST API and the qBit shim expect as `X-Api-Key`.

Forward the BT listen ports on your router for inbound peers (or use one of the
networking modes below).

---

## Configuration

Everything lives in a single `default.toml` (template: [`configs/default.toml`](configs/default.toml)).
Most settings are also editable from the **Config** tab in the UI: scalar
settings apply live, engine-level settings write the file and offer an
"Apply & restart" button.

Key sections:

| Section          | What it does                                                      |
|------------------|------------------------------------------------------------------|
| `[daemon]`       | API host/port, `data_dir`, auto-generated `api_key`.             |
| `[race]`         | The race engine (aggressive) — ports, connection caps, choking.  |
| `[hoard]`        | The hoard engine (seeding) — ports, upload slots, queue limits.  |
| `[auth]`         | `username` + bcrypt `password_hash` (login → returns api_key).   |
| `[race_drain]`   | Auto-purge of the race scratch disk by watermark.                |
| `[vpn_speedtest]`| Periodic egress capacity check against a public iperf3 server.    |

### Networking modes

Each engine can reach the swarm in one of several ways — pick per your setup:

- **Direct** (default): set `listen_port`, forward it on your router. Nothing
  else needed.
- **SOCKS5 egress**: set `socks5_outbound_host`/`_port` so outbound peer/tracker
  traffic exits through a proxy.
- **PROXY-v2 relay**: put the companion **[hydra-relay](https://github.com/Kheopsian/hydra-relay)** on a
  cheap VPS — inbound peers hit the VPS (haproxy + PROXY protocol v2) and land on
  Hydra with their *real* IP preserved, while `socks5_outbound_*` sends egress
  out the same VPS. This changes your seedbox's public IP with no L3/VPN tunnel
  (12 bytes of PROXY-v2 header, no MTU/kernel module cost). Set
  `listen_port_proxy_v2`, `listen_addr_proxy_v2`, and `proxy_v2_trusted_sources`.
- **gluetun / dynamic port**: run an engine in a VPN container whose forwarded
  port rotates. The listener rebinds **without a restart** (torrents and live
  peers stay up) — two ways to push the new port, no extra component needed:
  - CLI (any node, incl. `--agent-only` with no HTTP API):
    `hydra set-listen-port <engine-socket> <port>`
  - HTTP (monolith / front): `POST /api/{race,hoard}/listen-port {"port":N}`.

  Wire it to gluetun's own port-forwarding hook, the same way people do it for
  qBittorrent — gluetun runs it on every (re)negotiation:

  ```yaml
  # docker-compose, gluetun service. {{PORTS}} is substituted by gluetun.
  environment:
    - VPN_PORT_FORWARDING=on
    - VPN_PORT_FORWARDING_UP_COMMAND=/bin/sh -c 'hydra set-listen-port /configs/hoard.sock {{PORTS}}'
  ```

  ⚠️ **One forwarded port serves one engine.** Two engines are two processes and
  cannot share an inbound TCP port. If your VPN only forwards a single port, do
  **not** try to point it at both — see the deployment patterns below.

Advanced networking keys (`listen_port_proxy_v2`, `socks5_outbound_*`, …) are
opt-in and off by default; the template ships the simple direct setup.

---

## Distributed / multi-engine

Hydra runs in three topologies:

- **Monolith** — one process, race + hoard local (the default above).
- **Agent** (`--agent-only`) — a headless node hosting one or more engines,
  exposed over gRPC for a remote front to drive.
- **Front-only** (`--front-only`) — a UI/controller node with no local engine
  that aggregates remote agents.

A monolith can also register remote agents and shard by simply *adding* more
hoard engines (via the UI's *Agents → Local engines*, or `[[engine]]` blocks in
the config). Categories carry a **placement** (which engines host new torrents),
a **strategy** (e.g. fan-out to all, least-torrents), and a **save-path per
engine**.

### Choosing a deployment

- **You control real inbound ports** (home router with port-forwarding, or a
  VPS): run the **monolith**. race and hoard each bind and forward their own
  port — simplest setup, one container.
- **You're behind a VPN that forwards a single port** (gluetun + Proton/PIA/…):
  don't cram both engines behind one tunnel. Run a **front-only** controller
  plus **one `--agent-only` per engine, each in its own gluetun** so each engine
  gets its own forwarded port. Every agent's gluetun `UP_COMMAND` pushes its
  rotated port to its local engine (`hydra set-listen-port …`), and the
  front-only node aggregates and drives them all from one UI. This is the
  recommended way to seed over a single-port VPN without losing inbound on
  either engine.

---

## Building from source

Requires Rust (1.x, 2021+) and Go 1.25.

```bash
# Rust engine
cd typhon-engine && cargo build --release   # -> target/release/typhon-engine

# Go daemon
go build -o hydra ./cmd/hydra
```

The provided multi-stage `Dockerfile` builds both and assembles a slim runtime
image; it's the recommended path.

---

## API

- **Native REST** under `/api/*`, authenticated with `X-Api-Key`. Full reference:
  [`docs/API.md`](docs/API.md).
- **qBittorrent shim** under `/api/v2/*` for compatibility with autobrr, the
  `*arr` suite, cross-seed, etc. Use `skip_checking=true` to seed data already on
  disk without re-hashing.

---

## License

Hydra is licensed under the **GNU Affero General Public License v3.0** (AGPL-3.0)
— see [`LICENSE`](LICENSE). In short: you're free to run, study, modify, and
share it, including self-hosting a modified version — but if you offer a modified
Hydra to others over a network, you must make your source available under the
same license.
