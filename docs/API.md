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

**Public routes** (no key): `/health`, `/metrics`, `/`, `/static/*`, `/changelog.md`, `/install.sh`, `GET|POST /api/setup`, `POST /api/login`, `GET /api/startup`, `POST /api/v2/auth/login`, `POST /api/v2/auth/logout`, `POST /api/nodes/register` (enrolment token instead of the key), and `POST /api/auth/api-key/confirm` (the pending key of a rotation instead of the key).

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
| `/api/torrents/:info_hash/files` | GET | `{"files":[{"path","size","done","progress"}]}`: `done` is the bytes of that file already held (from the pieces held, by overlap), `progress` 0-1; both `null` when the torrent is incomplete and has no piece map to read. | ✓ |
| `/api/torrents/:info_hash/trackers` | GET, POST | Read / edit (saved at once). POST `{"op":"add"\|"remove","urls":[...]}`, `{"op":"replace","from","to"}` or `{"op":"set","tiers":[ ["u1"], ["u2"] ]}`. `.../add-tracker` takes `{"url"}`. | ✓ |
| `/api/torrents/:info_hash/reannounce` | POST | Announce now, one copy only: the `?engine=` one, else the first engine holding it. `429` in the 60 s cooldown. | ✓ |
| `/api/torrents/:info_hash/peers` | POST | `{"peers":["203.0.113.5:16172"]}`: dial these peers. | ✓ |
| `/api/torrents/:info_hash/share-limits` | GET, POST | The torrent's share limits: `{"info_hash","copies":[{"engine","ratio_limit","seeding_time_limit","inactive_seeding_time_limit","effective":{"ratio","seeding_time","inactive_seeding_time"},"action"}]}` (own values -2 = the engine's, -1 = none; `effective` -1 = none; minutes). POST `{"ratio_limit","seeding_time_limit","inactive_seeding_time_limit"}`, each optional: stored for the torrent (every copy), answers the GET. None → 400; unknown hash → 404. The detail routes carry the same object as `share_limits`. New in 4.4. | ✓ |
| `/api/torrents/:info_hash/limits` | GET, POST | The torrent's own speed caps, per local copy (`?engine=` for one): `{"info_hash","copies":[{"engine","upload_limit","download_limit","up_kib","down_kib"}]}` (`*_limit` in bytes/s, 0 = none). POST `{"up_kib","down_kib"}` (KiB/s, either optional, 0 or negative = no cap of its own): live at once and saved with the torrent, so it survives a restart and an engine move; answers what the engine now holds. Neither field → 400; unknown hash → 404. The engine's cap still applies above it: the narrower one binds. New in 4.4. | ✓ |
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
| `/api/engines` | GET | Local engines (`id`, `role`, `listen_port`, `torrents`…), each with `stats`: `torrents`, `states` (`stopped`, `checking`, `downloading`, `seeding`, `error`, `paused`), rates, `peers`, `with_peers`, `uploading`, lifetime and session bytes, `seed_size`, announce figures, rate caps, `choking`, `unchoke_slots`, `listening`, `held`. | ✓ |
| `/api/engines` | POST | `{"id","role":"race"\|"hoard","listen_port","bind_interface"}`: add an extra engine to the config. Answers `restart_required: true`. A non-empty `bind_interface` outside Linux → 400 with the reason (see `bind_interface` below). | ✓ (after restart) |
| `/api/engines/:id` | DELETE | Remove an extra engine from the config. `409` with the count while it still holds torrents (move or remove them first); `race` and `hoard` → 400. An empty engine keeps its listener until restart. | ✓ (after restart) |
| `/api/engines/:id/page` | GET | Paged list (see below). | ✓ |
| `/api/engines/:id/torrents` | GET | The engine as one array, with the page filters applied when given (no paging: `offset`/`limit` are ignored). | ✓ |
| `/api/engines/:id/pinned` | GET | Forced-download hashes. | ✓ |
| `/api/engines/:id/pause` | POST | `{"hashes":[...],"paused":true}`: stop/start exact hashes. | ✓ |
| `/api/engines/:id/torrents/bulk` | POST | `{"action":"stop"\|"start","hashes":[...],"exclude":[...],"all":false}`. Unknown fields → 400; an empty `hashes` is **not** "all". | ✓ |
| `/api/engines/:id/{pause-all,resume-all}` | POST | Stop / start every torrent of the engine. | ✓ |
| `/api/engines/:id/listen-port` | POST | `{"port":16172}`: moves the TCP listener(s) **and the uTP socket** live (live TCP peers kept; uTP connections on the old socket end and come back on the new port), then announces and the home-router mapping follow, and `listen_port` is written to the engine's section (`[race]`/`[hoard]` or the `[[engine]]` block's `session`). Answers after the bind: `{"ok","engine","port","previous_port","persisted"}`. A port that cannot be bound (in use, say) → **409** `{"error","engine","port"}`: the engine still listens on `port` (the old one), nothing written. Another engine's port, or an engine whose port comes from gluetun or its WireGuard tunnel's forwarding → 409. Engine not on the network → 503. Since 4.4 (4.3: TCP only, answered before binding, not saved, a failed bind stopped the listener). | ✓ |
| `/api/engines/:id/dial-limits` | POST | `{"max_dials_per_sec","max_connections"}` (0 = unlimited): applied to the live limiter and written as `max_dials_per_sec` / `max_connections` in the engine's section, read at the next start. Answers the limiter's values and `persisted`. Since 4.4 (4.3: not saved, and `max_dials_per_sec` was never read from the file). | ✓ |
| `/api/engines/:id/share-limits` | GET, POST | The engine's share limits: `{"engine","max_ratio","max_seeding_time","max_inactive_seeding_time","share_limit_action"}` (-1 = off, times in minutes, action `stop` \| `remove` \| `remove_with_files`). POST any of these fields: written to the config (`[race]`/`[hoard]` or the `[[engine]]` block's `session`, created if absent) and read by the share-limit worker on its next pass. A bad action → 400, unknown engine → 404. New in 4.4. | ✓ |
| `/api/engines/:id/rate-limits` | GET, POST | The engine's speed caps. GET `{"engine","upload_kib","download_kib"` (config) `,"upload_limit","download_limit"` (live, bytes/s) `,"client_upload_limit","client_download_limit"}` (the qBit-shim global cap above every engine). POST `{"upload_kib","download_kib"}` (KiB/s, either optional, 0 = unlimited): written to the config as `upload_rate_limit` / `download_rate_limit` in **bytes/s** (in `[race]`/`[hoard]` or the `[[engine]]` block's `session`, created if absent) and applied live. Unknown engine → 404. New in 4.4. | ✓ |
| `/api/hoard/stats` | GET | Totals. | ✓ |
| `/api/hoard/download-slots` | GET | Configured `active_downloads`; other counters are zeros. | ◐ |
| `/api/race/settings` | GET | Race settings echo; `upload_rate_limit` / `download_rate_limit` are the configured caps in bytes/s (constant 0 up to 4.3.1). | ◐ |
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
| `limits` | `{"up_kib","down_kib"}`: each torrent's own speed caps, KiB/s, either optional, 0 = no cap of its own (see `/api/torrents/:info_hash/limits`). Neither → 400. New in 4.4. |
| `share_limits` | `{"ratio_limit","seeding_time_limit","inactive_seeding_time_limit"}`: each torrent's own share limits, each optional, -2 = its engine's, -1 = none, minutes (see `/api/torrents/:info_hash/share-limits`). A store write, set-based: a few transactions for the whole selection. None → 400. New in 4.4. |

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
| `/api/announce/{health,policy}`, `/api/announce/errors?host=` | GET | Tracker health, live policy, error details. Every engine (extra ones included, keyed by id); `badges` count distinct trackers, not one per engine. | ✓ |
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
| `/api/jobs/:id` | DELETE | Cancel. A queued job → `{"status":"cancelled"}`; a `waiting` move to a node → `"cancelled"`, both copies kept; a running graduation stops between two files and is rolled back (`"cancelling"`); another running move → 409 (it finishes); unknown id → 404. | ✓ |

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
| `/api/nodes/:name` | PATCH | Edit (4.4): any of `{"name","url","api_key"}`. A new URL or key is probed first (400 if the node does not answer, or on a loopback URL); a new name taken → 409; moves waiting on the node follow the rename. `{"status","name","health"}`. | ✓ |
| `/api/nodes/:name/rotate-key` | POST | Rotate the node's API key (4.4, node on 4.4): the node mints a key, adopts it once this instance presents it back, refuses the old one from then on; stored here. `{"status","name","api_key","health"}`: the new key is returned once, for the node's other clients. 502 with the reason if the node refused (it then keeps its old key). | ✓ |
| `/api/auth/api-key/rotate` | POST | Node side, step 1: mint a pending key (valid 10 min, in memory), the current key still valid. `{"pending_key","expires_in"}`. | ✓ |
| `/api/auth/api-key/confirm` | POST | Node side, step 2, authenticated by the pending key in `X-Api-Key`: written to `[daemon] api_key`, live at once; the old key is refused from the next request. 401 for any other key. Logged-in browser sessions stay valid. | ✓ |
| `/api/nodes/enrol`, `/api/nodes/register` | POST | One-time token + install command; called by the new machine with the token. A name already taken is refused before the token is spent. | ✓ |
| `/api/nodes/:name/{handoff,fetch,move-engine}` | POST | Push / pull / move between that node's engines (bodies: [Nodes and Multiple Machines](https://github.com/Kheopsian/Hydranos/wiki/Nodes-and-Multiple-Machines)). | ✓ |

