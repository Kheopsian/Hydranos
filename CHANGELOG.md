# Changelog

All notable changes to Hydra are documented here. This project follows
[semantic versioning](https://semver.org).

## Unreleased

### Added

- **PUID/PGID support in the container.** Set `PUID` and `PGID` and Hydra drops
  privileges at start, so the config, the resume data and the files it writes
  belong to your user instead of root -- which is what Sonarr, Radarr and
  friends need to hardlink them. Leaving both unset keeps the previous
  behaviour: everything runs as root. The config directory is chowned on start
  (skip it with `HYDRA_SKIP_CHOWN=1`); the payload directory is never touched,
  its permissions stay yours to manage. If you route through a VPN with an
  fwmark, add `HYDRA_CAP_NET_ADMIN=1` and `--cap-add=NET_ADMIN` so `SO_MARK`
  survives the privilege drop.

## v3.46.0 — 2026-08-07

### Added

- **Thread-per-core session pinning, behind a hot flag, off by default.** Pins
  each peer session to one single-threaded runtime so its socket is never
  contended; toggle with `session_pinning` on `POST /api/opt/flags`. Only new
  sessions follow a flip, so it lands over minutes as peers churn.

## v3.45.0 — 2026-08-07

### Added

- **See what is inside a .torrent before adding it.** The Add screen reads the
  files you pick and lists their contents beside the form, parsed in your
  browser — nothing is uploaded until you press Add. A checkbox turns it off.

- **A Content tab in the torrent detail panel**, on both race and hoard: every
  file with its size and share of the total, plus swarm piece availability where
  it exists. Seeding torrents carry no piece map, so there it reads n/a.

## v3.44.2 — 2026-08-06

### Fixed

- **A deleted category no longer comes back after a restart.** Deleting a
  category removed it from the category list and cleared its label from the
  torrents that carried it — in memory only. The label also lives in the store,
  which is what the daemon reloads from at boot, so every one of those torrents
  came back wearing a category the user had deleted. It looked like the deletion
  had been ignored, when in fact it had worked and then been undone by the next
  boot. The sidebar chips are built from the torrents' own labels rather than
  from the category list, which is why the ghosts were so visible. The label is
  now cleared in the store as part of the deletion, in one statement rather than
  a write per torrent — at a hundred thousand torrents that is a single scan
  instead of as many transactions. The race engine gained the in-memory clear
  the hoard engine already had: until now a race torrent kept its label even
  before a restart.

  Two consequences worth stating. Labels stranded by a deletion made *before*
  this fix are not cleared by upgrading — they are still on the torrents, and
  the delete route used to refuse them with a 404 because the category was no
  longer in the list, which left them unreachable. That route now accepts an
  orphaned label and clears it; 404 is reserved for a name nothing carries at
  all. `GET /api/categories/orphans` lists those labels, and the categories
  screen shows them with a delete button, since that screen holds the only one.
  They are deliberately kept out of the add-torrent dropdown: a category that no
  longer exists is not something to assign.

  Closes #7.

## v3.44.1 — 2026-08-06

### Fixed

- **A tracker override set from the interface now survives a restart.** The
  tracker editor writes a client spoof, a passkey or a secondary-stats mode
  straight into the announce registry, where it takes effect on the very next
  announce. Nothing ever wrote it down. At boot that registry is rebuilt from
  the config file alone, so every override placed through the UI was silently
  gone the moment the daemon came back, and the only way to keep one was to
  hand-edit `default.toml` — which is why anyone who had done that never saw
  the bug. The three routes now mirror what they set into `[announce_clients]`,
  `[announce_passkeys]` and `[announce_secondary_stats]`, reusing the settings
  editor's machinery: the file is edited line by line so comments, ordering and
  every unrelated line survive, the result must still parse *and* still decode
  into the typed config before it is committed, and the write goes through a
  backup and an atomic rename. Persisting is deliberately not allowed to fail
  the request — the hot change has already applied, so a read-only config file
  yields `"persisted": false` and a warning rather than an override that looks
  rejected while being live. Clearing an override removes it from the file, and
  takes the enclosing table with it once it holds nothing else. One gap remains,
  and it is worth stating: overrides are pushed to remote agents when they are
  set, but an agent that restarts does not get them back, because the control
  plane does not re-push at boot. Single-node installs never meet this.

  Closes #3.

## v3.44.0 — 2026-08-06

### Changed

- **Four control-plane optimisations are now on by default**, measured at
  −41% CPU on the Go process with resident memory unchanged. Each one stays
  switchable at runtime through `/api/opt/flags`.
- **`list_cache` fires at all now.** Its lifetime was a three-second constant
  while the schedulers sharing it tick at ten seconds and above, so every
  caller always found it expired. It is adjustable now and defaults to nine.

### Notes

- `gogc` stays a knob at the Go default of 100: raising it bought 6.6 points of
  CPU for three extra gigabytes of resident memory.

## v3.43.2 — 2026-08-06

### Added

- **Five runtime knobs for profiling the control plane, all off by default**:
  `ipc_prealloc`, `qbit_snapshot`, `totals_cache`, `list_cache_ttl_ms` and
  `gogc`, switchable through `/api/opt/flags`. They ship off so each can be
  measured against the running system; nothing changes until one is turned on.

## v3.43.1 — 2026-08-06

### Fixed

- **Per-peer transfer rates are no longer reported as zero.** The peer panel
  showed every connection at 0 B/s no matter what it was doing, on torrents
  visibly moving hundreds of megabytes. Nothing was wrong with the display or
  with the plumbing between the engine and the control plane: the engine had
  simply never filled the field, emitting a literal `0` for both directions next
  to a TODO. The cumulative byte counters per peer already existed, so only the
  derivative was missing. Rates are now sampled inside the `get_peers` call
  rather than from a background tick — at a hundred thousand torrents, sweeping
  every connected peer on a timer would cost O(all peers) forever to feed a
  panel that is almost never open, whereas this call only happens while someone
  is actually looking. The delta therefore lands over the caller's own polling
  interval. One consequence worth knowing: the first reading for a freshly
  connected peer is still 0, by design. The rate tracker seeds its reference
  point before it will emit anything, which is what stops a peer restored with a
  resumed byte count from reporting a single absurd spike.

## v3.43.0 — 2026-08-06

### Changed

- **The control plane no longer parses every engine reply up to four times.**
  A CPU profile of a 107k-torrent instance showed the Go side burning roughly a
  third of its time in the IPC read loop, on a listing it had already read: each
  frame from the engine was decoded once to test whether it was an event, once
  more for its id, again for its error field, and only then by the caller that
  actually wanted the data. On a `list_torrents` frame — about 100 MB at this
  scale — that is three full JSON walks thrown away per reply. Routing now takes
  a single header-only decode. Measured over 600-second windows on a live
  instance, the Go process drops from 88-96% of a core to about 75%; the
  drift between two identical control windows was around 9%, so the win is well
  clear of the noise but should be read as a range, not a figure.
- **Frame assembly stopped re-scanning what it had already scanned.** Reading a
  frame walked the entire accumulated buffer again on every refill, the way
  `bufio.Scanner` does internally — quadratic in frame size, which is exactly
  the wrong shape for a 100 MB listing. Frames are now assembled in one pass.
  This one deletes a measurable ~27 seconds of rescanning per 600-second window,
  but its effect on total CPU sits below the noise floor: it ships because it
  cannot cost anything, not because the total moved.

### Added

- **`GET`/`POST /api/opt/flags` toggles each IPC optimisation at runtime.**
  Turning a flag off restores the previous code path exactly, which makes it a
  faster rollback than any restart — and restarts are not free here, since they
  reset the per-torrent upload counters that trackers credit by maximum. The
  same switch is what let each optimisation be measured in isolation against a
  real baseline rather than a remembered one. `ipc_frame` and `ipc_route` ship
  on; `list_cache` ships off, because a 3-second shared snapshot never survives
  callers that tick 10 seconds apart — sharing that snapshot deliberately is a
  different change.

### Fixed

- **The profiler had been dead in production for two weeks without saying so.**
  pprof bound to `127.0.0.1:6060`, but Hydra runs with host networking and
  CrowdSec already holds 6060 there, so the bind failed and the error was logged
  at warning level and never seen — while `curl` against the port answered 200,
  because CrowdSec was answering. It now defaults to 6061, honours
  `HYDRA_PPROF_ADDR`, logs the address it listens on, and reports a failed bind
  as an error.

## v3.42.2 — 2026-08-04

### Fixed

- **An empty torrent listing is now an empty array, not `null`.** The qBittorrent-compatible `/api/v2/torrents/info` handed out a JSON `null` whenever it had nothing to report, where real qBittorrent always answers with an array. Clients dereference that body directly - cross-seed calls `.find()` on it - so the null threw a `TypeError` in the client instead of reading as "no torrents". The window is narrow but real: during boot, before the engines have restored their state, the default `filter=all` skips every filter and the listing is genuinely empty. Requests that filtered by hash were already safe; only the unfiltered listing could go null.

## v3.42.1 — 2026-08-04

### Fixed
- **A deleted torrent came back after a restart.** Removing a torrent dropped it
  from the engine immediately but left its row in the store until the next
  five-minute sync, and booting reloads from the store — so a restart inside
  that window resurrected it, tags, category and all. The row is now dropped in
  the same transaction that carries the torrent's lifetime bytes away, at the
  moment of removal. (Issue #13, reproduced on a test instance: delete, restart
  within five minutes, torrent back.)
- **The torrent list never exposed the pause intent.** `/api/hoard/torrents`
  builds each row field by field and had no `user_paused`, so the UI could not
  tell a torrent you paused from one a scheduler is holding — the same omission
  that hid `tracker_host` in v3.29.3.
- **Tags and pause set on a freshly added torrent were lost on restart.** Both
  are written with an UPDATE, which quietly touches nothing while the torrent's
  row does not exist yet — and the row only appears at the next five-minute
  sync. Both now ride along with that sync, so the state lands whichever path
  gets there first.
- **A restored pause did not show as paused until the next stats refresh**, as
  the intent was not projected into the stats cache the way tags and category
  already were.

## v3.42.0 — 2026-08-04

### Added
- **Manual pause, and it stays paused.** Torrents can be paused and resumed
  from the right-click menu, from the native API, and through the qBittorrent
  shim — whose pause and resume verbs had been acknowledging requests without
  doing anything. What gets recorded is the *intent*, on the torrent's own row
  in the store, so it survives a restart. This is deliberately not the same
  thing as being stopped: the download and disk slot managers stop and start
  torrents constantly, and a torrent they are holding back now reads as
  **queued** while one you paused reads as **paused**. The schedulers read the
  intent and never write it, so freeing a slot can no longer quietly undo your
  decision. Pausing everything persists too — a restart after "pause all" would
  otherwise resume the whole library.

### Changed
- **The database schema can be upgraded now.** The table definitions were
  `CREATE TABLE IF NOT EXISTS`, which meant a new column would silently only
  ever reach people who started from an empty database. Schema changes are now
  ordered migrations recorded in `PRAGMA user_version`, each applied in one
  transaction with its own version bump. Opening a database written by a newer
  Hydra fails with an explanation instead of running against a shape it does
  not understand.
- **Tags and the lifetime counters moved out of their JSON sidecars** and onto
  the torrent row and a counters table. `tags.json`, `tags_registry.json`,
  `baseline.json` and `baseline_trackers.json` are imported once at startup and
  renamed `.migrated` — kept, never deleted. A node with no database of its own
  (front-only) still uses the files.

### Fixed
- **Removing a torrent can no longer lose or double-count its lifetime bytes.**
  Folding a removed torrent's upload into the carry-over totals and deleting its
  row were two writes to two different files with no transaction spanning them,
  so a crash in between either counted the torrent twice on the next boot or
  dropped its bytes permanently — and lifetime upload cannot be recomputed from
  anything else. Both now happen in a single commit.
- **A torrent waiting for a slot no longer reports itself as stalled to the
  \*arr apps.** The state a stopped torrent actually carries had no case in the
  qBittorrent state mapping and fell through to `stalledDL`/`stalledUP`, which
  is what Sonarr treats as a broken download. It now reports `queuedDL`/
  `queuedUP`, and a genuinely user-paused torrent reports `pausedDL`/`pausedUP`.

## v3.41.3 — 2026-08-04

### Fixed
- **Two blank rows in the race policy header.** The drain-result and warning
  messages each lived inside a permanent row wrapper, so while the message was
  hidden the wrapper still contributed its padding and separator line — the
  panel showed two empty stripes between the disk gauge and the policy toggles.
  The message elements are now the rows themselves, so hiding a message hides
  its stripe.

## v3.41.2 — 2026-08-04

### Fixed
- **The new per-tick eviction cap could hold the pool far above its slot limit.** The cap reprieves evicted torrents, and was meant to pay for each reprieve by dropping an incoming one — but when there were no incoming torrents it reprieved them for free. Right after boot that is exactly the situation: stagger start has every incomplete torrent running and nothing is waiting to come in, so the pool stayed near its full size and shed only a few slots per tick. Observed on the v3.41.1 deploy: 19976 active slots against a 2000 limit. A reprieve is now strictly a swap, so the pool can never exceed its limit and converges on the first tick.

## v3.41.1 — 2026-08-04

### Fixed
- **The download slot manager was pausing healthy downloads, constantly.** With more incomplete torrents than slots, the pool was rebuilt from scratch on every 30s tick and ranked purely on tracker-scrape seed counts — whether a torrent was *currently downloading* carried no weight at all. Two things then made that ranking unstable: the sort was unstable and most of the pool is tied at zero scrape seeds, so the "top N" was effectively redrawn each tick; and the probe quota (a fifth of the slots) rotated on last-attempt time, which taking a slot immediately updated, so a probe slot lasted exactly one tick. Observed in production on a 2000-slot pool over 20k incomplete torrents: up to 1800 slots swapped per tick. Stop/start drops every peer connection, so nothing could stay connected long enough to download, and the progress check then demoted the survivors with escalating cooldowns up to an hour — for a stall the manager had caused itself. A torrent that holds a slot and is making progress now keeps it, ranking runs only for the remaining slots, ties break on info_hash, and a new slot is held for a minimum of 5 minutes before it can be rotated. Rank-driven eviction is additionally capped at 5% of the pool per tick as a backstop.
- **The progress check could demote a genuinely downloading torrent.** It compared completed bytes, which the engine quantises to whole pieces, so a download spread across several large partial pieces could report no progress over an entire window. Live download rate is now accepted as evidence of progress.

## v3.41.0 — 2026-08-04

### Changed
- **In the age/ratio policy, a threshold of 0 now means "no constraint", not "off".** "Older than 0h" and "ratio at least 0" are true of every torrent, so leaving both at 0 makes the policy match everything past the keep floor — which is what the fields read like. The toggle is the on/off switch; the thresholds no longer double as one. The keep floor (`min_age_minutes`) is unchanged and remains the only guard.
- **"Drain now" is no longer greyed out when both thresholds are 0**, since that is now a working policy rather than a dead one.

### Added
- **A warning when an unconstrained policy is set to Delete.** Both thresholds at 0 with the Delete action erases every race torrent past the keep floor. The panel says so, and "Drain now" asks for confirmation before running it. Move → hoard is unaffected.

## v3.40.2 — 2026-08-03

### Fixed
- **"Drain now" looked dead when only the age/ratio policy was on.** The button did run, but the result was thrown away: the check applied the age/ratio policy first and then returned nothing whenever the emergency drain was disabled, so the API always answered `no_drain_needed` — even right after graduating torrents. The age/ratio outcome is now what the check returns in that case.
- **The panel never showed what a drain did.** The button swallowed both the response and any error, so "moved 3 torrents", "nothing matched" and "request failed" all looked identical: nothing. It now prints a one-line result, including the failure message.
- **A policy with no trigger now says so.** Age/ratio enabled with both `max_age_hours` and `min_ratio` at 0 can never match a torrent. The button is greyed out with an explanation instead of appearing armed, and the API reports `no_threshold`.
- **Torrents that match but cannot graduate are no longer silent.** A race category with no hoard category linked made the mover skip the torrent without a word. The result now counts them separately, so a configuration gap is not read as a quiet success.

## v3.40.1 — 2026-08-03

### Fixed
- **Graduation tooltip no longer assumes ZFS/NVMe.** It described the move as "copied to ZFS, removed from the NVMe"; reworded to the hoard category's storage and the race disk, since deployments use all kinds of filesystems.

## v3.40.0 — 2026-08-03

### Added
- **Live progress for race\u2192hoard graduation copies.** While a torrent is being moved to the hoard, the race policy panel shows a progress bar (bytes copied / total) per in-flight graduation, backed by a new /api/drain/graduations endpoint. A torrent already being graduated is never picked twice.

## v3.39.2 — 2026-08-03

### Added
- **The categories table shows each category's graduation target.** A new "Graduate to" column displays the linked hoard category (or a dash), so the race\u2192hoard routing is visible at a glance without opening the editor.

## v3.39.1 — 2026-08-03

### Changed
- **Drain now and the watermark drain now respect the Emergency-drain toggle.** The manual Drain-now button is greyed out unless a policy is enabled, and the watermark cleanup no longer runs (even manually) when Emergency drain is off — so a disabled policy truly deletes nothing.

## v3.39.0 — 2026-08-03

### Changed
- **Race policy panel redesigned into two clear, self-contained policies.** Each policy (Emergency drain; Handle old races) is now one labelled line with its own on/off toggle and an info (i) tooltip explaining exactly what it does. "Handle old races" gets an explicit enable switch (age_ratio_enabled) instead of being implicitly on when a threshold was set.
- **Dropped the hard 507 add-guard.** A new race add on a full NVMe now just triggers a background emergency drain and proceeds — missing a grab was worse than a transient disk-full the drain resolves.

## v3.38.1 — 2026-08-03

### Fixed
- **Editing a category now saves its graduation link.** The category update handler merged fields individually and never copied graduate_to, so adding or changing a race category's linked hoard category was silently dropped (creating a new category already worked).

## v3.38.0 — 2026-08-03

### Added
- **Graduation: move a matched race torrent to the hoard (race→hoard).** The age/ratio trigger gains an action selector — Delete (default) or Move → hoard. On Move, a torrent whose race category is linked (graduate_to) to a hoard category is copied NVMe→ZFS, verified, registered in the hoard in seed mode, then removed from race without re-deleting (global totals preserved). The hoard announces fresh, so there is no tracker over-credit. No link → the torrent is left in place, never deleted. Default off (action delete, and no links by default).

## v3.37.2 — 2026-08-03

### Fixed
- **qBittorrent shim reported the wrong file paths, breaking cross-seed's client-torrent search.** `torrents/files` prefixed every hoard file with the parent directory's name, so a single-file torrent came back as `Torr9/movie.mkv` while its `save_path` already ended in `Torr9`. Any client rebuilding `save_path + files[i].name` — cross-seed does — landed on `.../Torr9/Torr9/movie.mkv` and failed to link. Since single-file torrents are the common case in a film and episode library, effectively every searchee coming from the client list failed. `torrents/info`, `torrents/properties` and `torrents/files` now agree on plain BEP-3 semantics: `save_path` is the directory holding the content root, `name` is `info.name`, and file names carry the release directory only for multi-file torrents.
- **`torrents/files` returned a single made-up entry for multi-file torrents.** It never read the engine's file list, so a multi-file torrent was reported as one file named after its own directory (`Release/Release`). It now returns the real files, each with its real size.
- **A seed-mode add of a multi-file torrent pointed the engine one directory too high.** The save path had a level stripped to undo a directory join that a seed-mode add never performs, so the engine looked for the data in a directory that does not exist. The level is now only stripped when the save path really is the content root.

## v3.37.1 — 2026-08-03

### Fixed
- **Race policy fields no longer snap back after editing.** Because race settings are restart-required, the status poll kept reporting the old values and overwrote a just-made change (most visibly the AND/OR selector, which reverted instantly). Edited fields are now held until the page is reloaded.

## v3.37.0 — 2026-08-03

### Added
- **Category links for graduation (race \u2192 hoard).** A race category can now name a target hoard category. It is set in the category editor (shown only for race categories, listing hoard categories). This is the routing foundation for graduation: a graduating torrent will move to its linked hoard category's storage and label. The move itself lands in a following release.

## v3.36.0 — 2026-08-03

### Added
- **Race auto-eviction by age and/or ratio.** A second drain trigger, independent of disk pressure: delete race torrents older than N hours and/or above a ratio, combined with an AND/OR selector. Off by default (both thresholds 0); the min-age floor still protects torrents mid-race. Surfaced on the race policy bar.
- **API admission guard against a full race NVMe.** New race adds (native and the qBittorrent shim) are checked against the disk: when it is at/over the high watermark (or below a configurable free-space reserve), an emergency drain runs first, and only if it still cannot make room is the add rejected with 507. On by default; reserve 0 = act on the watermark alone.

## v3.35.0 — 2026-08-03

### Added
- **Race drain policy panel, above the Race list.** The race auto-drain used to be tailored, opaque, and buried in `[race_drain]` TOML. A compact bar now shows the NVMe usage gauge (with the low/high watermark marks), the Auto-drain toggle, the drain-to/from thresholds, the min-age floor and check interval, a Drain-now button, and a foldable drain history. Editing a value persists it and offers Apply & restart.

## v3.34.0 — 2026-08-03

### Added
- **Uncategorized and Untagged filter pills in the Hoard view.** Two meta-filters let you list torrents that have no category or no tag — handy for triaging what is left after a category or tag is removed. They appear only when categories/tags are actually in use and something lacks one.

## v3.33.4 — 2026-08-03

### Fixed
- **Deleting a category now clears it from its torrents.** Removing a category from the categories menu deleted the category but left the torrents that used it pointing at a dead label. They are now set back to uncategorized (no file move), matching qBittorrent.

## v3.33.3 — 2026-08-03

### Fixed
- **The Speedtest total-throughput labels are now English.** The cards added in 3.33.0 shipped with French labels (test only / total link); corrected for consistency with the rest of the tab.

## v3.33.2 — 2026-08-03

### Fixed
- **The Benchmark tab is now fully English.** A few labels and one error message had been left in French (the A/B comparison title, its middle-point label and Compare button, the speedtest last-test label, a validation message, and the iperf3 config error); they are translated so the tab reads in one language.

## v3.33.1 — 2026-08-02

### Fixed
- **The torrent list no longer shows a 0.00 ratio on our own uploads.** The list kept the engine's raw upload/download ratio (0 when nothing was downloaded) for any seeding torrent that wasn't refreshed by a live stats update, while the detail panel already showed the correct figure. The ratio is now computed against the data held at ingest and rendered the same way as the detail panel, so both agree.

## v3.33.0 — 2026-08-02

### Added
- **The speedtest now reports real link throughput, not just spare capacity.** The periodic speedtest shares the WAN with torrent seeding, so it only ever measured the bandwidth left over after seeding — understating the link. Each run now samples the concurrent engine throughput over exactly its own window and stores it, and the panel shows both the raw test figures and a "total link" line (test + concurrent seeding) per direction.

### Changed
- **"VPN Speedtest" is now just "Speedtest".** The label predated the move to a direct (relay-less) connection and was misleading.

## v3.32.6 — 2026-08-02

### Fixed
- **The torrent detail panel now shows the same ratio as the list.** For a torrent we uploaded but never downloaded, the engine's raw upload/download ratio is 0 (division by a zero download). The list already measures upload against the data we actually hold; the detail panel now does the same, so our own uploads display a meaningful ratio instead of a flat 0.

## v3.32.5 — 2026-08-02

### Fixed
- **The health dot no longer latches red in direct-connection mode.** The exit-IP leak detector flagged the home WAN IP as a VPN leak — correct under the old relay setup, wrong now that the node connects directly by design. It no longer forces the dot red (nor adds a LEAK row) on the home IP; the dot again reflects listen / port-forward health only.

## v3.32.4 — 2026-08-02

### Security
- **The ntfy alert topic is now opt-in via the `HYDRA_NTFY_TOPIC` env var, with no built-in default.** Health/watchdog push alerts are disabled unless the operator sets the topic explicitly, so a stock deployment never posts to a shared or third-party topic.

## v3.32.3 — 2026-08-02

### Fixed
- **Settings dropdowns now render dark natively instead of a washed-out light popup.** Custom `<option>` colors are ignored by Firefox-based browsers in the native popup, which left the open list light with an inconsistent selected/hover highlight. Switched to `color-scheme: dark` on the root and dropped the custom option overrides so the browser draws a coherent dark menu.

## v3.32.2 — 2026-08-02

### Fixed
- **The speed unit toggle (bytes/s ↔ bits/s) now applies to the overview and header readouts.** The Race / Hoard / Total upload & download figures were hard-coded to bits (Gbps) and ignored the *Display units → Speeds* setting. They now honor it, like the detail panels already did.

## v3.32.1 — 2026-08-01

### Performance
- **Zero-copy serving is now restricted to non-ZFS backends, so ZFS-backed torrents keep a warm ARC.** On ZFS-on-Linux, `sendfile(2)` reads go through the Linux page cache and bypass the ARC, so serving ZFS-backed pieces zero-copy pushed the hot working set into the kernel's plain LRU and starved the ARC (ZFS's compressed, scan-resistant cache with prefetch). ZFS datasets now serve through the buffered read path so blocks flow through the ARC; the zero-copy fast path is kept for non-ZFS storage (e.g. NVMe/XFS) where the workload is CPU-bound and the ARC is not involved. Detected automatically per torrent — no configuration.

