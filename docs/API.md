# REST API

*Applies to Hydranos 4.3.1.*

For scripting Hydranos: every native API route (`/api/*`) in 4.3.1, which work, which are stubs, `curl` examples. The qBittorrent-compatible API (`/api/v2/*`) is documented on [qBittorrent Shim and Automation](https://github.com/Kheopsian/Hydranos/wiki/qBittorrent-Shim-and-Automation) (changes in 4.3.1: *qBittorrent shim* below), and the MCP endpoint on [MCP Server](https://github.com/Kheopsian/Hydranos/wiki/MCP-Server).

## Basics

### Base URL and format

One port, **8199** by default (`[daemon] api_port`), serves the web UI, `/api`, the qBittorrent shim and `/mcp`. Examples use:

```bash
export HY=http://hydranos.example.org:8199
export KEY=YOUR_API_KEY
```

Bodies and answers are JSON unless a row says otherwise. Responses over 32 bytes are gzip-compressed when the client accepts it (`curl --compressed`).

### Authentication

There is one credential, the **API key** (`[daemon] api_key`, 48 hex characters, generated at first start). Where to find it, how to rotate it and how to expose Hydranos safely: [Security and Access](https://github.com/Kheopsian/Hydranos/wiki/Security-and-Access).

| How the key travels | Example | Notes |
|---|---|---|
| `X-Api-Key` header | `-H "X-Api-Key: $KEY"` | The normal way. |
| `apikey` query parameter | `"$HY/api/status?apikey=$KEY"` | For clients that cannot set headers. Lands in proxy logs. |
| `SID` session cookie | from `POST /api/v2/auth/login` | Form `username` + `password` (admin password or API key). Unlocks the **whole** native API. Expires after 3600 s idle; lost on restart. |
| `Authorization: Bearer` | `-H "Authorization: Bearer $KEY"` | Accepted by `/mcp` **only**. |

`POST /api/login` (JSON `{"username","password"}`) does **not** set a cookie: it returns `{"api_key": "..."}`, which the web UI then sends as `X-Api-Key`. An empty `api_key` in the config refuses every caller.

**Refusal shapes** (test the status code, not the text):

| Where | Status | Body |
|---|---|---|
| `/api/*` (almost every route) | `401` | `{"error":"Invalid or missing API key"}` |
| `/api/selection/*` | `401` | `{"error":"unauthorized"}` |
| `/api/v2/*` (qBittorrent shim) | `403` | plain text `Forbidden.` (Sonarr and Radarr expect 403 before login) |
| `/mcp` | `401` | JSON as above + header `WWW-Authenticate: Bearer` |
| `POST /api/nodes/register` | `401` | `{"error":"enrolment token unknown, already used, or expired"}` |
| `POST /api/setup` | `409` once an admin exists; `403` if the caller is not on loopback or a private network | JSON |

`POST /api/setup` judges the caller by the socket peer first: a public peer is refused whatever headers it sends, and a private peer (a reverse proxy) that sends `X-Forwarded-For` must name a private client there too.

**Public routes** (no key): `/health`, `/metrics`, `/`, `/static/*`, `/changelog.md`, `/install.sh`, `GET|POST /api/setup`, `POST /api/login`, `GET /api/startup`, `POST /api/v2/auth/login`, `POST /api/v2/auth/logout`, and `POST /api/nodes/register` (enrolment token instead of the key).

### Errors

Errors are JSON `{"error": "<message>"}`, sometimes with more fields:

- `400`: bad or missing field (strict bodies name the unknown field).
- `404`: `{"error":"torrent not found"}`, or no such engine/node/job.
- `409`: refused because of state. Data moves add `"reason"` (`hardlinks`, `space`, …) and a `plan`; a filter selection that grew answers `"reason":"grew"` with the new `count`; an add of a torrent already present answers 409.
- `POST /api/torrents` answers `400` (bad request), `404` (`torrent_path` not found), `409` (already added) or `500` (the engine failed), with `{"error","targets"}`.

### Hashes, engines and copies

- `:info_hash` is the 40-character hex v1 hash. Some routes resolve a shorter prefix and act on the first match: always send the full hash.
- Engines are addressed by id under `/api/engines/:id/...`; `/api/race/...` and `/api/hoard/...` are aliases for the two engines that always exist. Exception: `GET /api/race/torrents` lists **every** engine with the race role.
- The same torrent can be in several engines (one **copy** per engine). Torrent routes pick a copy with **`?engine=<id>`**. The legacy spelling **`?agent=`** (a 3.x name) is also accepted: `local` or empty means the default, `local-<id>` means engine `<id>`.
- List rows carry a label in the field `agent` (legacy name): `local-<engine>` for a local copy, `<node>-<engine>` for a row merged from a declared node. Per-torrent routes answer 404 for a node row; `/api/selection/*` relays reannounce, recheck, stop/start, remove, category and set location to the node that holds it.

## Quick examples

Status (fields: *Monitoring*):

```bash
curl -s -H "X-Api-Key: $KEY" "$HY/api/status"
```

First 100 hoard torrents of category `tv`, newest first (server-side filter and paging):

```bash
curl -s --compressed -H "X-Api-Key: $KEY" \
  "$HY/api/hoard/page?category=tv&limit=100&sort=added_time"
```

Create a category (there is no default save path, so adds need one):

```bash
curl -s -H "X-Api-Key: $KEY" -H "Content-Type: application/json" \
  -d '{"name":"tv","save_path":"/data/tv","mode":"hoard"}' "$HY/api/categories"
```

Add a `.torrent` file with a category (multipart):

```bash
curl -s -H "X-Api-Key: $KEY" \
  -F "torrents=@show.s01e01.torrent" -F "category=tv" "$HY/api/torrents/upload"
```

Add `-F "seed_mode=true"` to seed data already on disk without hashing it (**seed mode**; risks: [Adding Torrents and Existing Data](https://github.com/Kheopsian/Hydranos/wiki/Adding-Torrents-and-Existing-Data)).

Stop every hoard torrent of category `old` (a **bulk action** by filter; count first, send the count as `expect`):

```bash
N=$(curl -s -H "X-Api-Key: $KEY" "$HY/api/hoard/page?category=old&limit=1" | jq .filtered)
curl -s -H "X-Api-Key: $KEY" -H "Content-Type: application/json" \
  -d "{\"selection\":{\"filter\":\"category=old\",\"view\":\"hoard\",\"expect\":$N},\"params\":{}}" \
  "$HY/api/selection/stop"
# 202 {"job":"...","total":N}  ->  poll GET /api/selection/jobs/<job>
```

## Torrents and adding

Status: ✓ works · ◐ works with a caveat. Stub routes appear only in *Routes that are stubs in 4.3.1*.

| Route | Method | Purpose | Status |
|---|---|---|---|
| `/api/torrents/upload` | POST multipart | Add a `.torrent`. File part `torrents`, `torrent` or `file`; fields `category`, `savepath`/`save_path`, `tags`, `engine` or `mode` (`race`/`hoard`: the engine with that role), `paused`/`stopped`, `skip_checking`/`seed_mode`/`skip_recheck`, `create_subfolder`. Unknown `engine` or `mode` → 400; already added → 409. Body limit 2 MB. | ✓ |
| `/api/torrents` | POST JSON | Add from exactly one of `torrent_path` (a file **on the Hydranos host**), `torrent_url` (fetched by Hydranos) or `magnet_uri`; plus `category`, `save_path`, `tags`, `engine` or `mode`, `stopped`, `seed_mode` (or `skip_recheck`), `create_subfolder`. Magnet → `202 {"info_hash","status":"resolving"}`; `seed_mode` and `stopped` apply to magnets too. | ✓ |
| `/api/torrents/:info_hash` | DELETE | Remove. `?delete_files=true` deletes data; `?engine=` removes one copy, otherwise all. Files another torrent still reads are kept. | ✓ |
| `/api/torrents/:info_hash/{files,torrent}` | GET | File list; the `.torrent` bytes from the store. | ✓ |
| `/api/torrents/:info_hash/trackers` | GET, POST | Read / edit (saved at once). POST `{"op":"add"\|"remove","urls":[...]}`, `{"op":"replace","from","to"}` or `{"op":"set","tiers":[ ["u1"], ["u2"] ]}`. `.../add-tracker` takes `{"url"}`. | ✓ |
| `/api/torrents/:info_hash/reannounce` | POST | Announce now, one copy only: the `?engine=` one, else the first engine holding it. `429` in the 60 s cooldown. | ✓ |
| `/api/torrents/:info_hash/peers` | POST | `{"peers":["203.0.113.5:16172"]}`: dial these peers. | ✓ |
| `/api/torrents/:info_hash/copy` | POST | `{"engine":"vpn1"}`: add a second copy in another engine, same files. | ✓ |
| `/api/torrents/:info_hash/engine` | POST | `{"engine":"vpn1"}`: move the torrent to another engine, files stay where they are. `?engine=` names the source copy. The target adopts the source's state (pieces, trackers, counters, stopped) before the source lets go; if either refuses, nothing changes. | ✓ |
| `/api/torrents/:info_hash/graduate` | POST | `{"engine","category","save_path","allow_breaking_hardlinks":false}`: queue a `graduate` job (data move + engine change). `save_path` defaults to the category's. Same checks as a category change (unsafe path, shared files, hardlinks, free space: 409 with `reason`); a failure moves the files back. | ✓ |
| `/api/torrents/add-defaults` | GET | `{"create_subfolder","skip_recheck"}`: the defaults the Add tab starts from (`create_subfolder` is `[daemon] create_torrent_folder`). | ✓ |
| `/api/torrents/export` | POST form | `format=zip\|txt\|csv`, `selection` (JSON) **or** `hashes`, `strip_trackers=1`. Streams a download. | ✓ |
| `/api/{hoard,race}/torrents/:info_hash` | GET | One row with live detail. | ✓ |
| `/api/{hoard,race}/torrents/:info_hash/category` | POST | `{"category","move_files":false,"allow_breaking_hardlinks":false}`. With `move_files`, `202` + job (or a graduation if the mode differs). | ✓ |
| `/api/{hoard,race}/torrents/:info_hash/location` | POST | **Set location**: `{"location":"/abs/path","allow_breaking_hardlinks":false}`. `202` + job when bytes move, `200 "moved":false` if already there. | ✓ |
| `/api/{hoard,race}/torrents/:info_hash/move-preview` | GET | What a move would do, without doing it. | ✓ |
| `/api/{hoard,race}/torrents/:info_hash/tags` | POST | `{"tags":[...]}` replaces the whole set. | ✓ |
| `/api/{hoard,race}/torrents/:info_hash/{pause,resume}` | POST | Stop / start one torrent. | ✓ |
| `/api/hoard/torrents/:info_hash/{pin,unpin}` | POST | Force download / stop forcing. | ✓ |
| `/api/hoard/torrents/:info_hash/verify` | POST | Recheck (`?engine=` picks the copy). | ✓ |
| `/api/race/torrents/:info_hash/purge` | POST | Remove the **race copy** (engine first). Its data is deleted when no other copy of the torrent remains; files another torrent reads are kept. Other copies are untouched. `404` if the torrent is not in race. | ✓ |
| `/api/race/timeline/:info_hash`, `/api/trackers` | GET | Race timeline; Trackers tab table. | ✓ |

How an add picks its engine and save path: [Categories and Routing](https://github.com/Kheopsian/Hydranos/wiki/Categories-and-Routing). Without `engine` or `mode`, the category decides. `create_subfolder` gives a single-file torrent a folder of its own, named after the torrent.

## Lists and engines

Each `/api/engines/:id/...` row also answers as `/api/hoard/...` and `/api/race/...`, except `pinned`, `pause-all` and `resume-all` (hoard alias only).

| Route | Method | Purpose | Status |
|---|---|---|---|
| `/api/engines` | GET | Local engines (`id`, `role`, `listen_port`, `torrents`…). | ✓ |
| `/api/engines` | POST | `{"id","role":"race"\|"hoard","listen_port","bind_interface"}`: add an extra engine to the config. Answers `restart_required: true`. | ✓ (after restart) |
| `/api/engines/:id` | DELETE | Remove an extra engine from the config. `409` with the count while it still holds torrents (move or remove them first); `race` and `hoard` → 400. An empty engine keeps its listener until restart. | ✓ (after restart) |
| `/api/engines/:id/page` | GET | Paged list (see below). | ✓ |
| `/api/engines/:id/torrents` | GET | The engine as one array, with the page filters applied when given (no paging: `offset`/`limit` are ignored). | ✓ |
| `/api/engines/:id/pinned` | GET | Forced-download hashes. | ✓ |
| `/api/engines/:id/pause` | POST | `{"hashes":[...],"paused":true}`: stop/start exact hashes. | ✓ |
| `/api/engines/:id/torrents/bulk` | POST | `{"action":"stop"\|"start","hashes":[...],"exclude":[...],"all":false}`. Unknown fields → 400; an empty `hashes` is **not** "all". | ✓ |
| `/api/engines/:id/{pause-all,resume-all}` | POST | Stop / start every torrent of the engine. | ✓ |
| `/api/engines/:id/listen-port` | POST | `{"port":16172}`: rebind the TCP listener live; announces carry the new port. Answers `"persisted": false`. | ◐ not persisted; uTP keeps the startup port |
| `/api/engines/:id/dial-limits` | POST | `{"max_dials_per_sec","max_connections"}` (0 = unlimited). | ◐ not persisted |
| `/api/hoard/stats` | GET | Totals. | ✓ |
| `/api/hoard/download-slots` | GET | Configured `active_downloads`; other counters are zeros. | ◐ |
| `/api/race/settings` | GET | Race settings echo. | ◐ |
| `/api/startup-pause`, `/api/startup-pause/release` | GET, POST | Startup gate: `{"held":[engines],"holding"}`; POST releases it and answers `released`. With `start_paused`, a held engine makes no dial and no announce until released ([Engines: Race and Hoard](https://github.com/Kheopsian/Hydranos/wiki/Engines-Race-and-Hoard)). | ✓ |

**Paging parameters** of `.../page`: `offset` (default 0), `limit` (default 500, clamped to 1–5000), `sort` (a row field, default `added_time`), `order=asc` (default descending), `facets=1` (adds category/tag/tracker counts), `fields=hash` (hashes only). Filters: `search` (words ANDed, or a hex hash prefix of 6+ characters), `category`, `tag`, `tracker`, `error_class` and their `_not` variants (comma-separated lists, `__none__` = without one), `state` (a row state such as `seeding`, or `__active__`, `__error__`, `__tracker_err__`, `__pinned__`). The answer is `{"total","filtered","offset","limit","rows","facets"}`. A page also merges the other local engines of the same role and the rows of declared nodes. The same filters apply to `GET /api/{hoard,race}/torrents` and `/api/engines/:id/torrents`.

## Selections (bulk actions)

`POST /api/selection/:action` runs one action on many torrents as an in-memory **bulk task** (the web UI's context menu uses it). It answers `202 {"job","total"}`; poll `GET /api/selection/jobs/:id` (kept 1 h after it ends, lost on restart). `POST /api/selection/jobs/:id/cancel` stops between two torrents. These tasks are not the persistent jobs of the **Jobs** tab: see [Jobs, Bulk Actions and Moving Data](https://github.com/Kheopsian/Hydranos/wiki/Jobs-and-Moving-Data).

Body: `{"selection": ..., "params": {...}}`, strict (unknown keys → 400).

- By rows: `"selection":{"items":[{"hash":"<40 hex>","agent":"local-hoard"}]}`. Each row acts on its own copy (the engine in its `agent` label).
- By filter: `"selection":{"filter":"category=tv&state=seeding","view":"hoard","expect":<count>}`, optional `"exclude":[items]`. `filter` takes only the filter keys listed under *Lists and engines*; `view` is an engine id (default `hoard`); `expect` is required.

| Action | `params` |
|---|---|
| `stop`, `start`, `pin`, `unpin`, `reannounce`, `recheck` | `{}` (`recheck` covers race copies too) |
| `tags` | `{"tags":["a"],"op":"add"\|"remove"}` |
| `category` | `{"category","move_files":false,"allow_breaking_hardlinks":false}` |
| `location` | `{"location":"/abs/path","allow_breaking_hardlinks":false}` |
| `remove` | `{"delete_files":false}` |
| `copy`, `move-engine` | `{"engine":"vpn1"}` |
| `handoff` | `{"node","engine","then":"keep"\|"remove"}` |
| `node-fetch` | `{"node","from_engine","engine"}` |
| `node-move` | `{"node","engine"}` |

## Categories and tags

| Route | Method | Purpose | Status |
|---|---|---|---|
| `/api/categories` | GET, POST | List / create `{"name","save_path","mode":"race"\|"hoard","graduate_to","transit"}` → `201`. `mode` is case-insensitive, defaults to `race`, any other value → 400; an existing name → 409. | ✓ |
| `/api/categories/:name` | PUT, DELETE | Replace; a different `name` in the body **renames** it and relabels its torrents (`{"name","relabelled"}`; 409 if the new name exists) / delete: the label is cleared from its torrents (`{"cleared"}`). | ✓ |
| `/api/categories/orphans` | GET | Labels worn by torrents that match no category: `[{"name","torrents","mode","save_path"}]` (most common role and save path). | ✓ |
| `/api/tags` | GET | Tags. | ✓ |

## Trackers and announces

| Route | Method | Purpose | Status |
|---|---|---|---|
| `/api/announce/{health,policy}`, `/api/announce/errors?host=` | GET | Tracker health, live policy, error details. | ✓ |
| `/api/announce/ip-modes` | GET, POST | `{"host","mode"}` IP family per tracker. | ✓ |
| `/api/announce/passkeys` | GET, POST | `{"host","passkey"}` (empty passkey clears). | ✓ |
| `/api/announce/min-seed` | POST | `{"host","hours"}` or `{"host","clear":true}`. | ✓ |
| `/api/announce/{mute,hidden}` | POST | `{"host","muted"}` / `{"host","hidden"}` or `{"hosts":[...],"hidden"}`. | ✓ |

Meaning of each setting: [Trackers and Announces](https://github.com/Kheopsian/Hydranos/wiki/Trackers-and-Announces).

## Workflows and jobs

| Route | Method | Purpose | Status |
|---|---|---|---|
| `/api/workflows` | GET, POST | List / create or replace a workflow. | ✓ |
| `/api/workflows/:id`, `/api/workflows/:id/run` | DELETE, POST | Delete; run now (`?dry=1`: dry run). | ✓ |
| `/api/workflows/{fields,activity,links}`, `.../preview` | GET, POST | Editor fields, activity log, link status; POST preview: torrents a draft would touch. | ✓ |
| `/api/jobs`, `/api/jobs/:id` | GET | Persistent jobs (`?limit=`, default 100); one job. | ✓ |
| `/api/jobs/:id` | DELETE | Cancel. A queued job → `{"status":"cancelled"}`; a running graduation stops between two files and is rolled back (`"cancelling"`); another running move → 409 (it finishes); unknown id → 404. | ✓ |

Bodies: [Workflows](https://github.com/Kheopsian/Hydranos/wiki/Workflows), [Jobs, Bulk Actions and Moving Data](https://github.com/Kheopsian/Hydranos/wiki/Jobs-and-Moving-Data).

## Race drain

| Route | Method | Purpose | Status |
|---|---|---|---|
| `/api/drain/{status,history,graduations}` | GET | Volumes, past drains, graduations in progress. | ✓ |
| `/api/drain/now?volume=` | POST | Drain now. | ✓ |
| `/api/drain/policy` | POST | `{"volume","enabled","high_watermark","low_watermark"}` or `{"volume","inherit":true}`; optional `"quota_gb"` (declared capacity in GB of 10^9 bytes, `0` removes it), on its own or with either form. | ✓ |

A **quota** is for a volume shared with other data (a seedbox slot): with one, the volume's `used` is what Hydranos torrents wrote there and `free` is the smaller of the quota left and the disk's free space; the drain, the add refusal and the panel all use it. Each volume in `GET /api/drain/status` carries `quota` (bytes, or `null`), `basis` (`"quota"` or `"disk"`: what `total`/`used`/`free`/`used_pct` are measured against) and the disk underneath as `disk_total`, `disk_used`, `disk_free`.

See [Race Drain, Graduation and Seed Obligations](https://github.com/Kheopsian/Hydranos/wiki/Race-Drain-and-Graduation).

## Nodes

| Route | Method | Purpose | Status |
|---|---|---|---|
| `/api/nodes` | GET, POST | List (probed live) / declare `{"name","url","api_key"}`. | ✓ |
| `/api/nodes/test` | POST | Probe without saving. | ✓ |
| `/api/nodes/:name` | DELETE | Forget a node (the remote is untouched). | ✓ |
| `/api/nodes/enrol`, `/api/nodes/register` | POST | One-time token + install command; called by the new machine with the token. A name already taken is refused before the token is spent. | ✓ |
| `/api/nodes/:name/{handoff,fetch,move-engine}` | POST | Push / pull / move between that node's engines (bodies: [Nodes and Multiple Machines](https://github.com/Kheopsian/Hydranos/wiki/Nodes-and-Multiple-Machines)). | ✓ |
| `/node/:name/open` | GET | Redirect to the node's UI with its key. | ✓ |
| `/install.sh` | GET | Enrolment script (public). | ✓ |

The legacy 3.x routes `/api/agents*` were removed in 4.4. Any path under `/api/agents` and any method gets `410 Gone` with `{"error":"…","see":["/api/engines","/api/nodes"]}`. Up to 4.3.1, `GET /api/agents` listed the local engines as `local-<id>`, and the other agent routes were stubs. Use `/api/engines` for the engines on this machine and `/api/nodes` for other machines ([Upgrading from 3.x](https://github.com/Kheopsian/Hydranos/wiki/Upgrading-from-3x)).

## Magnets, watched folders, import, dedup

| Route | Method | Purpose | Status |
|---|---|---|---|
| `/api/magnets`, `/api/magnets/:hash`, `/api/magnets/:hash/retry` | GET, DELETE, POST | Magnets resolving or failed; drop; retry. | ✓ |
| `/api/watch` | GET, PUT | Watched folders. PUT takes the whole list `[{"path","category","engine","paused","enabled"}]`; path absolute and existing, category must exist. | ✓ |
| `/api/import/qbit/{preview,start}`, `.../{status,events}` | POST, GET | Import from a qBittorrent instance; progress (`events` is SSE). | ✓ |
| `/api/import/transmission/{upload,preview,start}` | POST | Import from Transmission (`upload` multipart, up to 4 GiB). | ✓ |
| `/api/import/retry` | POST | Retry the torrents the last import failed, with its choices and (for qBittorrent) its login, held in memory: no password to resend. `{"job_id","torrents"}`; `404` no import, `409` one is running, `400` nothing left to retry. | ✓ |
| `/api/import/check-paths` | POST | `{"paths":[...]}`: which paths this host can see. | ✓ |
| `/api/provenance` | GET | Where the library was imported from. | ◐ (see below) |
| `/api/dedup/config`, `/api/dedup/stats` | POST, GET | `{"enabled":true}` (restart to apply); duplicate-data counters. | ✓ |

**Import status** (`GET /api/import/qbit/status`, and each `events` frame) describes the latest import: `job_id`, `running`, `phase`, `total`, `done`, `seeded` (complete, data found), `downloading` (to check or download), `stopped` (added stopped), `skipped`, `failed`, `failures` (up to 200 `{"name","hash","error"}`), `retryable` (true when `POST /api/import/retry` has something to do), `finished`, `error`, `current`. A qBittorrent export is retried on a dropped connection, a 5xx or a 429 (4 attempts, with backoff), and the import logs in again if the session expires.

Import wizard: [Migrating from qBittorrent](https://github.com/Kheopsian/Hydranos/wiki/Migrating-from-qBittorrent). Magnets, watched folders, dedup: [Adding Torrents and Existing Data](https://github.com/Kheopsian/Hydranos/wiki/Adding-Torrents-and-Existing-Data).

## Network and IP filter

| Route | Method | Purpose | Status |
|---|---|---|---|
| `/api/network/{interfaces,engines}`, `/api/public-ip` | GET | Interfaces that are up; exit IP per engine (`?refresh=1`); process exit IP. | ✓ |
| `/api/network/mode` | GET, POST | Network tab: POST writes ports, interfaces and other keys, `restart_required`. A port outside 1–65535 → 400. Several modes are not implemented. | ◐ |
| `/api/network/check` | POST | Partly constant results. | ◐ |
| `/api/ipfilter` | GET, PUT | Status / `{"enabled","sources":[...],"refresh_hours"}`. | ✓ |
| `/api/ipfilter/bans`, `/api/ipfilter/reload` | POST, DELETE | Ban `{"ip","reason"}` / unban `{"ip"}`; POST reload reloads block lists. | ✓ |

What works and what does not on the Network tab: [Networking](https://github.com/Kheopsian/Hydranos/wiki/Networking-Modes).

## Settings, setup, restart

| Route | Method | Purpose | Status |
|---|---|---|---|
| `/api/setup` | GET, POST | First run: `{"needs_setup","network_storage",...}` (`network_storage` is `"network share"` when `data_dir` is on NFS, SMB or FUSE, where the database cannot use WAL; `""` otherwise; always `""` up to 4.3.1) / create the admin `{"username","password"}` (8+ chars) → API key. Public, loopback or private network only. | ✓ |
| `/api/login` | POST | `{"username","password"}` → `{"api_key"}`. Public. | ✓ |
| `/api/auth/password` | POST | `{"password"}` (8+ chars): stored hashed in `[auth] password_hash`, as setup does. | ✓ |
| `/api/settings` | GET, POST | Whole config as JSON, **secrets included** / `{"changes":[{"section","key","value"}]}` edits existing keys only. | ✓ |
| `/api/settings/reset` | POST | Reset to defaults (keeps login, key, data dir). | ✓ |
| `/api/settings/restart`, `/api/restart` | POST | Answer, then stop cleanly (the same path as SIGTERM: `stopped` to trackers, resume data flushed) and exit with code **75** for the supervisor to restart (systemd `Restart=on-failure` included). | ✓ |
| `/api/fs/browse?path=` | GET | List folders on the Hydranos host. | ✓ |
| `/api/update-check` | GET | New release available. | ✓ |

A forgotten password is reset on the host with `hydranos reset-password <password> [config]`: [Security and Access](https://github.com/Kheopsian/Hydranos/wiki/Security-and-Access).

## Monitoring

| Route | Method | Purpose | Status |
|---|---|---|---|
| `/health` | GET | Public, static `healthy` + version + uptime. | ◐ |
| `/metrics` | GET | Public, Prometheus: `hydra_up`, `hydra_uptime_seconds`, `hydra_torrents{engine}`. | ✓ |
| `/api/startup` | GET | Public; `ready` is always true. | ◐ |
| `/api/status` | GET | Header and Overview figures. | ✓ |
| `/api/events` | GET | SSE stream of status snapshots. | ✓ |
| `/api/logs`, `/api/logs/stream` | GET | Recent log lines (query filters ignored); SSE tail. | ◐ |
| `/api/stats/baseline` | GET, POST | Lifetime counter baseline `{"total_uploaded","total_downloaded"}`. | ✓ |
| `/api/health/anomalies` | GET | Mostly zeros. | ◐ |
| `/api/benchmark/{records,range,current,race-events}`, `.../trackers/{current,range}` | GET | Benchmark data, tracker stats. | ✓ |

**`/api/status` fields** (also the `status_snapshot` frame of `/api/events`, 1/s). Bytes, bytes/s. Engines `race` and `hoard` only.

| Field | Meaning |
|---|---|
| `version`, `uptime` (s), `server_ts` (Unix s) · `day_uploaded`, `day_downloaded` | Process · since local midnight, all engines. |
| `baseline.session_*` · `.total_*` · `.global_*` | Since start · stored counter (`/api/stats/baseline` + removed torrents) · stored + loaded torrents' lifetime. |
| `hoard.active_upload_rate`, `.active_download_rate`, `.active_peers`, `.torrents_with_peers`, `.torrents_uploading`, `.total_torrents`, `.torrents_announced`, `.swarm_leechers`, `.unseeded_peers`, `.listen_port` | Hoard, live. |
| `hoard.session_uploaded`, `.session_downloaded` · `race.session_downloaded` | Per engine since start. |
| `race.session_uploaded`, `race.session_ratio` | **All engines** since start. |
| `race.total_upload_rate`, `.total_download_rate`, `.total_peers`, `.torrents`, `.active_seeds`, `.active_downloads`, `.torrents_with_peers` | Race, live. |
| `hoard.seed_size`, `race.seed_size`, `storage.seeded_bytes` | Last tracker pass (≤ 30 s); a cross-seed counts per tracker. `null` before. |
| `storage.data_bytes`, `.shared_bytes`, `.missing_files`, `.measured_torrents`, `.measured_at` | Link scan (≤ 1 day old). `null` before. |
| `engine_counters.*` | Peer-wire diagnostics. |
| `hoard.running`, `.stagger_complete`, `race.session_grabbed`, `tunnels` | Constants. |

**`/api/benchmark/current`**: `ts`, `global_uploaded`/`_downloaded` (as `baseline.global_*`), `hoard_upload_rate`, `hoard_peers`, `hoard_with_peers`, `hoard_uploading`, `race_upload_rate`, `race_download_rate`, `race_uploading`, `race_torrents`, `race_session_uploaded` (all engines), `iowait_pct`, `open_fds`, `arc_*` (host ZFS ARC). Always 0: `*_announce_rate`, `*_announce_fail_rate`, `race_avg_share`, `hoard_session_uploaded`. `race_peers` is the race torrent count.

**`/api/benchmark/trackers/current`**: one row per engine and tracker host: `engine`, `tracker`, `torrents`, `active` (with a peer), `peers`, `upload_rate`, `download_rate`, `cum_uploaded`/`cum_downloaded` (lifetime), `seed_size`, `ts`. `range` routes take `start`/`end` (Unix s, default last 24 h); `trackers/range` also `tracker=`.

What each monitoring route reports: [Monitoring and Logs](https://github.com/Kheopsian/Hydranos/wiki/Monitoring-and-Logs).

## qBittorrent shim

Documented on [qBittorrent Shim and Automation](https://github.com/Kheopsian/Hydranos/wiki/qBittorrent-Shim-and-Automation). Changes in 4.3.1:

- `torrents/files`, `torrents/properties` and `torrents/trackers` read `hash` from a POST form body as well as the query string (cross-seed sends every call as a POST). An unknown hash on `torrents/trackers` is a 404.
- `torrents/categories` and `torrents/tags` answer POST as well as GET.
- `torrents/export` serves the stored `.torrent` of `hash`.
- `torrents/info` lists every engine, each torrent once, with `tracker` filled; `hashes=all` means every torrent on the write endpoints; `torrents/files` gives real piece ranges and progress, with the torrent's folder in multi-file names.

## Routes that are stubs in 4.3.1

These routes answer but do not do what their name says. The live listen-port and dial-limit changes and `/api/provenance`: callouts below.

| Route | What it answers | Use instead |
|---|---|---|
| `GET /api/hoard/download-slots` | The configured `active_downloads` and zeros. | `active_downloads` in the config, restart ([Configuration: Engines and Race Drain](https://github.com/Kheopsian/Hydranos/wiki/Configuration-Engines)). |
| `GET /api/arr-cleanup/scan` | An empty scan. | — |
| `/api/network/wireguard*`, `/api/vpn-speedtest/*`, `/api/benchmark/compare`, `/api/benchmark/race-snapshots/:info_hash`, `GET /api/port-forward` (ports and IPs real, reachability constant) | Constants, empty lists, or a fixed 400. | — |

> **Known limitation in 4.3.1:** a live listen-port or dial-limit change answers OK and applies at once (the TCP listener moves and announces carry the new port) — it is lost at the next restart, and uTP keeps the port it was opened on at startup — change `listen_port` (or the dial limits) in the config and restart to make it permanent (details on [Networking](https://github.com/Kheopsian/Hydranos/wiki/Networking-Modes)).

> **Known limitation in 4.3.1:** `GET /api/provenance` is not written by any 4.x import, so after a 4.3.x import it answers `{"present":false}` (a 3.x import's record is still shown).

## Routes removed in 4.4

These were stubs up to 4.3.1: they returned a success-shaped answer and did nothing. No screen called them. They are gone now. A removed path returns `404`. A removed method on a path that keeps its `GET` returns `405`.

| Route | Up to 4.3.1 | Since 4.4 | Use instead |
|---|---|---|---|
| `POST\|DELETE /api/hoard/download-slots` | Echoed the GET, changed nothing. | 405 | `active_downloads` in the config, restart. |
| `POST /api/race/settings` | Echoed the config, ignored the body. | 405 | `POST /api/settings`, then restart. |
| `GET\|POST /api/opt/flags` | 3.x constants; POST always `400 unknown flag`. | 404 | — |
| `GET /api/race/choking` | Always `null`. | 404 | — |
| `POST /api/hoard/verify-downloading` | `{"verified":0}`, did nothing. | 404 | `POST /api/selection/recheck`. |
| `POST /api/hoard/restart-stuck` | `{"restarted":0}`, did nothing. | 404 | `stop` then `start` through `/api/selection/*`. |
| `POST /api/arr-cleanup/execute` | `{"errors":null,"removed":0}`. | 404 | — |
| `/api/agents`, `/api/agents/*` (every method) | See *Nodes* above. | 410, body names `/api/engines` and `/api/nodes` | `/api/engines`, `/api/nodes`. |
| `POST /api/jobs/move-remote` | Always 400. | 410, body names `/api/selection/handoff` | `POST /api/selection/handoff`. |