**Move to a node** (`handoff` with `"then":"remove"`, 4.4): the answer carries `"job"`, a persistent job of type `handoff` in state `waiting` (Jobs tab, `GET /api/jobs`), with `params` `{"name","target","node","engine","deadline"}`. Every 30 s it asks the node, through `/api/engines/<id>/page` (every engine of the node when none was named), how much it holds and records it as the job's progress; once the node holds 100 % the job is `done` and the local copy is removed from every engine with its files. It survives a restart (the 6-hour deadline is stored), and ends `failed` with the local copy kept when the deadline passes or the node is removed. `DELETE /api/jobs/:id` stops it. A second move of the same torrent while one waits → 409; `then` other than `keep`/`remove` → 400. Up to 4.3.1 the wait was a task lost on restart, and a move to a node's extra engine (`vpn1`) was never confirmed.
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
| `/api/provenance` | GET | Where the library was imported from: `{"present","source_client","source_date","imported_count","carried_uploaded_bytes"}`. Written at the end of every qBittorrent or Transmission import (and retry) that added at least one torrent; several imports add up, `source_date` is the first one's, every client is named once (`"qBittorrent, Transmission"`). Up to 4.3.1 no 4.x import wrote it. | ✓ |
| `/api/dedup/config`, `/api/dedup/stats` | POST, GET | `{"enabled":true}` (restart to apply); duplicate-data counters. | ✓ |