## v3.32.0 — 2026-08-01

### Changed
- **Multi-seeding a torrent from both engines is now first-class.** Dropped the race/hoard anti-dual-announce gate and its announce-offset handoff — they only worked around one tracker's per-user upload crediting, and seeding the same torrent from race and hoard is perfectly legitimate. Both engines announce and seed independently.
- **Dropped the dual-family "secondary announce".** It fired a second announce with an XORed peer_id to register an extra peer, which showed up as duplicate rows in tracker peer lists. qBittorrent covers v4/v6 with a single peer_id — so do we now.

### Fixed
- **Peers can no longer show above 100% progress.** A duplicate HAVE kept incrementing a peer's piece count past the total; each piece is now counted once.

### Internal
- **Self-connection avoidance is dynamic.** Outbound dials abort at the handshake on a matching peer_id, and the self-IP filter is refreshed at runtime instead of a hard-coded list.

## v3.31.0 — 2026-08-01

### Performance
- **Seeding serves page-cache-resident pieces zero-copy, cutting serve CPU dramatically.** When a requested block is already in the OS page cache, Hydra now hands it straight from the cache to the peer's socket with `sendfile(2)`, skipping the three in-memory copies (disk cache → buffer → wire codec → kernel) the previous path paid on every 16 KiB block. On a fully resident working set this sustained ~30 Gbit/s at roughly a third of the CPU the buffered path used — the buffered path capped near 4 Gbit/s while pinning several cores. Blocks that would need a disk read are detected up front with a non-blocking `RWF_NOWAIT` probe and fall back to the buffered, thread-pool-offloaded read, so random reads from spinning storage keep their full disk concurrency and are never throttled by the zero-copy path. Only plaintext TCP peers take the fast path; encrypted (MSE) and uTP peers are unchanged.

## v3.30.6 — 2026-08-01

### Fixed
- **The health dot no longer sticks red after a transient hiccup.** The dot reflects port-forward / listen health (updated every 60s). A separate health poll every few seconds turned it red on any momentary /health failure but never turned it back, so on a busy server it stayed red even though connectivity was fine. That poll no longer touches the dot; the port-forward check is the sole owner.

## v3.30.5 — 2026-08-01

### Fixed
- **Tracker rows keep a fixed height regardless of error length.** A long last-error message wrapped onto several lines, so the row (and the whole table) grew and shrank as errors came and went. The error cell now stays on a single line, truncated with an ellipsis; the full text is still available on hover.

## v3.30.4 — 2026-08-01

### Fixed
- **The Trackers page no longer flickers on refresh.** The tracker table, the per-tracker stats table and its chart selector were rebuilt from scratch on every poll, so the whole page (notably the last-error cells) flickered a few times a second. Each is now re-rendered only when its data actually changed, and the chart reloads only when a tracker is (re)selected rather than on every poll.

## v3.30.3 — 2026-08-01

### Fixed
- **The records aggregate never blocks a request.** Computing the all-time records is a ~90s full scan on a large bench database. Previously the first request after the cache expired paid that cost (and any concurrent record requests queued behind it). GetRecords now serves the cached value immediately and refreshes in the background on the read-only connection (serve-stale-while-revalidate), the cache is warmed at startup, and its lifetime was raised to 30 minutes. The Benchmark tab stays responsive; records simply update a moment later.