**Import status** (`GET /api/import/qbit/status`, and each `events` frame) describes the latest import: `job_id`, `running`, `phase`, `total`, `done`, `seeded` (complete, data found), `downloading` (to check or download), `stopped` (added stopped), `skipped`, `failed`, `failures` (up to 200 `{"name","hash","error"}`), `retryable` (true when `POST /api/import/retry` has something to do), `finished`, `error`, `current`. A qBittorrent export is retried on a dropped connection, a 5xx or a 429 (4 attempts, with backoff), and the import logs in again if the session expires.

Import wizard: [Migrating from qBittorrent](https://github.com/Kheopsian/Hydranos/wiki/Migrating-from-qBittorrent). Magnets, watched folders, dedup: [Adding Torrents and Existing Data](https://github.com/Kheopsian/Hydranos/wiki/Adding-Torrents-and-Existing-Data).

## Network and IP filter

| Route | Method | Purpose | Status |
|---|---|---|---|
| `/api/network/{interfaces,engines}`, `/api/public-ip` | GET | Interfaces that are up; exit IP per engine (`?refresh=1`; each row: `agent` and `engine` = the engine id — 4.3 wrote `local` —, `role`, `bind_interface`, `listen_port` = the port held now, `exit_ip`, `exit_ip_v6`, `state`: `ok` = the outbound exit probe answered, `warn` = no answer yet through its interface, `bad` = no answer; it is not inbound reachability, see `/api/port-forward`); process exit IP. | ✓ |
| `/api/network/mode` | GET, POST | Network tab. GET: `mode` as saved in `[network] mode` (a file never saved by the tab: deduced from the keys), `fields` (incl. `announce_proxy`, `announce_ip`, `gluetun_port_engine` read from the file), `warnings`, `env_overrides` (`TYPHON_ANNOUNCE_PROXY`, credentials redacted). POST `{"mode","fields","extra_engines"}`: writes the chosen mode's keys and REMOVES the other modes' keys (`socks5_*`/`announce_proxy`/`announce_ip` outside `socks5` and `proxy_v2`, `*_proxy_v2` outside `proxy_v2`, `gluetun_*` outside `gluetun`) from race, hoard and every extra engine; answers `{"status","mode","restart_required","warnings"}`. `restart_required` is true only when a running engine was started with different network keys, or the kill switch would now block another set of engines. `warnings` includes the UDP-trackers-not-announced notice when an engine with a proxy has `enable_udp_trackers` on or holds a `udp://` tracker, and the DHT-off notice when an engine with a SOCKS5 proxy has `enable_dht` on (the engine turns its DHT off behind the proxy; PEX stays). GET also carries `wireguard` (`{"supported","reason"}`: whether managed WireGuard can work on this host, and the fix when not) and `engine_state` (per running engine: `bind_interface`, `dht_running`, `dht_note` — why the DHT is off, `offline` for an engine not on the network —, `port_pending` — announces held until a forwarded port is known —, `tunnel` or null), and `kill_switch` — the same report as `GET /api/network/egress`, so the tab shows each engine's verdict and the daemon's way out from one answer. Leaving `wireguard` also removes `wireguard_*` and takes the live tunnels down at once (`tunnels_down` in the answer): the engines pinned to them reach nobody until the restart. Unknown mode, SOCKS5 without host, PROXY-v2 without a port or with a non-IP trusted source, a port outside 1–65535 → 400; `wireguard` on a host that cannot bring a tunnel up (no `NET_ADMIN`, not Linux, no `ip`/`wg`) → 409 with the reason. | ✓ |
| `/api/network/wireguard` | GET | Managed WireGuard. `configs` (stored provider files: `name`, `address`, `endpoint`, `peer_public_key`, or `error`; never a private or preshared key), `directory` (`<data_dir>/wireguard`), `engines` (per local engine: `enabled`, `assignment` — `tunnel`, `direct` or `none` —, `config_file`, `provider`, `manual_port`, `port_forward`, `device` = `wg-<engine>`), `providers`, `supported` + `unsupported_reason`, `tunnels` (null before any tunnel; else per tunnel: `engine`, `device`, `provider`, `provider_label`, `config_file`, `created`, `present`, `up` — handshake under 180 s —, `handshake_age_seconds`, `endpoint`, `forwarded_port`, `port_forward`, `degraded` — IPv6 half dropped, with the fix —, `last_error`, `dns` — the file's `DNS =` servers, which the engine's tracker names are resolved by, through the tunnel —, `dns_leak` — true when the file has no `DNS =` line and tracker names go to the host's resolver). | ✓ |
| `/api/network/egress` | GET, POST | The kill switch and the daemon's own traffic (tracker lists, ipfilter lists, update check, webhooks, `.torrent` URLs). The kill switch follows the network mode: disarmed in `direct`, armed in every other; `[daemon] kill_switch = false` disarms it, `true` arms it even in `direct`. Armed, an engine with no tunnel, no interface and no proxy is **blocked** — never put on the network (no listener, dial, announce, DHT or LSD), `degraded` on `/health` — unless its section has `allow_direct = true` (in WireGuard mode: the block's "Direct" choice; an unassigned engine is blocked); in gluetun mode an engine pinned to another interface than the one holding the namespace's default route is blocked. GET: `mode`, `armed`, `armed_why`, `kill_switch` (as written: `true`, `false`, or null = deduced), `egress` (`auto`, `direct`, `proxy` or `engine:<id>`), `daemon` (`via` — one line —, `via_kind` — `direct`, `proxy`, `engine_tunnel`, `engine_proxy`, `engine_interface`, `engine_direct`, `gluetun` or `refused` —, `via_engine`, `route` — credentials removed —, `socks5_host`, `socks5_port`, `socks5_user`, `socks5_pass`, `bind_interface`, `direct`, `error` — why the daemon's requests are refused right now, or empty), `engines` (per local engine: `engine`, `state` — `covered`, `direct`, `blocked` or `uncovered` (would be blocked, kill switch disarmed) —, `covered`, `blocked`, `blocked_now` — what the RUNNING process did, null for an engine it does not run —, `allow_direct`, `how`, `gaps`, `line`), `all_engines_covered`, `blocked` (ids), `summary` (the startup log's lines: one per engine and one for the daemon), `not_covered` (what the kill switch never covers). `egress = auto`: direct in `direct` and `gluetun` modes; in `socks5`/`proxy_v2` the race engine's proxy, else the first engine's; in `wireguard` the race engine's tunnel, else the first tunnelled engine's; armed and nothing to borrow → refused, never direct. POST, every field optional, only what is sent is written: `egress` (`auto` removes the key; an unknown value or engine → 400), `socks5_host/port/user/pass` (`[proxy]`, removed when the host is empty), `bind_interface`, `kill_switch` (`true`/`false`, or null/`"auto"` to remove the key), `allow_direct` (`{"<engine>": bool}`, unknown engine → 400). Applies the daemon's route at once and answers the GET body plus `restart_required` (true when the set of blocked engines changes: that is decided at boot). A port outside 1–65535 with a host → 400. | ✓ |
| `/api/network/wireguard/configs` | POST | Store a provider `.conf`: multipart field `file` (its file name kept) or a raw body with `?name=x.conf`, 64 KiB at most. Parsed first (PrivateKey, Address and a `[Peer]` with PublicKey required), then written at 0600 in `<data_dir>/wireguard`; same name = replaced. Answers `{"status","name","address","endpoint","note"}`, never the keys. Bad name, not a `.conf`, unreadable file → 400. Accepted without `NET_ADMIN`: storing needs no privilege. | ✓ |
| `/api/network/wireguard/configs/:name` | DELETE | Remove a stored file → `{"removed": name}`; absent → 404; a name that is not a bare `.conf` → 400. An engine still assigned to it fails its tunnel at the next boot and reaches nobody until another file is chosen. | ✓ |
| `/api/network/wireguard/engines` | POST | `{"engines":[{"engine_id","assignment","config_file","provider","manual_port","port_forward"}]}`, `assignment` = `tunnel`, `direct` (the default route on purpose: `allow_direct = true`) or `none` (unassigned: the kill switch blocks the engine, said in `warnings`); without `assignment`, `enabled: true` is `tunnel` and anything else `direct`, as the old tick box meant. Writes `wireguard_config`, `wireguard_provider`, `wireguard_port`, `wireguard_port_forward`, plus `wireguard_enabled = true` for a tunnel or `allow_direct = true` for direct (the other removed; both removed for `none`). An unknown `assignment` → 400. A file saved before 4.4 with `wireguard_enabled = false` is read as `direct`; an engine with neither key (an `[[engine]]` added later) as `none`; with any tunnel on, sets `[network] mode = "wireguard"` and removes the other modes' keys. `port_forward`: empty = the provider's way, `manual`, `off`, `natpmp`. Answers `{"status","restart_required","warnings","note"}` (warnings: no port typed for a manual provider, no forwarding for this provider). Unknown engine, missing or unreadable file, unknown provider, two engines on one file, a port outside 0–65535 → 400; a tunnel on a host that cannot bring it up → 409 with the reason. The tunnels are built at boot, before the engines start. | ✓ |
| `/api/network/check` | POST | A leak test. Body `{"echo_url"}` optional (default `https://api.ipify.org/`). Answers `{"mode","results":[{"id","label","status":"ok"\|"warn"\|"fail","detail"}]}`: `default_route` — the address of the default route, no proxy, no interface (what a leak shows; not measured under the kill switch); `announce_<engine>` / `peer_egress_<engine>` for **every** local engine — the address trackers / peers see, each probe through the engine's own path (announce proxy, SOCKS5, interface), **`fail` with `LEAK:` when an engine set to leave by an interface or a proxy shows the default route's address**, `fail` when its path does not answer, `ok` naming the route otherwise (an engine with neither interface nor proxy: `ok`, "by the default route"); `host_ip` — the daemon's own requests (`/api/network/egress`), compared the same way; `inbound_<engine>` — from what happened, as in `/api/port-forward`: `ok` once a peer from outside connected, `fail` when nothing listens, `warn` "not proven" otherwise, with how the port is forwarded. An engine the kill switch blocks is not probed (`warn`, with the reason). Since 4.4 (4.3: four identical default-route requests, race and hoard only, inbound always "not tested"). | ✓ |
| `/api/port-forward` | GET | Per engine, what is true now: `{"engines":[{"engine","role","listening","listen_port"` (the port held, a live change included) `,"inbound_peers"` (peers from outside that connected since start) `,"reachable":"yes"\|"no"\|"unproven","forward":{...}}],"listen_healthy","all_connectable","ipv6_wanted","public_ip","public_ip_v6"}`. `forward.by`: `upnp` / `natpmp` (the home router: `internal_port`, `external_port`, `tcp`, `udp`, `udp_error`, `error`, `at`), `wireguard` (`method`, `external_port`, `error`), `gluetun` (`external_port`, `pending`), `none` (`refused`: why no mapping was asked, or `error` after both protocols failed), `off` (`auto_port_forward = false`), `pending` (no answer yet). Since 4.4 (4.3: constants, race and hoard only). | ✓ |
| `/api/ipfilter` | GET, PUT | Status / `{"enabled","sources":[...],"refresh_hours"}`. | ✓ |
| `/api/ipfilter/bans`, `/api/ipfilter/reload` | POST, DELETE | Ban `{"ip","reason"}` / unban `{"ip"}`; POST reload reloads block lists. | ✓ |

What works and what does not on the Network tab: [Networking](https://github.com/Kheopsian/Hydranos/wiki/Networking-Modes).

## Settings, setup, restart

| Route | Method | Purpose | Status |
|---|---|---|---|
| `/api/setup` | GET, POST | First run: `{"needs_setup","network_storage",...}` (`network_storage` is `"network share"` when `data_dir` is on NFS, SMB or FUSE, where the database cannot use WAL; `""` otherwise; always `""` up to 4.3.1) / create the admin `{"username","password"}` (8+ chars) → API key. Public, loopback or private network only. | ✓ |
| `/api/login` | POST | `{"username","password"}` → `{"api_key"}`. Public. | ✓ |
| `/api/auth/password` | POST | `{"password"}` (8+ chars): stored hashed in `[auth] password_hash`, as setup does. | ✓ |
| `/api/settings` | GET, POST | Whole config as JSON, **secrets included** / `{"changes":[{"section","key","value"}]}` edits existing keys only, except the live engine keys of `[race]`/`[hoard]` (`upload_rate_limit`, `download_rate_limit`, `peer_timeout`, `choking`, `max_uploads_per_torrent`), which are created when absent, and `enable_lsd`, created when absent too but applied at the next start. Those are put on the running engines at once; the answer carries `applied_live` (engines updated) and `restart_required` (false when every change is one of them). | ✓ |
| `/api/settings/reset` | POST | Reset to defaults (keeps login, key, data dir). | ✓ |
| `/api/settings/restart`, `/api/restart` | POST | Answer, then stop cleanly (the same path as SIGTERM: `stopped` to trackers, resume data flushed) and exit with code **75** for the supervisor to restart (systemd `Restart=on-failure` included). | ✓ |
| `/api/fs/browse?path=` | GET | List folders on the Hydranos host. | ✓ |
| `/api/update-check` | GET | New release available. | ✓ |

A forgotten password is reset on the host with `hydranos reset-password <password> [config]`: [Security and Access](https://github.com/Kheopsian/Hydranos/wiki/Security-and-Access).

## Monitoring

| Route | Method | Purpose | Status |
|---|---|---|---|
| `/health` | GET | Public. `{"status","version","uptime","checks":{"store","engines":[{id,torrents,online,listening,held,blocked}]},"problems":[…]}`. `healthy` (200); `degraded` (200: an online engine without a listener, held by `start_paused`, or blocked by the kill switch — `blocked` carries the reason —, even when every engine is); `unhealthy` (**503**: the store does not answer a query, or no online engine listens); `starting` (200, with `startup`) while the catalogue loads. | ✓ |
| `/metrics` | GET | Public, Prometheus with `# HELP`/`# TYPE`, per engine (extra engines included): torrents, by state, paused, rates, session bytes (counters), lifetime bytes, peers, seed size, announces ok/failed (engine and tracker), tracker errors last hour by class, announce in flight / slots / late / needed, rate caps (engine and client), choker, listening, held, jemalloc allocated/resident, build info, uptime. From kept counters: no catalogue walk per scrape. While starting: `hydra_up`, `hydra_starting`, `hydra_startup_restored{engine}`, `hydra_startup_records{engine}`. | ✓ |
| `/api/startup` | GET | Public. `{"ready","phase","total","restored","engines":[{id,restored,total}],"uptime"}`. The port opens before the catalogue loads; `phase` is `loading`, `connecting`, `opening_store`, `starting_workers`, then `ready`. While not ready, every other route except `/`, `/static/*`, `/health`, `/metrics` and `GET /api/setup` answers 503 `{"error","startup"}` with `Retry-After: 5`. | ✓ |
| `/api/status` | GET | Header and Overview figures. | ✓ |
| `/api/events` | GET | SSE stream of status snapshots. Ends itself when the daemon stops. | ✓ |
| `/api/logs` | GET | The in-memory ring (last 2000 lines). Filters, server side: `level` (minimum: `ERROR`, `WARN`, `INFO`, `DEBUG`, `TRACE`), `module` (substring of the module path), `q` (substring of the message and its fields), `since` (`5m`, `1h`, `24h`, `2d` or Unix s), `after` (a `seq`), `limit` (newest N). An unreadable filter → 400. `{"entries":[{ts,source,level,module,msg,seq}],"last_seq","modules":[…]}`. | ✓ |
| `/api/logs/stream` | GET | SSE, events named `log`, `id` = `seq`; new lines only, after `after=` or `Last-Event-ID`; same `level`/`module`/`q` filters. Ends itself when the daemon stops. | ✓ |
| `/api/stats/baseline` | GET, POST | Lifetime counter baseline `{"total_uploaded","total_downloaded"}`. | ✓ |
| `/api/health/anomalies` | GET | `scan` (`pending` until the first background pass, 2 min after start, then every 5 min), `generated_at`, `scan_duration_ms`, `scanned{engine}`, `counts` (`files_missing`, `ghost`, `fake_seed`, `starved`, `redl`, `dual_seed`, `trackers_failing`, `announces_late`, `engines_held`, `disks_low`), `anomalies` (≤ 500, `anomalies_truncated`), `wasted_bytes`, `efficiency`, `redl_historical(_bytes)`, `trackers` (red/amber hosts and their engines), `announces`, `disks` (free space per filesystem holding data), `engines{id:{torrents,states}}`. Removed in 4.4: the constant fields `errors`, `persistent_counters`, `ghost_files`, `orphan_files`, `goroutines`, `gc_cpu_pct`. | ✓ |
| `/api/benchmark/{records,range,current,race-events}`, `.../trackers/{current,range}` | GET | Benchmark data, tracker stats. | ✓ |
| `/api/benchmark/engines` | GET | `start`/`end`: the engines beyond `race` and `hoard`, `{engine:[{ts,upload_rate,download_rate,peers,uploading,torrents}]}`, averaged into ≤ ~300 points. | ✓ |

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

**`/api/benchmark/current`**: `ts`, `global_uploaded`/`_downloaded` (as `baseline.global_*`), `hoard_upload_rate`, `hoard_peers`, `hoard_with_peers`, `hoard_uploading`, `race_upload_rate`, `race_download_rate`, `race_uploading`, `race_torrents`, `race_session_uploaded` (all engines), `iowait_pct`, `open_fds`, `arc_*` (host ZFS ARC), `race_peers` (connected peers; the race torrent count until 4.4), `engines` (every engine: `id`, `role`, `upload_rate`, `download_rate`, `peers`, `uploading`, `torrents`). Always 0: `*_announce_rate`, `*_announce_fail_rate`, `race_avg_share`, `hoard_session_uploaded`. `bench_samples` also records `hoard_download_rate` and the `extra_*` sums of the other engines, which the Records count.

**`/api/benchmark/trackers/current`**: one row per engine and tracker host: `engine`, `tracker`, `torrents`, `active` (with a peer), `peers`, `upload_rate`, `download_rate`, `cum_uploaded`/`cum_downloaded` (lifetime), `seed_size`, `ts`. `range` routes take `start`/`end` (Unix s, default last 24 h); `trackers/range` also `tracker=`.

What each monitoring route reports: [Monitoring and Logs](https://github.com/Kheopsian/Hydranos/wiki/Monitoring-and-Logs).

## qBittorrent shim

Documented on [qBittorrent Shim and Automation](https://github.com/Kheopsian/Hydranos/wiki/qBittorrent-Shim-and-Automation). Changes in 4.3.1:

- `torrents/files`, `torrents/properties` and `torrents/trackers` read `hash` from a POST form body as well as the query string (cross-seed sends every call as a POST). An unknown hash on `torrents/trackers` is a 404. Its `** [LSD] **` row is real: `status` 0 with `msg` "LSD is off for this engine" or "This torrent is private", else 2 with `num_peers` = LAN peers Local Service Discovery found for the torrent since the engine started.
- `torrents/categories` and `torrents/tags` answer POST as well as GET.
- `torrents/export` serves the stored `.torrent` of `hash`.
- `torrents/info` lists every engine, each torrent once, with `tracker` filled; `hashes=all` means every torrent on the write endpoints; `torrents/files` gives real piece ranges and progress, with the torrent's folder in multi-file names.

Changes in 4.4:

- **Real transfer figures.** `torrents/info` reports the real `uploaded`, `downloaded`, `ratio` and `seeding_time`; `torrents/properties` the real `total_uploaded`, `total_downloaded`, `share_ratio` and `seeding_time`, computed the same way (a finished torrent with nothing downloaded counts its size as downloaded, so a cross-seed's ratio is upload / size). Up to 4.3.1 every one of them was 0, for every torrent: *arr seeding goals and autobrr ratio rules now see the truth.
- **Speed limits**, in bytes/s as qBittorrent has them. Parameters in the query string or the form body.
  - `POST torrents/setUploadLimit`, `torrents/setDownloadLimit`: `hashes` (`|`-separated, or `all`) and `limit` (0 or negative = no limit). Every local copy of each hash. Persisted with the torrent.
  - `torrents/uploadLimit`, `torrents/downloadLimit` (any method): `{"<hash>": <bytes/s>}`, 0 = none.
  - `POST transfer/setUploadLimit`, `transfer/setDownloadLimit`: `limit`. The global cap: one bucket above every engine of this host, persisted in the store. `transfer/uploadLimit`, `transfer/downloadLimit` (any method) answer it as a plain number, 0 = none.
  - `torrents/add` takes `upLimit` / `dlLimit` (bytes/s) for a `.torrent` file; a magnet has no torrent to cap yet and ignores them.
  - `up_limit` / `dl_limit` in `torrents/info` and `torrents/properties` are the torrent's own caps, -1 = none; `up_rate_limit` / `dl_rate_limit` in `transfer/info` and `up_limit` / `dl_limit` in `app/preferences` are the global cap, 0 = none.
- **Share limits**, as qBittorrent has them: a ratio and two times in **minutes**. Per torrent -2 = follow the engine, -1 = no limit. All off by default: a config without the keys reports `max_ratio_enabled: false` and every torrent `ratio_limit: -2`.
  - `POST torrents/setShareLimits`: `hashes` (`|`-separated, or `all`), `ratioLimit` and `seedingTimeLimit` (both required, 400 otherwise), `inactiveSeedingTimeLimit` (optional, -2 when absent). Query string or form body. Stored per torrent (every copy), in the store.
  - `torrents/info` and `torrents/properties` carry `ratio_limit`, `seeding_time_limit`, `inactive_seeding_time_limit` (the torrent's own) and `max_ratio`, `max_seeding_time`, `max_inactive_seeding_time` (in force, -1 = none).
  - `torrents/add` takes `ratioLimit`, `seedingTimeLimit`, `inactiveSeedingTimeLimit`, for a `.torrent` file and a magnet alike (Sonarr and Radarr send their indexer's seed goal there).
  - `app/preferences` answers `max_ratio_enabled`, `max_ratio`, `max_seeding_time_enabled`, `max_seeding_time`, `max_inactive_seeding_time_enabled`, `max_inactive_seeding_time` and `max_ratio_act` (0 stop, 1 remove, 3 remove with the files) for the race engine (the first engine when there is none). `app/setPreferences` writes them to **every** local engine, the qBittorrent way: a value is taken only with its `*_enabled` flag, `false` switches it off, `max_ratio_act` 2 (super seeding) is ignored. A document that changes nothing (a client posting back the page it read) writes nothing.
  - `lsd` is whether that same engine runs Local Service Discovery (`enable_lsd`, absent = on for race, off for hoard). `app/setPreferences` with an `lsd` that differs from it writes `enable_lsd` to every local engine, applied at the next start; an `lsd` equal to it writes nothing.
  - `queueing_enabled`, and under it `max_active_uploads` / `max_active_torrents`, are the race engine's `queueing`, `active_seeds` and `active_limit`.
- **`app/setPreferences` applies more** (each written only when it differs from what `app/preferences` shows, so posting back the page changes nothing): `up_limit` / `dl_limit` (the global caps, bytes/s, live and kept, as `transfer/setUploadLimit`); `dht`, `pex` (`enable_dht` / `enable_pex` of every local engine, at the next start); `max_active_downloads` (`active_downloads` of every local engine, live); under queueing only, `max_active_uploads` / `max_active_torrents` (`active_seeds` / `active_limit`; queueing itself is switched in Hydranos, not by a client); `save_path` (absolute, else 400 and nothing written): where a shim add with neither a category with a folder nor a `savepath` lands, also answered by `app/defaultSavePath`. **Every other key answers 200 and changes nothing** (`listen_port`, `max_connec`, `max_uploads_per_torrent`, `encryption`, `create_subfolder_enabled`, `queueing_enabled`, schedulers, web UI and RSS settings…); they are named once in the log at info level.
- **`app/preferences`**: `max_active_downloads` is the race engine's `active_downloads` (-1 = none; was a constant 20), `save_path` the folder above (empty until a client sets one; was a constant `/downloads` nothing wrote to).
- **`torrents/properties`**: `creation_date`, `created_by`, `comment` from the `.torrent` (-1 / empty when it has none; `creation_date` used to repeat the addition date); `nb_connections` and `peers` = connected peers (seeds and leechers are not told apart once connected, so `seeds` stays 0); `peers_total` / `seeds_total` = the tracker's leechers and seeders; `eta` as in `torrents/info`; `dl_speed_avg` / `up_speed_avg` = lifetime bytes / time since added; `last_seen` = now when this copy is complete or the tracker counts a seed, else -1. `total_wasted` is not measured (0).
- **Routes that answered 404:**
  - `POST torrents/setLocation` (`hashes`, `location`): moves every copy's data as **Set location…** does, a job per copy on the Jobs tab. 400 without `location`; 409 with the reason when nothing could be queued (relative path, another torrent reads the files…).
  - `POST torrents/setForceStart` (`hashes`, `value`): `true` starts the torrents where no queue applies (it is then exactly a start); 409 and nothing started when an engine holding one runs a queue (`active_downloads`, or queueing): Hydranos's queue has no "forced" exemption and would stop it again. `false` → 200, nothing is ever forced.
  - `POST torrents/filePrio` (`hash`, `id` `|`-separated, `priority`): the engine downloads every file of a torrent. Priority `1`, `6`, `7` → 200 (that is what happens); `0` (skip) → 409; another priority → 400; an unknown file id → 409; unknown hash → 404.
  - `POST torrents/rename` → 409: the name shown is the `.torrent`'s, there is no display name to change.
  - `POST torrents/{topPrio,bottomPrio,increasePrio,decreasePrio}` → 409, `Torrent queueing is not enabled` (qBittorrent's answer) with queueing off; with it on, 409 too: the queue orders by age, positions cannot be set.
  - `sync/maindata` (any method): always the full state, `full_update: true`, `rid` increasing on every call; `torrents` keyed by hash (rows of `torrents/info`), `categories`, `tags`, `server_state` (the `transfer/info` figures, `queueing`, `refresh_interval`); `trackers` empty. No delta is computed: a full answer is valid at any `rid`.
  - `torrents/count`, `app/defaultSavePath` (any method): plain text.
  - Still 404: `sync/torrentPeers`, `torrents/{setSuperSeeding,setAutoManagement,toggleSequentialDownload,toggleFirstLastPiecePrio,renameFile,renameFolder,editTracker,pieceStates,pieceHashes,webseeds}`, `app/shutdown`, `transfer/banPeers`, speed-limit mode, `log/*`, `search/*`, `rss/*`.
- **`torrents/add` fields** (4.4): `contentLayout` `Subfolder` gives a single-file torrent a folder named after it (as `create_subfolder`), `Original` none; absent = `[daemon] create_torrent_folder`; `NoSubfolder` on a **multi-file** torrent is refused (`Fails.` when it was the only one): the engine always writes it under its name, and a client linking from the stripped layout would find nothing. The pre-4.3.2 `root_folder` (`true`/`false`) means the same. `autoTMM=true` with a category that has a folder: that folder, the `savepath` sent alongside is ignored (qBittorrent's managed mode). `stopCondition` `MetadataReceived` / `FilesChecked`: added stopped (the data is still checked). `skip_checking` is honoured for files and magnets. Accepted without effect, named in the log: `rename`, `sequentialDownload`, `firstLastPiecePrio`, `cookie`. On a magnet, the layout fields have no effect (the files are not known yet).
- **`app/buildInfo`** keeps answering qBittorrent's version numbers (`qt`, `libtorrent`, `boost`, `openssl`, `bitness`): clients test them to pick an API dialect, and Hydranos's own versions would make them choose wrongly. Hydranos's version is `GET /api/status` `version`.
  - The worker acts on seeds only, every 2 minutes: never on a download, a check or a torrent the operator stopped, and **never before the tracker's minimum seeding time** (`announce_min_seed_hours`): a seed that still owes it, or whose tracker declares none, is left seeding whatever the limit says. A stop is the operator's kind (`stoppedUP`, not restarted by the queue). A removal takes the API's path: `stopped` to the trackers, files only with the last copy for `remove_with_files`.

## Routes that are stubs in 4.3.1

These routes answer but do not do what their name says.

| Route | What it answers | Use instead |
|---|---|---|
| `GET /api/hoard/download-slots` | The configured `active_downloads` and zeros. | `active_downloads` in the config, read live since 4.4 ([Configuration: Engines and Race Drain](https://github.com/Kheopsian/Hydranos/wiki/Configuration-Engines)). |
| `GET /api/arr-cleanup/scan` | An empty scan. | — |
| `/api/vpn-speedtest/*`, `/api/benchmark/compare`, `/api/benchmark/race-snapshots/:info_hash` | Constants, empty lists, or a fixed 400. | — |

Real since 4.4: `GET /api/port-forward` (was constants), the live listen-port and dial-limit changes (were lost at restart, uTP stayed on the old port), `POST /api/network/check` (could not see a leak).

**`bind_interface` is Linux-only since 4.4.** Windows has no way to pin a socket to an interface by name, and on macOS the TCP peer sockets cannot be pinned. A non-empty `bind_interface` / `*_bind_interface` is refused with 400 and the reason by `POST /api/network/mode` and `POST /api/engines`; `GET /api/network/mode` carries `bind_interface: {"supported","reason"}` and the Network tab hides the fields there. A file that still sets it starts with a warning naming the engine, and that engine stays off the network: no listener, no peer dial, no announce, no DHT, LSD or uTP.

## Routes removed in 4.4

These were stubs up to 4.3.1: they returned a success-shaped answer and did nothing. No screen called them. They are gone now. A removed path returns `404`. A removed method on a path that keeps its `GET` returns `405`.

| Route | Up to 4.3.1 | Since 4.4 | Use instead |
|---|---|---|---|
| `POST\|DELETE /api/hoard/download-slots` | Echoed the GET, changed nothing. | 405 | `active_downloads` in the config, read live since 4.4. |
| `POST /api/race/settings` | Echoed the config, ignored the body. | 405 | `POST /api/settings`, then restart. |
| `GET\|POST /api/opt/flags` | 3.x constants; POST always `400 unknown flag`. | 404 | — |
| `GET /api/race/choking` | Always `null`. | 404 | — |
| `POST /api/hoard/verify-downloading` | `{"verified":0}`, did nothing. | 404 | `POST /api/selection/recheck`. |
| `POST /api/hoard/restart-stuck` | `{"restarted":0}`, did nothing. | 404 | `stop` then `start` through `/api/selection/*`. |
| `POST /api/arr-cleanup/execute` | `{"errors":null,"removed":0}`. | 404 | — |
| `/api/agents`, `/api/agents/*` (every method) | See *Nodes* above. | 410, body names `/api/engines` and `/api/nodes` | `/api/engines`, `/api/nodes`. |
| `POST /api/jobs/move-remote` | Always 400. | 410, body names `/api/selection/handoff` | `POST /api/selection/handoff`. |