## v3.30.2 — 2026-08-01

### Fixed
- **Benchmark, timeline and tracker endpoints no longer hang under a large bench database.** GetRecords computed the all-time records by running several unbounded full scans of the bench_samples table (over a million rows) while holding the single benchmark-DB mutex, taking a minute or more per call. Because every benchmark read and the 5-second sampler share that mutex, a few concurrent record refreshes starved everything else and requests piled up. The main connection now runs in WAL mode, the records aggregate is computed on a dedicated read-only connection so it never holds the write mutex, and the result is cached for a few minutes so repeated polls are instant.

## v3.30.1 — 2026-08-01

### Fixed
- **Per-tracker cumulative totals no longer drop when a torrent is removed.** The tracker stats summed only the torrents currently present, so deleting a torrent made its tracker's lifetime up/down shrink. Removing a torrent now folds its carried UL/DL into a persistent per-(engine, tracker) baseline (same mechanism as the global baseline, saved to baseline_trackers.json), so the cumulative is monotone across deletions and restarts. Trackers with no live torrents left still show their carried-over total.

## v3.30.0 — 2026-08-01

### Added
- **Per-tracker stats in the Trackers tab.** Hydra now records an upload/download rollup per tracker (split by engine) at each benchmark tick and shows it on the Trackers tab: a table with the hoard and race parts side by side — current up/down rate, peers, active/total torrents, lifetime up/down totals and ratio — plus a per-tracker time-series chart with separate hoard and race upload lines. Backed by a new `tracker_samples` table in bench.db; the rollup folds each engine's cached stats under a read lock with no full-list copy, so it stays cheap at 100k+ torrents.

## v3.29.3 — 2026-08-01

### Fixed
- **Tracker column now shows in the race list.** The Tracker column (added in v3.24.0) stayed empty for race torrents. The race list is served over REST from `torrentStatsToMap`, which builds each row field-by-field and never copied the `tracker_host` value — unlike the hoard list, which is streamed over SSE by marshaling the stats struct directly (so it always had it). Added `tracker_host` to the map builder; the race Tracker column and its filter now populate.

## v3.29.2 — 2026-08-01

### Fixed
- **Race timeline graphs and peers now render on networks without internet access.** Chart.js was loaded from an external CDN (jsdelivr), so on any network where the browser could not reach it — corporate wifi, DPI, LAN-only setups — the charting library was missing and the race timeline showed no graphs. Worse, the failed graph render threw before the peers list and event log were drawn, so those disappeared too. Chart.js is now vendored and served locally from the app's embedded assets, and each timeline section renders independently so a failure in one can never blank out the others.

## v3.29.1 — 2026-08-01

### Fixed
- **Recheck now works on seed-mode torrents.** Rechecking a torrent that was added in seed mode (skip-checking) used to fail with "cannot recheck a seed_mode torrent" because such torrents keep no piece picker. Recheck now builds a picker on demand and hash-checks the data on disk; if pieces are missing or corrupt the torrent switches to downloading to refetch them, otherwise it stays seeding. Trusted seed-mode adds still skip the check at add time, so there is no memory cost at scale — only a rechecked torrent allocates a picker.

## v3.29.0 — 2026-08-01

### Added
- **Drag-and-drop column reordering.** Grab any column header in the Hoard or Race list and drop it to change the column order; the layout is remembered per table (and per browser). The tables now render from a column registry, so this shares the same machinery as the right-click "Columns" show/hide menu, which is now keyed by column identity (surviving reordering) instead of position.

## v3.28.0 — 2026-08-01

### Added
- **Tags in the Hoard UI (phase 3).** The Hoard list shows a Tags column and a row of tag filter chips (between the tracker and category chips) to narrow the list to one tag. Right-clicking a torrent (or a multi-selection) offers "Edit tags", a submenu that toggles existing tags on/off and adds new ones, applied to every selected hoard torrent. This completes the tag feature (backend + qBittorrent parity + UI).

## v3.27.0 — 2026-08-01

### Added
- **qBittorrent tag parity (phase 2).** The qBittorrent-compatible API now exposes real torrent tags instead of a placeholder: `torrents/info` reports each torrent's actual tags (and accepts a `tag` filter), `torrents/add` honours the `tags` field, and the standard tag endpoints are implemented — `torrents/tags` (list), `createTags`, `deleteTags`, `addTags`, `removeTags`. This lets autobrr, the *arr apps and other qBittorrent clients read and manage Hydra tags the same way they would a real qBittorrent. (Tagging currently targets hoard torrents.)

### Changed
- The qBittorrent `tags` field previously returned a fixed `hydra:<engine>` placeholder; it now returns the torrent's real tags.

## v3.26.0 — 2026-08-01

### Added
- **Torrent tags (backend, phase 1).** Hoard torrents can now carry multiple qBittorrent-style tags in addition to their single category. Tags are set via `POST /api/hoard/torrents/<hash>/tags` (`op` = set/add/remove) and listed via `GET /api/tags`; they show up in the torrent stats and survive restart (persisted to a `tags.json` overlay). The qBittorrent shim parity and the UI land in follow-up releases.

## v3.25.0 — 2026-08-01

### Added
- **Recheck in the torrent context menu.** Right-clicking a hoard torrent (or a multi-selection) now offers Recheck, which hash-checks the torrent's data on disk and resumes from the verified state. It works for hoard torrents whether they run locally or on a remote agent, and the item is hidden when the selection contains no hoard torrents.

## v3.24.0 — 2026-08-01

### Added
- **Tracker column in the torrent lists.** The Hoard and Race lists now show each torrent's tracker (the announce host from its .torrent), so you can see at a glance which tracker a torrent belongs to and sort by it. In Hoard the column sits just before Category; in Race, before Added. The value is the static tracker of the torrent, so it is shown for every torrent regardless of announce state, with no extra per-torrent lookup.

- **Tracker filter chips in Hoard.** A row of tracker pills (one per tracker, with counts) sits between the status and category filters; click one to show only that tracker's torrents. Combines with the status and category filters.

## v3.23.0 — 2026-08-01

### Added
- **Trackers tab aggregates every node's trackers.** The tab now merges the local announce registry with each connected agent's, so a front-only controller (which runs no engine of its own) can see and manage the whole fleet's trackers instead of an empty list. One row per host with summed torrent counts; each row lists which nodes announce to that tracker, and setting a spoof or passkey still applies globally across the fleet.

## v3.22.0 — 2026-08-01

### Added
- **Tracker overrides now propagate to remote agents.** Setting a client spoof or announce passkey on the Trackers tab is pushed to every connected agent, so a global override stays consistent across a multi-node fleet instead of only affecting the front node. The API response reports how many agents received the push. Each agent still persists its own copy and re-seeds from its config on restart.

## v3.21.0 — 2026-08-01

### Added
- **Trackers tab.** A new top-level tab lists every tracker you are actively announcing to (the hot set of announcing torrents), showing the number of torrents per tracker, the last-announce status, and any tracker error. From each row you can override the announced client identity — the peer_id prefix and User-Agent — to pass a tracker's client whitelist (one-click presets for qBittorrent, Transmission and Deluge), or override the announce passkey. Overrides apply on the next announce with no restart, surfacing in the UI what previously meant editing `default.toml` by hand.

- **`reset-password` command for locked-out admins.** `hydra reset-password <new>` hashes the new password and writes it straight into `[auth] password_hash`, so recovering a lost admin login is a single command (`docker exec <container> hydra reset-password <new>`, then restart) instead of hand-editing TOML.

### Fixed
- **Startup banner no longer points to a nonexistent credentials file.** Once the admin login is configured, the banner claimed the credentials were "already configured (admin-credentials.txt)" — but that file only ever held the first-run temporary password and may be long gone. It now tells you how to reset a lost password instead.

## v3.20.1 — 2026-08-01

### Fixed
- **qBittorrent import now replicates the source layout exactly.** The importer derives each torrent's real content directory from qBit's `content_path` (stat distinguishes multi-file folders from loose single files), so files are found where qBit actually stored them and the per-torrent content-folder flag is recorded correctly. Fixes a regression in v3.20.0 where multi-file and subfolder single-file imports pointed at the wrong path; loose single-file (no-subfolder) imports also import cleanly now.

## v3.20.0 — 2026-08-01

### Added
- **Content layout option (`create_torrent_folder`).** Single-file torrents can now be saved qBittorrent-style — directly in the category/save folder instead of always being wrapped in a per-name subfolder. The `[daemon] create_torrent_folder` toggle (Config tab → General) controls it, **off by default** to match qBittorrent. Multi-file torrents are unaffected (they always keep their own folder). The choice is recorded per-torrent, so it only affects newly added torrents; existing torrents and their paths are untouched, cross-seed export presents loose single files correctly, and category moves relocate the bare file instead of a folder.

### Changed
- Existing installs keep their current behavior: `create_torrent_folder` was already `true` in their config, so nothing changes unless they turn it off.

## v3.19.2 — 2026-07-31

### Fixed
- **Logs tab no longer stays stuck on "Loading..." after a refresh.** Restoring the active tab from the URL hash ran at script-parse time and invoked the Logs loader before its module state was initialized, throwing a TDZ ReferenceError that silently aborted the initial fetch. Tab restoration is now deferred to DOMContentLoaded, so a hard refresh while on the Logs tab loads the backlog as expected (switching tabs already worked).
- **Logs filter selectors sit on one row again.** The global full-width form-control style was stretching each Logs filter dropdown to 100%, forcing them to wrap onto separate lines; they now size to their content and line up horizontally. The Live checkbox is no longer stretched full-width either (same global-style leak, forced back to its natural size).

## v3.19.1 — 2026-07-31

### Fixed
- **Logs tab now uses the full window width.** It was capped at 600px by the shared add-form container; the tab renders edge-to-edge so long log lines no longer wrap prematurely.

## v3.19.0 — 2026-07-31

### Added
- **Logs tab.** A new UI tab backed by the in-process log hub: filter by source, level, time window and free text, live-tail over SSE, and copy/export the current view or open a pre-filled issue from a selection.

## v3.18.3 — 2026-07-31

### Changed
- **Detached banner tuning.** The detached (headless) default is now the 80-wide logo, the version line is centered, and the rule/summary are sized to the logo width for a tidy header in log viewers.

## v3.18.2 — 2026-07-31

### Changed
- **Banner logo margins tightened.** Side margins on the logo are tight-cropped and the detached default steps up to the 100-wide logo.

## v3.18.1 — 2026-07-31

### Changed
- **Full-size logo when detached.** When no console width is reported (w==0, e.g. running detached), the banner prints the full-size logo, since those logs are typically read in wide web viewers.

## v3.18.0 — 2026-07-31

### Changed
- **Adaptive banner ladder.** The startup logo picks between 80/100/120-wide variants based on terminal width, and reads CONOUT$ so the width is detected even when stdout is redirected.

## v3.17.6 — 2026-07-31

### Changed
- **Exit IP pill cursor.** Uses a standard pointer cursor on the Exit IP pill instead of the custom refresh cursor.

## v3.17.5 — 2026-07-31

### Changed
- **Exit IP is now a hover pill.** The Exit IP block became a pill with an always-visible refresh icon and a pointer cursor on hover.

## v3.17.4 — 2026-07-31

### Added
- **Exit IP refresh button.** A refresh button in the Exit IP label, revealed on hover, that spins while a refresh is in flight.

## v3.17.3 — 2026-07-31

### Changed
- **Exit-IP refresh button moved beside the value** in the header, and a manual refresh now plays a brief slot-machine scramble animation on the IP text until the new value lands. The 2-minute background poll still updates silently (no flicker).

## v3.17.2 — 2026-07-31

### Added
- **Manual "refresh IP now" button** next to the header exit IP. It forces a fresh lookup (`/api/public-ip?refresh=1`) that bypasses the cache — handy right after switching a VPN server.

### Changed
- **Public-IP freshness.** Backend cache lowered from 5 min to 90 s and the UI poll from 5 min to 2 min. With the cache TTL now shorter than the poll interval, the old aliasing that could leave the shown IP stale for up to ~10 minutes is gone (worst case ~2 min).

## v3.17.1 — 2026-07-31

### Added
- **Platform-aware update notifications.** Releases now carry a `Platforms:` label (derived in CI from `[windows]`/`[linux]` commit markers; no marker = both), and the in-app update check only notifies when the running OS is targeted — a Windows-only fix no longer nags Linux users, and vice-versa. Releases without a label are treated as affecting all platforms.

## v3.17.0 — 2026-07-31

### Added
- **Adaptive startup banner.** Hydra detects the console width and prints a detailed hydra logo on wide terminals, a compact one on standard 80-column consoles, and a plain wordmark when very narrow — so the banner never wraps into garbage.

## v3.16.9 — 2026-07-31

### Changed
- Startup banner: refined the hydra logo eye placement.

## v3.16.8 — 2026-07-31

### Changed
- Startup banner: reptilian slit eyes on the hydra heads.

## v3.16.7 — 2026-07-31

### Changed
- Startup banner: added eyes to the hydra logo.

## v3.16.6 — 2026-07-31

### Added
- **Real hydra logo in the startup banner**, rendered as shaded ASCII from the project emblem (replaces the placeholder wordmark).

## v3.16.5 — 2026-07-31

### Added
- **In-process log hub + clean startup console.** All logs (Go, HTTP, and the Typhon engine's stdout/stderr) now funnel into a bounded ring buffer plus a `hydra.log` file next to the config, instead of flooding the console. The console shows only a human startup banner; the generated admin password is printed there in a readable box and saved to `admin-credentials.txt` — and never written to the log stream. High-frequency poll endpoints are excluded from HTTP request logging.

## v3.16.4 — 2026-07-31

### Fixed
- **Windows: the engine watchdog falsely reported the engine dead every ~30 s** and restarted in a loop. Its liveness check read `/proc/<pid>/stat`, which does not exist on Windows; it now uses `OpenProcess` + `GetExitCodeProcess`. (The per-engine RSS ceiling stays Linux-only.)

## v3.16.3 — 2026-07-31

### Fixed
- **Windows: the DHT crashed immediately with `os error 10054` (WSAECONNRESET)**, taking the engine into a restart loop. The DHT's UDP socket — created directly by `librqbit-dht`, bypassing the dual-stack helper patched in 3.16.1 — now sets `SIO_UDP_CONNRESET` on Windows and tolerates connection-reset errors in its read loop. Linux unaffected.

## v3.16.2 — 2026-07-31

### Fixed
- **Docker image build (docker workflow / GHCR) and local prod rebuilds.** The vendored `librqbit-dualstack-sockets` crate is referenced via a `[patch]` path `../third_party/...`, but the Dockerfile's Typhon build stage only copied `typhon-engine/`, so the patch path didn't resolve and the image build failed. The Dockerfile now copies `third_party/`. Release tarballs/zip were unaffected.

## v3.16.1 — 2026-07-31

### Fixed
- **Windows release build.** The vendored `librqbit-dualstack-sockets` crate's SIO_UDP_CONNRESET fix calls `WSAIoctl`, which windows-sys 0.59 gates behind the `Win32_System_IO` feature (its signature references `OVERLAPPED`). That feature was missing so `WSAIoctl` was compiled out and the Windows build failed. Added it. Linux unaffected.

## v3.16.0 — 2026-07-31

### Added
- **Per-disk seed-slot regulation (HDD quiet mode).** Advanced, opt-in via `[hoard.disk_slots]` (off by default). Bounds how many torrents actively serve pieces from each disk at once so a spinning drive stays quiet (fewer concurrent seeks/noise). Over the cap, the least-critical seeders (many sources + slow upload; rare torrents protected) are *serving-suspended* — force-choked so they do zero disk I/O while staying connected and announcing (seedtime preserved, instant resume), not paused. A waiting queue resumes the most-demanded torrent when the disk frees up, with cooldown/hysteresis/warm-up to avoid flapping. Configured per drive letter (Windows). Linux path-prefix groups and a per-disk read elevator are planned follow-ups.

## v3.15.3 — 2026-07-31

### Changed
- **qBit import now logs a failure breakdown.** On completion it emits a bucketed summary of why torrents failed (e.g. `export: timeout x280 | export: http 404 x15 | add: ...`) instead of only a bare `failed=N` count, so a slow or lossy import can be diagnosed from the logs without inspecting each torrent.

## v3.15.2 — 2026-07-30

### Fixed
- **Header exit-IP briefly showed the home WAN IP at launch.** The shared SOCKS5 exit dialer is now installed before anything can call the public-IP lookup, so the first lookup goes through the proxy instead of racing it and caching the direct (home) IP for 5 minutes. Also refreshed the front-end leak-detection list with the current home WAN IP so a real leak is flagged.

## v3.15.1 — 2026-07-30

### Changed
- **Windows: the auto-generated config binds the web UI to `127.0.0.1`** (localhost only) instead of all interfaces, so a desktop install is not exposed to the LAN out of the box. Set `api_host = "0.0.0.0"` in the generated `default.toml` if you want to reach the UI from other machines. Linux defaults are unchanged.

## v3.15.0 — 2026-07-30

### Added
- **Zero-setup start.** Run `hydra` with no `--config` and it finds a `default.toml` next to the executable (or the working directory); if none exists it writes a fresh one there and starts. A relative or empty `data_dir` now resolves next to the executable, so an unzipped build runs from anywhere and keeps its data beside it. On Windows this means: unzip, run `hydra.exe`, done — no config editing, no `--config`. Docker/systemd installs pass `--config` explicitly and are unaffected.

## v3.14.0 — 2026-07-30

### Added
- **Windows support.** Hydra now builds and runs natively on Windows (`hydra.exe` + `hydra-engine.exe`), published as a zip on each release alongside the Linux tarballs. On Windows the daemon and the Typhon engine talk over a TCP loopback socket (the Unix domain socket stays the default on Linux). VPN routing is delegated to the system VPN client — the Linux fwmark/SO_MARK path is disabled on Windows. jemalloc heap profiling stays Linux-only; Windows uses the system allocator.

## v3.13.9 — 2026-07-30

### Fixed
- **Trackers rejecting announces** ("invalid peer_id length: 21"). Since v3.11.0 the peer_id prefix was 9 bytes instead of 8 (the version encoding overflowed when the minor version hit two digits), producing a 21-byte peer_id that strict trackers reject. The prefix is now 8 bytes again, with a guard so it can never regress. Anyone on v3.11.0-v3.13.8 should update.

## v3.13.8 — 2026-07-30

### Changed
- Incognito now also masks save paths (Categories tab and torrent detail) with a fake path, so real folders/usernames don't leak in screenshots. Form fields keep the real path for editing.

## v3.13.7 — 2026-07-30

### Changed
- Incognito now also masks peer IPs in the torrent detail peers tables and the race progress graph legend.

## v3.13.6 — 2026-07-30

### Changed
- In incognito, the exit IP (header + agents) is now shown as an obviously-redacted mask instead of a realistic fake IP, so it cannot be mistaken for a real address. Peer IPs stay distinct placeholders.

## v3.13.5 — 2026-07-30

### Changed
- Incognito now also masks category names in the Categories tab, the category filter chips and category dropdowns (display only; editing/filtering still use the real names).

## v3.13.4 — 2026-07-30

### Fixed
- Removed the border box around the Incognito header icon (now a plain white icon).

## v3.13.3 — 2026-07-30

### Fixed
- Incognito header icon is now white so it is visible on the dark header.

## v3.13.2 — 2026-07-30

### Fixed
- The index page is served with `Cache-Control: no-cache` so UI updates show up without a manual hard-refresh. Made the Incognito header button clearly visible (bordered icon).

## v3.13.1 — 2026-07-30

### Changed
- Moved the Incognito toggle out of the tab bar into a small icon button in the header (next to the health dot), so it no longer looks like a navigation tab.

## v3.13.0 — 2026-07-30

### Changed
- Saving settings no longer always demands a full restart. Changes are now tiered: the peer **listen port** is applied **live** (no restart at all); engine knobs and daemon/auth settings show an accurate "restart" prompt instead of a blanket one. (Live engine-only restart for engine knobs is coming next.)

## v3.12.0 — 2026-07-30

### Added
- **Incognito mode** — a toggle next to "Add Torrent" that anonymizes the UI for screenshots and screen-sharing: torrent names become Linux ISOs, categories become distro names, and every IP (peers, tunnels, exit IP, agents) is replaced with a reserved documentation IP. Display-only and deterministic (stable labels); your real data, filters and actions are untouched. Remembered in the browser.

## v3.11.3 — 2026-07-30

### Changed
- Reworked which settings are shown by default vs behind "Show advanced" in the Configuration tab, aligned with qBittorrent's everyday options: per-engine listen port, bind interface, connection limits, upload rate and queueing (active downloads/seeds/torrents) plus WebUI/auth are now front-and-center; proxy/SOCKS, PROXY-v2, choking internals, timeouts and tuning knobs move to advanced. Also dropped stale references to removed settings.

## v3.11.2 — 2026-07-30

### Added
- The `bind_interface` setting in the Configuration tab is now a **dropdown of detected interfaces** (name — IP), not a free-text field.

### Fixed
- Existing installs now automatically gain newly-added config keys (currently `bind_interface`/`listen_interfaces`) on the next start — they are appended to `default.toml` additively (existing lines untouched) so the options appear in the Configuration editor without a manual edit.

## v3.11.1 — 2026-07-30

### Fixed
- `bind_interface` and `listen_interfaces` now ship (empty) in the default `[race]`/`[hoard]` config, so they show up as editable fields in the Configuration tab instead of being invisible until hand-added.

## v3.11.0 — 2026-07-30

### Added
- **Exit IP in the header** — the daemon's public egress IP is shown top-right next to the health dot, so you can confirm at a glance that traffic leaves through your VPN.
- **Agents show their exit IP and interfaces** — the Agents tab has an Exit IP column (each agent reports its own egress, i.e. its own tunnel), and hovering it lists that agent's network interfaces. Backed by a new node-level `node_info` agent call.

## v3.10.0 — 2026-07-30

### Added
- **Interface binding made easy**: the Configuration tab now lists the host's network interfaces (name + IP), and a new `bind_interface` setting under `[race]`/`[hoard]` pins an engine to an interface by **name** (e.g. `wg0`). The name is resolved to its current IP at engine start, so it keeps working across VPN reconnects where the tunnel IP rotates. Explicit `listen_interfaces` (IP-based) still wins when set.

## v3.9.1 — 2026-07-30

### Fixed
- The **Configuration** tab no longer returns a 500 when the config file lives outside `data_dir` (e.g. the bare-metal install: config in `/etc/hydra`, data in `/var/lib/hydra`). The settings editor now reads and writes the actual `--config` file the daemon loaded instead of assuming `<data_dir>/default.toml`.

## v3.9.0 — 2026-07-30

### Changed
- **qBittorrent import is now pipelined** — a small pool of fetchers pulls `.torrent` files from qBittorrent while a larger pool adds them to the engine concurrently, instead of one torrent at a time. Large libraries import several times faster; the fetch concurrency is kept low so qBittorrent's WebUI stays responsive.

### Fixed
- The import now carries over the **all-time upload AND download** totals into Hydra's baseline, so the overview ratio/totals reflect your migrated history (previously only a cosmetic line; download was not read at all).
- Imported torrents keep their **original add date** from qBittorrent instead of showing the moment of import.

## v3.8.0 — 2026-07-30

### Added
- **Bare-metal install**: tagged releases now ship a self-contained Linux tarball (`hydra-vX.Y.Z-linux-{amd64,arm64}.tar.gz`) with the `hydra` and `hydra-engine` binaries, a sample config, a systemd unit, and an `install.sh`. `sudo ./install.sh` drops everything under `/opt/hydra` + `/etc/hydra` + `/var/lib/hydra`, creates a `hydra` user, and enables the service (starts on boot, restarts on failure). No Docker required.
- The engine binary path is now resolved via `HYDRA_ENGINE_BIN`, then next to the `hydra` binary, then the Docker default — so the two binaries can live anywhere together.

### Changed
- Web UI assets (templates + static + changelog) are now **embedded in the binary**. Hydra no longer depends on a `web/` directory next to its working directory, and the changelog is served from `/changelog.md`.

## v3.7.0 — 2026-07-30

### Added
- **Agent listen-port hook** (`--listen-port-hook <port>`, opt-in): an `--agent-only` node can now serve a loopback-only (`127.0.0.1`) `POST /listen-port` endpoint so a gluetun sidecar sharing its network namespace can push the VPN's forwarded port via a plain `wget` — the piece that was missing to run the one-agent-per-engine-behind-its-own-VPN topology. Bound to loopback in hard code (never reachable off the shared netns or over the tunnel); honors `--agent-token` via the `X-API-Key` header when set.

## v3.6.3 — 2026-07-30

### Fixed
- A freshly-added torrent now shows its **category** immediately instead of appearing under "none" until the next stats refresh (the category was stored correctly but not projected into the live list on add / recheck-to-seeding).

## v3.6.2 — 2026-07-30

### Added
- Torrent **progress** now streams live in the list — no refresh needed.

### Fixed
- Changing a torrent's **category** updates the row immediately instead of only after a refresh.
- The qBittorrent import counts already-present torrents as **skipped** rather than failed.
- The update-availability check no longer polls every second (throttled on the client; the server already caches it).
- The Add form no longer breaks when no engine mode is active (falls back to race).

## v3.6.1 — 2026-07-30

### Added
- **Import** is now a sub-tab of the Config tab — re-run the qBittorrent import any time, not just at the one-time first-run prompt.

### Fixed
- The qBittorrent import wizard "Skip" button is now styled (was an unstyled white button).

## v3.6.0 — 2026-07-29

### Added
- **Choose visible columns** — right-click a table header (Race or Hoard) to show/hide columns; remembered per table.
- **Changelog tab** — this page, rendered in-app from the repository CHANGELOG.
- **Update badge** — a small badge appears next to the version when a newer release exists. Checked server-side against GitHub (cached 6h, no browser CORS); opt out with .

## v3.5.0 — 2026-07-29

### Added
- **Column visibility** — right-click a table header (Race or Hoard) to pick which columns to show. Remembered per table.
- **Display units** — choose size units (binary `MiB` vs decimal `MB`) and speed units (bytes `MB/s` vs bits `Mbps`) in the Config tab; applied to every size/speed readout.
- **Change category without moving files** — right-clicking a hoard torrent now offers both `Change category` (re-tag only) and `Change category + move files`.

### Fixed
- The **Agents** tab now loads its data when you open it, not only when arriving via the `#agents` URL.

## v3.4.0 — 2026-07-29

### Added
- **Reworked Add form** — the category is the primary field and drives the mode + save path; mode/save-path move under an Advanced section. Clear confirmation on add.
- **Persistent table sort** — the column and direction you sort by are remembered across reloads, per table.
- **Config sub-tabs** — the settings menu is split into per-domain tabs instead of one long scroll.

### Fixed
- **No more re-downloading already-complete data** on a re-add: a recheck is no longer interrupted by the download slot manager (it used to pull 100–200 MB before finishing).
- **Torrent name shows immediately** in the list after adding — it previously appeared as the info-hash until a refresh (or two).
- **Category routing** — picking a category now routes to its engine role, instead of falling back to `race`.
- Fixed the category auto-fill in the Add form.

## v3.3.0 — 2026-07-29

### Added
- **Recheck** — adding a torrent whose data is already on disk now hash-checks it and seeds the valid pieces instead of blindly re-downloading over them. Missing/corrupt pieces are fetched; a complete torrent goes straight to seeding.
  - Available as the qBittorrent `POST /api/v2/torrents/recheck` and the native `POST /api/hoard/torrents/<hash>/verify` (hoard engine only).
  - `skip_checking=true` stays the trust-fast path for cross-seed / hardlinked data.
